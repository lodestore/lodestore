pub mod live;
pub mod messages;

use std::collections::HashSet;
use std::time::Duration;

use lodestore_format::keys::{MasterKey, ReadKeys};
use lodestore_format::locks::{DEFAULT_MAX_RUNTIME, RUNTIME_MARGIN};
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{
    JobAssign, JobCancel, JobSpec, Limits, RestoreSpec, ServerMessage,
};
use prost::Message;
use sqlx::Row;
use tokio_util::sync::CancellationToken;

use crate::db::now_ms;
use crate::grpc::registry::{CAP_GC, CAP_LIMITS, CAP_RESTORE};
use crate::jobs::restore::RestoreRecord;
use crate::maintenance::gc::db as gc_db;
use crate::repos::db::server_access;
use crate::state::AppState;

const KIND_RESTORE: &str = "restore";
const LIMITS_IGNORED_SEQ: i64 = -1;
const LIMITS_IGNORED_MESSAGE: &str =
    "The agent on this host is too old to apply the policy limits. Update the agent.";
const TIME_LIMIT_MESSAGE: &str = "The backup ran longer than its time limit and was stopped.";
const TICK: Duration = Duration::from_secs(1);
const JANITOR_EVERY: Duration = Duration::from_secs(60);
const PROGRESS_FLUSH: Duration = Duration::from_secs(10);
const QUEUED_TTL_MS: i64 = 24 * 3600 * 1000;
const UNACCEPTED_TTL_MS: i64 = 2 * 60 * 1000;
const RUNNING_OFFLINE_TTL_MS: i64 = 24 * 3600 * 1000;
const LOG_TTL_MS: i64 = 30 * 24 * 3600 * 1000;
const JOB_TTL_MS: i64 = 90 * 24 * 3600 * 1000;

fn server_message(body: ServerBody) -> ServerMessage {
    ServerMessage { body: Some(body) }
}

pub async fn run(state: AppState, stop: CancellationToken) -> anyhow::Result<()> {
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_janitor = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            _ = tick.tick() => {}
        }
        if let Err(error) = assign_queued(&state, None).await {
            tracing::warn!(%error, "assigning queued jobs failed");
        }
        if let Err(error) = push_cancels(&state).await {
            tracing::warn!(%error, "sending job cancels failed");
        }
        flush_progress(&state).await;
        if last_janitor.elapsed() >= JANITOR_EVERY {
            last_janitor = tokio::time::Instant::now();
            if let Err(error) = janitor_once(&state, now_ms()).await {
                tracing::warn!(%error, "job janitor failed");
            }
        }
    }
}

struct GcGate {
    enabled: bool,
    /// A cleanup cycle holds its marker in the repository, or stopped while
    /// deleting and still does.
    unfinished: bool,
}

async fn gc_gate(state: &AppState, host_id: &str) -> anyhow::Result<GcGate> {
    let enabled = sqlx::query(
        "SELECT s.enabled FROM agents a JOIN gc_settings s ON s.repository_id = a.repository_id \
         WHERE a.id = $1",
    )
    .bind(host_id)
    .fetch_optional(&state.db.pool)
    .await?
    .map(|row| row.try_get::<i64, _>("enabled"))
    .transpose()?
    .is_some_and(|enabled| enabled != 0);
    let unfinished = sqlx::query(
        "SELECT COUNT(*) AS n FROM gc_cycles c JOIN agents a ON a.repository_id = c.repository_id \
         WHERE a.id = $1 AND c.dry_run = 0 AND (c.state NOT IN ('done', 'failed', 'aborted') \
         OR (c.state = 'failed' AND c.error_after_delete = 1))",
    )
    .bind(host_id)
    .fetch_one(&state.db.pool)
    .await?
    .try_get::<i64, _>("n")?
        > 0;
    Ok(GcGate {
        enabled,
        unfinished,
    })
}

pub async fn assign_queued(state: &AppState, only_agent: Option<&str>) -> anyhow::Result<()> {
    let rows = match only_agent {
        Some(agent) => {
            sqlx::query(
                "SELECT id, host_id, spec, kind, restore_json FROM jobs \
                 WHERE state = 'queued' AND source = 'manual' \
                 AND host_id = $1 ORDER BY queued_at, id",
            )
            .bind(agent)
            .fetch_all(&state.db.pool)
            .await?
        }
        None => {
            sqlx::query(
                "SELECT id, host_id, spec, kind, restore_json FROM jobs \
                 WHERE state = 'queued' AND source = 'manual' ORDER BY queued_at, id",
            )
            .fetch_all(&state.db.pool)
            .await?
        }
    };
    let mut seen = HashSet::new();
    for row in rows {
        let job_id: String = row.try_get("id")?;
        let host_id: String = row.try_get("host_id")?;
        if !seen.insert(host_id.clone())
            || !state.registry.is_online(&host_id)
            || !state.registry.live.is_ready(&host_id)
        {
            continue;
        }
        let kind: String = row.try_get("kind")?;
        let raw: Vec<u8> = row.try_get("spec")?;
        let (spec, restore) = if kind == KIND_RESTORE {
            if !state.registry.supports(&host_id, CAP_RESTORE) {
                fail_job(
                    state,
                    &job_id,
                    "restore_agent_outdated",
                    "Update the agent on this host to restore to it.",
                )
                .await?;
                continue;
            }
            let source = row
                .try_get::<Option<String>, _>("restore_json")?
                .and_then(|json| serde_json::from_str::<RestoreRecord>(&json).ok())
                .map(|record| record.source_repository_id);
            if gc_db::sweeping(state, source.as_deref()).await? {
                continue;
            }
            match RestoreSpec::decode(raw.as_slice()) {
                Ok(spec) => (None, Some(spec)),
                Err(_) => {
                    fail_unreadable(state, &job_id, &kind).await?;
                    continue;
                }
            }
        } else {
            match JobSpec::decode(raw.as_slice()) {
                Ok(mut spec) => {
                    if spec.max_runtime_s == 0 {
                        spec.max_runtime_s = DEFAULT_MAX_RUNTIME.as_secs();
                    }
                    let gc = gc_gate(state, &host_id).await?;
                    let aware = state.registry.supports(&host_id, CAP_GC);
                    if gc.unfinished && !aware {
                        fail_job(
                            state,
                            &job_id,
                            "gc_agent_outdated",
                            "Update the agent on this host: a cleanup cycle is running in its repository.",
                        )
                        .await?;
                        continue;
                    }
                    spec.gc_locks_required = aware && (gc.enabled || gc.unfinished);
                    (Some(spec), None)
                }
                Err(_) => {
                    fail_unreadable(state, &job_id, &kind).await?;
                    continue;
                }
            }
        };
        let claimed = sqlx::query(
            "UPDATE jobs SET state = 'assigned', assigned_at = $1 WHERE id = $2 AND state = 'queued' \
             AND NOT EXISTS (SELECT 1 FROM jobs WHERE host_id = $3 AND source = 'manual' \
             AND state IN ('assigned', 'running'))",
        )
        .bind(now_ms())
        .bind(&job_id)
        .bind(&host_id)
        .execute(&state.db.pool)
        .await;
        match claimed {
            Ok(done) if done.rows_affected() == 1 => {}
            Ok(_) => continue,
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => continue,
            Err(error) => return Err(error.into()),
        }
        let has_limits = spec
            .as_ref()
            .and_then(|s| s.limits.as_ref())
            .is_some_and(|l| *l != Limits::default());
        if has_limits
            && !state.registry.supports(&host_id, CAP_LIMITS)
            && let Err(error) = log_limits_ignored(state, &job_id).await
        {
            tracing::warn!(%error, job_id, "writing the limits warning failed");
        }
        let restore = match restore {
            Some(spec) => {
                let record: Option<String> = row.try_get("restore_json")?;
                match with_source_keys(state, spec, record.as_deref()).await {
                    Ok(spec) => Some(spec),
                    Err(error) => {
                        tracing::warn!(%error, job_id, "preparing a restore job failed");
                        fail_job(
                            state,
                            &job_id,
                            "restore_failed",
                            "The source repository could not be opened.",
                        )
                        .await?;
                        continue;
                    }
                }
            }
            None => None,
        };
        let sent = state.registry.send(
            &host_id,
            server_message(ServerBody::JobAssign(JobAssign {
                job_id: job_id.clone(),
                spec,
                restore,
            })),
        );
        if !sent {
            requeue_assigned(state, &job_id, &host_id).await?;
        }
    }
    Ok(())
}

async fn with_source_keys(
    state: &AppState,
    mut spec: RestoreSpec,
    record: Option<&str>,
) -> anyhow::Result<RestoreSpec> {
    let record: RestoreRecord =
        serde_json::from_str(record.ok_or_else(|| anyhow::anyhow!("restore_json is missing"))?)?;
    let access = server_access(state, &record.source_repository_id).await?;
    let master: [u8; 32] = access
        .master
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("master secret must be 32 bytes"))?;
    let keys = ReadKeys::derive(MasterKey::from_bytes(master), access.format_repo_id);
    spec.source_repo_id = keys.repo_id().to_vec();
    spec.k_id = keys.k_id().to_vec();
    Ok(spec)
}

pub async fn requeue_assigned(state: &AppState, job_id: &str, host_id: &str) -> anyhow::Result<()> {
    let cancelled = sqlx::query(
        "UPDATE jobs SET state = 'cancelled', finished_at = $1, assigned_at = NULL \
         WHERE id = $2 AND host_id = $3 AND state = 'assigned' AND cancel_requested = 1",
    )
    .bind(now_ms())
    .bind(job_id)
    .bind(host_id)
    .execute(&state.db.pool)
    .await?
    .rows_affected();
    if cancelled == 0 {
        sqlx::query(
            "UPDATE jobs SET state = 'queued', assigned_at = NULL \
             WHERE id = $1 AND host_id = $2 AND state = 'assigned'",
        )
        .bind(job_id)
        .bind(host_id)
        .execute(&state.db.pool)
        .await?;
    }
    state.registry.live.cancel_unsent(job_id);
    Ok(())
}

async fn fail_job(state: &AppState, job_id: &str, code: &str, message: &str) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE jobs SET state = 'failed', finished_at = $1, error_code = $2, error_message = $3 \
         WHERE id = $4 AND state IN ('queued', 'assigned')",
    )
    .bind(now_ms())
    .bind(code)
    .bind(message)
    .bind(job_id)
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

async fn log_limits_ignored(state: &AppState, job_id: &str) -> anyhow::Result<()> {
    let mut tx = state.db.pool.begin().await?;
    let logged = sqlx::query(
        "INSERT INTO job_events (job_id, seq, at, level, kind, message) \
         VALUES ($1, $2, $3, 'warning', 'limits_ignored', $4) ON CONFLICT DO NOTHING",
    )
    .bind(job_id)
    .bind(LIMITS_IGNORED_SEQ)
    .bind(now_ms())
    .bind(LIMITS_IGNORED_MESSAGE)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if logged == 1 {
        sqlx::query("UPDATE jobs SET log_lines = log_lines + 1 WHERE id = $1")
            .bind(job_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn fail_unreadable(state: &AppState, job_id: &str, kind: &str) -> anyhow::Result<()> {
    let code = if kind == KIND_RESTORE {
        "restore_failed"
    } else {
        "backup_failed"
    };
    fail_job(state, job_id, code, "The job spec could not be read.").await
}

async fn push_cancels(state: &AppState) -> anyhow::Result<()> {
    let rows = sqlx::query(
        "SELECT id, host_id FROM jobs WHERE cancel_requested = 1 AND state IN ('assigned', 'running')",
    )
    .fetch_all(&state.db.pool)
    .await?;
    for row in rows {
        let job_id: String = row.try_get("id")?;
        let host_id: String = row.try_get("host_id")?;
        if !state.registry.is_online(&host_id) || !state.registry.live.cancel_is_new(&job_id) {
            continue;
        }
        let sent = state.registry.send(
            &host_id,
            server_message(ServerBody::JobCancel(JobCancel {
                job_id: job_id.clone(),
            })),
        );
        if !sent {
            state.registry.live.cancel_unsent(&job_id);
        }
    }
    Ok(())
}

async fn flush_progress(state: &AppState) {
    for (job_id, progress) in state.registry.live.take_due(PROGRESS_FLUSH) {
        let Ok(json) = serde_json::to_string(&progress) else {
            continue;
        };
        let result = sqlx::query(
            "UPDATE jobs SET progress_json = $1 WHERE id = $2 AND state IN ('assigned', 'running')",
        )
        .bind(json)
        .bind(&job_id)
        .execute(&state.db.pool)
        .await;
        if let Err(error) = result {
            tracing::warn!(%error, job_id, "could not store job progress");
        }
    }
}

async fn fail_overdue(pool: &sqlx::AnyPool, now_ms: i64) -> anyhow::Result<()> {
    let margin_ms = i64::try_from(RUNTIME_MARGIN.as_millis())?;
    let active = sqlx::query(
        "SELECT id, spec, COALESCE(started_at, assigned_at) AS began FROM jobs \
         WHERE kind <> $1 AND state IN ('assigned', 'running') \
         AND COALESCE(started_at, assigned_at) < $2",
    )
    .bind(KIND_RESTORE)
    .bind(now_ms - margin_ms)
    .fetch_all(pool)
    .await?;
    for row in active {
        let raw: Vec<u8> = row.try_get("spec")?;
        let limit = match JobSpec::decode(raw.as_slice()) {
            Ok(spec) if spec.max_runtime_s > 0 => Duration::from_secs(spec.max_runtime_s),
            _ => DEFAULT_MAX_RUNTIME,
        };
        let deadline = row
            .try_get::<i64, _>("began")?
            .saturating_add(i64::try_from((limit + RUNTIME_MARGIN).as_millis())?);
        if deadline >= now_ms {
            continue;
        }
        sqlx::query(
            "UPDATE jobs SET state = 'failed', finished_at = $1, error_code = 'time_limit', \
             error_message = $2 WHERE id = $3 AND state IN ('assigned', 'running')",
        )
        .bind(now_ms)
        .bind(TIME_LIMIT_MESSAGE)
        .bind(row.try_get::<String, _>("id")?)
        .execute(pool)
        .await?;
    }
    Ok(())
}

pub async fn janitor_once(state: &AppState, now_ms: i64) -> anyhow::Result<()> {
    let pool = &state.db.pool;
    sqlx::query(
        "UPDATE jobs SET state = 'not_started', finished_at = $1, error_code = 'not_started', \
         error_message = 'The job was not started within 24 hours.' \
         WHERE state = 'queued' AND queued_at < $2",
    )
    .bind(now_ms)
    .bind(now_ms - QUEUED_TTL_MS)
    .execute(pool)
    .await?;

    let stale =
        sqlx::query("SELECT id, host_id FROM jobs WHERE state = 'assigned' AND assigned_at < $1")
            .bind(now_ms - UNACCEPTED_TTL_MS)
            .fetch_all(pool)
            .await?;
    for row in stale {
        let host_id: String = row.try_get("host_id")?;
        requeue_assigned(state, &row.try_get::<String, _>("id")?, &host_id).await?;
    }

    let lost = sqlx::query(
        "SELECT j.id, j.host_id FROM jobs j JOIN agents a ON a.id = j.host_id \
         WHERE j.state = 'running' AND (a.last_seen_at IS NULL OR a.last_seen_at < $1)",
    )
    .bind(now_ms - RUNNING_OFFLINE_TTL_MS)
    .fetch_all(pool)
    .await?;
    for row in lost {
        let host_id: String = row.try_get("host_id")?;
        if !state.registry.is_online(&host_id) {
            messages::interrupt(state, &host_id, &row.try_get::<String, _>("id")?).await?;
        }
    }

    fail_overdue(pool, now_ms).await?;

    sqlx::query(
        "DELETE FROM job_events WHERE job_id IN \
         (SELECT id FROM jobs WHERE finished_at < $1 AND logs_purged = 0)",
    )
    .bind(now_ms - LOG_TTL_MS)
    .execute(pool)
    .await?;
    sqlx::query("UPDATE jobs SET logs_purged = 1 WHERE finished_at < $1 AND logs_purged = 0")
        .bind(now_ms - LOG_TTL_MS)
        .execute(pool)
        .await?;

    sqlx::query(
        "DELETE FROM job_events WHERE job_id IN (SELECT id FROM jobs WHERE finished_at < $1)",
    )
    .bind(now_ms - JOB_TTL_MS)
    .execute(pool)
    .await?;
    sqlx::query("DELETE FROM jobs WHERE finished_at < $1")
        .bind(now_ms - JOB_TTL_MS)
        .execute(pool)
        .await?;
    Ok(())
}
