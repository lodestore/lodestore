use std::collections::HashMap;
use std::time::Duration;

use lodestore_policy::jiff::{SignedDuration, Timestamp};
use lodestore_policy::schedule::CompiledSchedule;
use lodestore_policy::{BackupPolicy, Schedule};
use sha2::{Digest, Sha256};
use sqlx::Row;
use tokio_util::sync::CancellationToken;

use crate::db::now_ms;
use crate::jobs;
use crate::policies::matching::{AgentRow, load_agents};
use crate::state::AppState;

pub const SCHEDULE_TICK: Duration = Duration::from_secs(15);
pub const SCHEDULE_GRACE: Duration = Duration::from_secs(120);

const MISSED_CAP: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Schedule,
    Catchup,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Schedule => "schedule",
            Origin::Catchup => "catchup",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScheduledJob {
    pub job_id: String,
    pub policy: String,
    pub host_id: String,
    pub origin: Origin,
    pub created: bool,
}

#[derive(Debug, Default)]
pub struct TickReport {
    pub queued: Vec<ScheduledJob>,
}

pub fn jitter_offset(agent_id: &str, policy_name: &str, jitter: Duration) -> Duration {
    let jitter_ms = u64::try_from(jitter.as_millis()).unwrap_or(u64::MAX);
    if jitter_ms == 0 {
        return Duration::ZERO;
    }
    let digest = Sha256::new()
        .chain_update(agent_id.as_bytes())
        .chain_update([0u8])
        .chain_update(policy_name.as_bytes())
        .finalize();
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    Duration::from_millis(u64::from_be_bytes(head) % jitter_ms)
}

pub async fn run(state: AppState, stop: CancellationToken) -> anyhow::Result<()> {
    let period = Duration::from_secs(state.config.schedule_tick_secs.max(1));
    let mut interval = tokio::time::interval(period);
    loop {
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            _ = interval.tick() => {}
        }
        if let Err(error) = tick(&state, Timestamp::now()).await {
            tracing::warn!(%error, "schedule tick failed");
        }
    }
}

struct Cursor {
    fingerprint: String,
    last_slot_ms: i64,
}

struct Scheduled {
    id: String,
    repository_id: String,
    policy: BackupPolicy,
}

pub async fn tick(state: &AppState, now: Timestamp) -> anyhow::Result<TickReport> {
    let mut report = TickReport::default();
    let policies = scheduled_policies(state).await?;
    if policies.is_empty() {
        return Ok(report);
    }
    let agents = load_agents(state).await?;
    let mut cursors = load_cursors(state).await?;
    for entry in &policies {
        let Some(schedule) = &entry.policy.spec.schedule else {
            continue;
        };
        let compiled = match schedule.compile() {
            Ok(compiled) => compiled,
            Err(problem) => {
                tracing::warn!(policy = %entry.policy.metadata.name, ?problem, "stored schedule is invalid");
                continue;
            }
        };
        let fingerprint = fingerprint(schedule, &compiled);
        for agent in agents
            .iter()
            .filter(|a| a.applies(&entry.repository_id, &entry.policy.spec.selector))
        {
            let cursor = cursors.remove(&(entry.id.clone(), agent.id.clone()));
            let result = pair(
                state,
                now,
                entry,
                &compiled,
                &fingerprint,
                agent,
                cursor.as_ref(),
            )
            .await;
            match result {
                Ok(Some(job)) => report.queued.push(job),
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    policy = %entry.policy.metadata.name,
                    host = %agent.id,
                    %error,
                    "scheduling a backup failed"
                ),
            }
        }
    }
    Ok(report)
}

fn fingerprint(schedule: &Schedule, compiled: &CompiledSchedule) -> String {
    format!(
        "{}|{}|{}|{}",
        schedule.cron.trim(),
        compiled.timezone_name(),
        compiled.jitter().as_secs(),
        compiled.catch_up_window().as_secs()
    )
}

async fn pair(
    state: &AppState,
    now: Timestamp,
    entry: &Scheduled,
    compiled: &CompiledSchedule,
    fingerprint: &str,
    agent: &AgentRow,
    cursor: Option<&Cursor>,
) -> anyhow::Result<Option<ScheduledJob>> {
    let name = &entry.policy.metadata.name;
    let offset = jitter_offset(&agent.id, name, compiled.jitter());
    let Some(shifted) = now.checked_sub(SignedDuration::try_from(offset)?).ok() else {
        return Ok(None);
    };
    let Some(latest) = compiled.previous_slot(shifted) else {
        return Ok(None);
    };
    let latest_ms = latest.as_millisecond();
    let cursor = match cursor {
        Some(cursor) if cursor.fingerprint == fingerprint => cursor,
        _ => {
            store_cursor(state, &entry.id, &agent.id, fingerprint, latest_ms).await?;
            return Ok(None);
        }
    };
    if latest_ms <= cursor.last_slot_ms || !state.registry.is_online(&agent.id) {
        return Ok(None);
    }
    let late_ms = now.as_millisecond() - latest_ms - offset.as_millis() as i64;
    if late_ms > compiled.catch_up_window().as_millis() as i64 {
        store_cursor(state, &entry.id, &agent.id, fingerprint, latest_ms).await?;
        return Ok(None);
    }
    let missed = match Timestamp::from_millisecond(cursor.last_slot_ms) {
        Ok(after) => compiled.slots_between(after, latest, MISSED_CAP),
        Err(_) => 1,
    };
    let origin = if missed > 1 || late_ms > SCHEDULE_GRACE.as_millis() as i64 {
        Origin::Catchup
    } else {
        Origin::Schedule
    };
    let hosts = [(agent.id.clone(), agent.hostname.clone())];
    let spec = jobs::job_spec(&entry.policy);
    let queued =
        jobs::enqueue_with_origin(state, &entry.id, name, &hosts, &spec, origin.as_str()).await?;
    let Some(job) = queued.into_iter().next() else {
        return Ok(None);
    };
    if job.created {
        mark_origin(state, &job.id, origin, missed).await?;
    }
    store_cursor(state, &entry.id, &agent.id, fingerprint, latest_ms).await?;
    Ok(Some(ScheduledJob {
        job_id: job.id,
        policy: name.clone(),
        host_id: agent.id.clone(),
        origin,
        created: job.created,
    }))
}

async fn mark_origin(
    state: &AppState,
    job_id: &str,
    origin: Origin,
    missed: usize,
) -> anyhow::Result<()> {
    let mut tx = state.db.pool.begin().await?;
    let missed_runs = (origin == Origin::Catchup).then_some(missed as i64);
    sqlx::query("UPDATE jobs SET origin = $1, missed_runs = $2 WHERE id = $3")
        .bind(origin.as_str())
        .bind(missed_runs)
        .bind(job_id)
        .execute(&mut *tx)
        .await?;
    if origin == Origin::Catchup {
        let plural = if missed == 1 { "run" } else { "runs" };
        let logged = sqlx::query(
            "INSERT INTO job_events (job_id, seq, at, level, kind, message) \
             VALUES ($1, 0, $2, 'info', 'catch_up', $3) ON CONFLICT DO NOTHING",
        )
        .bind(job_id)
        .bind(now_ms())
        .bind(format!(
            "Running one catch-up backup for {missed} missed {plural}"
        ))
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if logged == 1 {
            sqlx::query("UPDATE jobs SET log_lines = log_lines + 1 WHERE id = $1")
                .bind(job_id)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

async fn store_cursor(
    state: &AppState,
    policy_id: &str,
    host_id: &str,
    fingerprint: &str,
    last_slot_ms: i64,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO schedule_state (policy_id, host_id, fingerprint, last_slot_ms, updated_at) \
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT (policy_id, host_id) DO UPDATE SET \
         fingerprint = excluded.fingerprint, last_slot_ms = excluded.last_slot_ms, \
         updated_at = excluded.updated_at",
    )
    .bind(policy_id)
    .bind(host_id)
    .bind(fingerprint)
    .bind(last_slot_ms)
    .bind(now_ms())
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

async fn load_cursors(state: &AppState) -> anyhow::Result<HashMap<(String, String), Cursor>> {
    let rows =
        sqlx::query("SELECT policy_id, host_id, fingerprint, last_slot_ms FROM schedule_state")
            .fetch_all(&state.db.pool)
            .await?;
    rows.iter()
        .map(|row| {
            Ok((
                (row.try_get("policy_id")?, row.try_get("host_id")?),
                Cursor {
                    fingerprint: row.try_get("fingerprint")?,
                    last_slot_ms: row.try_get("last_slot_ms")?,
                },
            ))
        })
        .collect()
}

async fn scheduled_policies(state: &AppState) -> anyhow::Result<Vec<Scheduled>> {
    let rows = sqlx::query("SELECT id, repository_id, spec_json FROM policies ORDER BY name")
        .fetch_all(&state.db.pool)
        .await?;
    let mut out = Vec::new();
    for row in &rows {
        let id: String = row.try_get("id")?;
        let policy: BackupPolicy =
            match serde_json::from_str(&row.try_get::<String, _>("spec_json")?) {
                Ok(policy) => policy,
                Err(error) => {
                    tracing::warn!(policy = %id, %error, "stored policy cannot be parsed");
                    continue;
                }
            };
        if policy.spec.schedule.is_some() {
            out.push(Scheduled {
                id,
                repository_id: row.try_get("repository_id")?,
                policy,
            });
        }
    }
    Ok(out)
}
