use std::collections::BTreeMap;

use anyhow::Context;
use lodestore_format::index::{hex32, parse_hex32};
use lodestore_format::snapshot::snapshot_object_key;
use lodestore_format::store::{BlobDelete, BlobError};
use lodestore_policy::retention::{Candidate, Decision, Rule, evaluate, timezone_of};
use lodestore_policy::{BackupPolicy, Retention};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use super::anomaly;
use super::open_repository;
use crate::db::now_ms;
use crate::find;
use crate::grpc::dispatch::messages::{LOG_LIMIT, LOG_LIMIT_MESSAGE, clip};
use crate::state::AppState;

const RUNNING: &str = r#"{"state":"running"}"#;
const SKIPPED: &str = r#"{"state":"skipped"}"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgetOutcome {
    Deleted,
    Blocked { reason: String },
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedSnapshot {
    pub id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairOutcome {
    pub forgot: u64,
    pub kept: u64,
    pub blocked: Vec<BlockedSnapshot>,
    pub postponed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanItem {
    pub id: String,
    pub received_at_ms: i64,
    pub time_ms: i64,
    pub suspicious: bool,
    pub keep: bool,
    pub reasons: Vec<Rule>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostPlan {
    pub host_id: String,
    pub hostname: String,
    pub items: Vec<PlanItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RetentionPlan {
    pub hosts: Vec<HostPlan>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pool {
    Scheduled,
    Manual,
}

struct PairRow {
    pool: Pool,
    host_id: String,
    hostname: String,
    time_ms: i64,
    exact: bool,
    candidate: Candidate,
}

const PAIRS_SQL: &str = "SELECT s.id, s.host_id, a.hostname, s.time_ms, s.received_at, \
     s.received_exact, s.suspicious, s.incomplete, s.origin, s.stdin_name FROM snapshots s \
     JOIN agents a ON a.id = s.host_id \
     WHERE s.policy_name = $1 AND s.repository_id = $2 AND s.job_id IS NOT NULL \
     AND s.verified_at IS NOT NULL";
const HOST_SQL: &str = "SELECT s.id, s.host_id, a.hostname, s.time_ms, s.received_at, \
     s.received_exact, s.suspicious, s.incomplete, s.origin, s.stdin_name FROM snapshots s \
     JOIN agents a ON a.id = s.host_id \
     WHERE s.policy_name = $1 AND s.repository_id = $2 AND s.host_id = $3 \
     AND s.job_id IS NOT NULL AND s.verified_at IS NOT NULL";

async fn repository_of(state: &AppState, policy: &BackupPolicy) -> anyhow::Result<String> {
    sqlx::query("SELECT id FROM repositories WHERE name = $1")
        .bind(&policy.spec.repository)
        .fetch_optional(&state.db.pool)
        .await?
        .with_context(|| format!("repository {} is missing", policy.spec.repository))?
        .try_get("id")
        .map_err(Into::into)
}

async fn load_pairs(
    state: &AppState,
    repository_id: &str,
    policy: &str,
    host_id: Option<&str>,
) -> anyhow::Result<Vec<PairRow>> {
    let query = match host_id {
        Some(host) => sqlx::query(HOST_SQL)
            .bind(policy)
            .bind(repository_id)
            .bind(host),
        None => sqlx::query(PAIRS_SQL).bind(policy).bind(repository_id),
    };
    query
        .fetch_all(&state.db.pool)
        .await?
        .iter()
        .filter_map(|r| pair_row(r).transpose())
        .collect()
}

fn pair_row(r: &sqlx::any::AnyRow) -> anyhow::Result<Option<PairRow>> {
    let origin: Option<String> = r.try_get("origin")?;
    let stdin_name: Option<String> = r.try_get("stdin_name")?;
    if origin.as_deref() == Some("stdin") || stdin_name.is_some() {
        return Ok(None);
    }
    let pool = if origin.as_deref() == Some("manual") {
        Pool::Manual
    } else {
        Pool::Scheduled
    };
    Ok(Some(PairRow {
        pool,
        host_id: r.try_get("host_id")?,
        hostname: r.try_get("hostname")?,
        time_ms: r.try_get("time_ms")?,
        exact: r.try_get::<i64, _>("received_exact")? != 0,
        candidate: Candidate {
            id: r.try_get("id")?,
            received_at_ms: r.try_get("received_at")?,
            suspicious: r.try_get::<i64, _>("suspicious")? != 0,
            incomplete: r.try_get::<i64, _>("incomplete")? != 0,
        },
    }))
}

fn times_unconfirmed(rows: &[&PairRow]) -> bool {
    rows.iter().any(|r| !r.exact)
}

fn decide(policy: &BackupPolicy, rules: &Retention, rows: &[&PairRow]) -> Vec<Decision> {
    if times_unconfirmed(rows) {
        return rows
            .iter()
            .map(|r| Decision {
                id: r.candidate.id.clone(),
                keep: true,
                reasons: Vec::new(),
            })
            .collect();
    }
    let timezone = timezone_of(&policy.spec);
    let mut decisions: Vec<Decision> = [Pool::Scheduled, Pool::Manual]
        .into_iter()
        .flat_map(|pool| {
            let candidates: Vec<Candidate> = rows
                .iter()
                .filter(|r| r.pool == pool)
                .map(|r| r.candidate.clone())
                .collect();
            evaluate(rules, &timezone, &candidates)
        })
        .collect();
    let received: BTreeMap<&str, i64> = rows
        .iter()
        .map(|r| (r.candidate.id.as_str(), r.candidate.received_at_ms))
        .collect();
    decisions.sort_by(|a, b| {
        received[b.id.as_str()]
            .cmp(&received[a.id.as_str()])
            .then_with(|| a.id.cmp(&b.id))
    });
    decisions
}

pub async fn plan_policy(
    state: &AppState,
    policy: &BackupPolicy,
    rules: &Retention,
) -> anyhow::Result<RetentionPlan> {
    let repository_id = repository_of(state, policy).await?;
    let rows = load_pairs(state, &repository_id, &policy.metadata.name, None).await?;
    let mut by_host: BTreeMap<(&str, &str), Vec<&PairRow>> = BTreeMap::new();
    for row in &rows {
        by_host
            .entry((row.hostname.as_str(), row.host_id.as_str()))
            .or_default()
            .push(row);
    }
    let hosts = by_host
        .into_iter()
        .map(|((hostname, host_id), rows)| {
            let times: BTreeMap<&str, &PairRow> =
                rows.iter().map(|r| (r.candidate.id.as_str(), *r)).collect();
            let items = decide(policy, rules, &rows)
                .into_iter()
                .filter_map(|d| {
                    let row = times.get(d.id.as_str())?;
                    Some(PlanItem {
                        received_at_ms: row.candidate.received_at_ms,
                        time_ms: row.time_ms,
                        suspicious: row.candidate.suspicious,
                        keep: d.keep,
                        reasons: d.reasons,
                        id: d.id,
                    })
                })
                .collect();
            HostPlan {
                host_id: host_id.to_string(),
                hostname: hostname.to_string(),
                items,
            }
        })
        .collect();
    Ok(RetentionPlan { hosts })
}

pub async fn forget_snapshot(
    state: &AppState,
    repository_id: &str,
    snapshot_id: &str,
) -> anyhow::Result<ForgetOutcome> {
    let id = parse_hex32(snapshot_id).context("snapshot id must be 64 hex characters")?;
    let hex = hex32(&id);
    sqlx::query("SELECT 1 FROM snapshots WHERE id = $1 AND repository_id = $2")
        .bind(&hex)
        .bind(repository_id)
        .fetch_optional(&state.db.pool)
        .await?
        .with_context(|| format!("snapshot {hex} is not in the catalog"))?;
    let source = open_repository(state, repository_id).await?;
    match source.store.delete(&snapshot_object_key(&id)).await {
        Ok(()) | Err(BlobError::NotFound) => {
            drop_row(state, repository_id, &hex).await?;
            Ok(ForgetOutcome::Deleted)
        }
        Err(BlobError::Denied { message }) => {
            let mut tx = state.db.pool.begin().await?;
            sqlx::query(
                "INSERT INTO snapshot_tombstones (id, repository_id, forgotten_at, reason) \
                 VALUES ($1, $2, $3, $4) ON CONFLICT (id) DO NOTHING",
            )
            .bind(&hex)
            .bind(repository_id)
            .bind(now_ms())
            .bind(clip(&message))
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM snapshots WHERE id = $1 AND repository_id = $2")
                .bind(&hex)
                .bind(repository_id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            find::cache::remove(&find::cache::dir(&state.config.data_dir), &hex);
            Ok(ForgetOutcome::Blocked { reason: message })
        }
        Err(error) => Ok(ForgetOutcome::Failed {
            reason: error.to_string(),
        }),
    }
}

async fn drop_row(state: &AppState, repository_id: &str, hex: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM snapshots WHERE id = $1 AND repository_id = $2")
        .bind(hex)
        .bind(repository_id)
        .execute(&state.db.pool)
        .await?;
    find::cache::remove(&find::cache::dir(&state.config.data_dir), hex);
    Ok(())
}

pub async fn append_event(
    state: &AppState,
    job_id: &str,
    level: &str,
    kind: &str,
    message: &str,
) -> anyhow::Result<()> {
    let mut tx = state.db.pool.begin().await?;
    let Some(row) = sqlx::query(
        "UPDATE jobs SET log_seq = log_seq + 1, log_lines = log_lines + 1 \
         WHERE id = $1 AND log_lines < $2 RETURNING log_seq, log_lines",
    )
    .bind(job_id)
    .bind(LOG_LIMIT)
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(());
    };
    let (seq, lines): (i64, i64) = (row.try_get("log_seq")?, row.try_get("log_lines")?);
    let (level, kind, message) = if lines == LOG_LIMIT {
        ("warning", "log_limit", LOG_LIMIT_MESSAGE.to_string())
    } else {
        (level, kind, clip(message))
    };
    sqlx::query(
        "INSERT INTO job_events (job_id, seq, at, level, kind, message) \
         VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING",
    )
    .bind(job_id)
    .bind(seq)
    .bind(now_ms())
    .bind(level)
    .bind(kind)
    .bind(message)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn note(state: &AppState, job_id: &str, level: &str, kind: &str, message: &str) {
    if let Err(error) = append_event(state, job_id, level, kind, message).await {
        tracing::warn!(%error, job_id, "writing a retention log entry failed");
    }
}

async fn set_retention(state: &AppState, job_id: &str, json: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE jobs SET retention_json = $1 WHERE id = $2")
        .bind(json)
        .bind(job_id)
        .execute(&state.db.pool)
        .await?;
    Ok(())
}

pub async fn run_for_pair(
    state: &AppState,
    policy: &BackupPolicy,
    host_id: &str,
    job_id: &str,
) -> anyhow::Result<PairOutcome> {
    let rules = policy.spec.retention.clone().unwrap_or_default();
    let repository_id = repository_of(state, policy).await?;
    let lock = state.catalog.pair_lock(&policy.metadata.name, host_id);
    let _pair = lock.lock().await;
    let rows = load_pairs(state, &repository_id, &policy.metadata.name, Some(host_id)).await?;
    let refs: Vec<&PairRow> = rows.iter().collect();
    let mut outcome = PairOutcome::default();
    if times_unconfirmed(&refs) {
        note(
            state,
            job_id,
            "warning",
            "retention_postponed",
            "Retention postponed until snapshot times are confirmed by a catalog sync",
        )
        .await;
        outcome.postponed = true;
        return Ok(outcome);
    }
    for decision in decide(policy, &rules, &refs) {
        if decision.keep {
            outcome.kept += u64::from(!decision.reasons.contains(&Rule::Suspicious));
            continue;
        }
        let id = &decision.id;
        match forget_snapshot(state, &repository_id, id).await {
            Ok(ForgetOutcome::Deleted) => {
                outcome.forgot += 1;
                note(
                    state,
                    job_id,
                    "info",
                    "snapshot_forgotten",
                    &format!("Forgot snapshot {id}: not kept by any rule"),
                )
                .await;
            }
            Ok(ForgetOutcome::Blocked { reason }) => {
                note(
                    state,
                    job_id,
                    "warning",
                    "forget_blocked",
                    &format!("Could not delete snapshot {id}: {reason}"),
                )
                .await;
                outcome.blocked.push(BlockedSnapshot {
                    id: id.clone(),
                    reason,
                });
            }
            Ok(ForgetOutcome::Failed { reason }) => {
                tracing::warn!(snapshot = %id, %reason, "deleting a snapshot failed, retrying on the next pass");
            }
            Err(error) => {
                tracing::warn!(snapshot = %id, %error, "forgetting a snapshot failed");
            }
        }
    }
    let done = serde_json::json!({
        "state": "done",
        "forgot": outcome.forgot,
        "kept": outcome.kept,
        "blocked": outcome.blocked,
    });
    set_retention(state, job_id, &done.to_string()).await?;
    Ok(outcome)
}

pub async fn after_backup(state: &AppState, job_id: &str) -> anyhow::Result<()> {
    let claimed = sqlx::query(
        "UPDATE jobs SET retention_json = $1 WHERE id = $2 AND retention_json IS NULL \
         AND kind = 'backup' AND state IN ('succeeded', 'warnings') AND snapshot_id IS NOT NULL",
    )
    .bind(RUNNING)
    .bind(job_id)
    .execute(&state.db.pool)
    .await?
    .rows_affected();
    if claimed == 0 {
        return Ok(());
    }
    match apply(state, job_id).await {
        Ok(true) => Ok(()),
        Ok(false) => set_retention(state, job_id, SKIPPED).await,
        Err(error) => {
            sqlx::query(
                "UPDATE jobs SET retention_json = NULL WHERE id = $1 AND retention_json = $2",
            )
            .bind(job_id)
            .bind(RUNNING)
            .execute(&state.db.pool)
            .await?;
            Err(error)
        }
    }
}

async fn apply(state: &AppState, job_id: &str) -> anyhow::Result<bool> {
    let job = sqlx::query("SELECT policy_name, host_id, snapshot_id FROM jobs WHERE id = $1")
        .bind(job_id)
        .fetch_one(&state.db.pool)
        .await?;
    let Some(name) = job.try_get::<Option<String>, _>("policy_name")? else {
        return Ok(false);
    };
    let host_id: String = job.try_get("host_id")?;
    let snapshot_id: String = job.try_get("snapshot_id")?;
    let Some(policy) = sqlx::query("SELECT spec_json FROM policies WHERE name = $1")
        .bind(&name)
        .fetch_optional(&state.db.pool)
        .await?
    else {
        return Ok(false);
    };
    let policy: BackupPolicy = serde_json::from_str(&policy.try_get::<String, _>("spec_json")?)?;
    let Some(snapshot) = sqlx::query("SELECT incomplete FROM snapshots WHERE id = $1")
        .bind(&snapshot_id)
        .fetch_optional(&state.db.pool)
        .await?
    else {
        tracing::warn!(
            job_id,
            snapshot_id,
            "the snapshot of a finished backup is not in the catalog, retention waits for the next backup"
        );
        return Ok(false);
    };
    if snapshot.try_get::<i64, _>("incomplete")? != 0 {
        return Ok(false);
    }
    if let Some(reasons) = anomaly::evaluate(state, &snapshot_id).await?
        && let Some(first) = reasons.first()
    {
        let reason = first.describe();
        note(
            state,
            job_id,
            "warning",
            "suspicious_snapshot",
            &format!(
                "Snapshot looks unusual: {}. Retention will not count it",
                reason.trim_end_matches('.')
            ),
        )
        .await;
    }
    if policy
        .spec
        .retention
        .as_ref()
        .is_none_or(Retention::is_empty)
    {
        return Ok(false);
    }
    let outcome = run_for_pair(state, &policy, &host_id, job_id).await?;
    Ok(!outcome.postponed)
}
