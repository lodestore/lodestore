use std::collections::HashMap;

use anyhow::Context;
use lodestore_policy::BackupPolicy;
use lodestore_policy::jiff::Timestamp;
use serde_json::Value;
use sqlx::Row;

use super::model::{HostFacts, PairFacts, SnapshotFacts, overdue_limit};
use crate::agent_update::reached;
use crate::maintenance::check::CheckLevel;
use crate::policies::matching::AgentRow;
use crate::state::AppState;

#[derive(Debug, Default)]
pub struct Inputs {
    pub hosts: Vec<HostFacts>,
    pub server_cert: Option<(i64, bool)>,
    pub check_failures: Vec<CheckFailure>,
    pub update_failures: Vec<UpdateFailure>,
    pub unreadable_policies: Vec<String>,
}

/// The last verdict of a check level in a repository, when it was a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckFailure {
    pub repository_id: String,
    pub repository: String,
    pub level: CheckLevel,
    pub run_id: String,
    pub findings: u64,
    pub error: Option<String>,
    pub finished_at: i64,
}

/// A host whose latest finished self-update was rolled back and which still runs another version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateFailure {
    pub host_id: String,
    pub hostname: String,
    pub reason: String,
}

struct Agent {
    row: AgentRow,
    platform: String,
    enrolled_at: i64,
    last_seen_ms: Option<i64>,
    cert_not_after_ms: i64,
}

struct Policy {
    name: String,
    repository_id: String,
    policy: BackupPolicy,
    created_at: i64,
}

type Pair = (String, String);

pub async fn gather(state: &AppState, now_ms: i64) -> anyhow::Result<Inputs> {
    let agents = load_agents(state).await?;
    let (policies, unreadable_policies) = load_policies(state).await?;
    let successes = last_successes(state).await?;
    let mut failures = last_failures(state).await?;
    let mut snapshots = last_snapshots(state).await?;
    let now = Timestamp::from_millisecond(now_ms).context("the clock is out of range")?;
    let limits: Vec<_> = policies
        .iter()
        .map(|p| overdue_limit(&p.policy.spec, now))
        .collect();
    let hosts = agents
        .into_iter()
        .map(|agent| {
            let id = agent.row.id.clone();
            let pairs = policies
                .iter()
                .zip(&limits)
                .filter(|(p, _)| agent.row.applies(&p.repository_id, &p.policy.spec.selector))
                .map(|(p, limit)| {
                    let key = (p.name.clone(), id.clone());
                    let last_success_ms = successes.get(&key).copied();
                    let last_failure = failures
                        .remove(&key)
                        .filter(|(at, _)| last_success_ms.is_none_or(|ok| *at > ok));
                    PairFacts {
                        policy: p.name.clone(),
                        limit: *limit,
                        last_success_ms,
                        baseline_ms: agent.enrolled_at.max(p.created_at),
                        last_failure,
                    }
                })
                .collect();
            HostFacts {
                online: state.registry.is_online(&id),
                last_snapshot: snapshots.remove(&id),
                host_id: id,
                hostname: agent.row.hostname,
                labels: agent.row.labels,
                platform: agent.platform,
                pairs,
                last_seen_ms: agent.last_seen_ms,
                cert_not_after_ms: agent.cert_not_after_ms,
            }
        })
        .collect();
    let server_cert = match crate::grpc::server_cert_not_after(state) {
        Ok(cert) => Some(cert),
        Err(error) => {
            tracing::warn!(%error, "cannot read the server gRPC certificate");
            None
        }
    };
    let check_failures = check_failures(state).await?;
    let update_failures = update_failures(state).await?;
    Ok(Inputs {
        hosts,
        server_cert,
        check_failures,
        update_failures,
        unreadable_policies,
    })
}

pub(crate) async fn update_failures(state: &AppState) -> anyhow::Result<Vec<UpdateFailure>> {
    let rows = sqlx::query(
        "SELECT u.host_id, u.reason, u.target_version, a.hostname, a.version \
         FROM agent_updates u JOIN agents a ON a.id = u.host_id JOIN ( \
             SELECT host_id, MAX(finished_at) AS at FROM agent_updates \
             WHERE state IN ('succeeded', 'rolled_back') AND finished_at IS NOT NULL \
             GROUP BY host_id \
         ) l ON l.host_id = u.host_id AND l.at = u.finished_at \
         WHERE u.state = 'rolled_back' AND a.status <> 'revoked' \
         ORDER BY a.hostname, u.host_id",
    )
    .fetch_all(&state.db.pool)
    .await
    .context("reading the last agent updates")?;
    let mut out: Vec<UpdateFailure> = Vec::new();
    for row in &rows {
        let host_id: String = row.try_get("host_id")?;
        let version: String = row.try_get("version")?;
        let target: String = row.try_get("target_version")?;
        if reached(&version, &target) || out.iter().any(|seen| seen.host_id == host_id) {
            continue;
        }
        let reason: Option<String> = row.try_get("reason")?;
        out.push(UpdateFailure {
            host_id,
            hostname: row.try_get("hostname")?,
            reason: reason.unwrap_or_else(|| "The update was rolled back.".into()),
        });
    }
    Ok(out)
}

async fn check_failures(state: &AppState) -> anyhow::Result<Vec<CheckFailure>> {
    let rows = sqlx::query(
        "SELECT c.id, c.repository_id, c.level, c.state, c.findings_total, c.error, \
         c.finished_at, r.name AS repository \
         FROM check_runs c JOIN repositories r ON r.id = c.repository_id JOIN ( \
             SELECT repository_id, level, MAX(created_at) AS at FROM check_runs \
             WHERE state IN ('done', 'failed') AND (outcome IS NULL OR outcome <> 'stopped' OR report_json IS NOT NULL) \
             GROUP BY repository_id, level \
         ) l ON l.repository_id = c.repository_id AND l.level = c.level AND l.at = c.created_at \
         WHERE c.state = 'failed' AND (c.outcome IS NULL OR c.outcome <> 'stopped' OR c.report_json IS NOT NULL) \
         ORDER BY r.name, c.level, c.id",
    )
    .fetch_all(&state.db.pool)
    .await
    .context("reading the last check runs")?;
    let mut out: Vec<CheckFailure> = Vec::new();
    for row in &rows {
        let level: String = row.try_get("level")?;
        let Some(level) = CheckLevel::parse(&level) else {
            continue;
        };
        let repository_id: String = row.try_get("repository_id")?;
        if out
            .iter()
            .any(|seen| seen.repository_id == repository_id && seen.level == level)
        {
            continue;
        }
        out.push(CheckFailure {
            repository_id,
            repository: row.try_get("repository")?,
            level,
            run_id: row.try_get("id")?,
            findings: u64::try_from(row.try_get::<i64, _>("findings_total")?).unwrap_or(0),
            error: row.try_get("error")?,
            finished_at: row.try_get("finished_at")?,
        });
    }
    Ok(out)
}

async fn load_agents(state: &AppState) -> anyhow::Result<Vec<Agent>> {
    let rows = sqlx::query(
        "SELECT a.id, a.hostname, a.repository_id, a.labels_json, a.os, a.arch, a.enrolled_at, \
         a.last_seen_at, a.cert_not_after, r.name AS repository_name \
         FROM agents a JOIN repositories r ON r.id = a.repository_id \
         WHERE a.status <> 'revoked' ORDER BY a.hostname, a.id",
    )
    .fetch_all(&state.db.pool)
    .await
    .context("reading agents")?;
    rows.iter()
        .map(|row| {
            let labels_json: String = row.try_get("labels_json")?;
            let os: String = row.try_get("os")?;
            let arch: String = row.try_get("arch")?;
            Ok(Agent {
                row: AgentRow {
                    id: row.try_get("id")?,
                    hostname: row.try_get("hostname")?,
                    repository_id: row.try_get("repository_id")?,
                    repository_name: row.try_get("repository_name")?,
                    labels: serde_json::from_str(&labels_json).context("agent labels")?,
                    facts: Value::Null,
                },
                platform: format!("{os} / {arch}"),
                enrolled_at: row.try_get("enrolled_at")?,
                last_seen_ms: row.try_get("last_seen_at")?,
                cert_not_after_ms: row.try_get("cert_not_after")?,
            })
        })
        .collect()
}

async fn load_policies(state: &AppState) -> anyhow::Result<(Vec<Policy>, Vec<String>)> {
    let rows = sqlx::query("SELECT name, repository_id, spec_json, created_at FROM policies")
        .fetch_all(&state.db.pool)
        .await
        .context("reading policies")?;
    let mut out = Vec::new();
    let mut unreadable = Vec::new();
    for row in &rows {
        let name: String = row.try_get("name")?;
        match serde_json::from_str(&row.try_get::<String, _>("spec_json")?) {
            Ok(policy) => out.push(Policy {
                name,
                repository_id: row.try_get("repository_id")?,
                policy,
                created_at: row.try_get("created_at")?,
            }),
            Err(error) => {
                tracing::warn!(policy = %name, %error, "stored policy cannot be parsed");
                unreadable.push(name);
            }
        }
    }
    Ok((out, unreadable))
}

async fn last_successes(state: &AppState) -> anyhow::Result<HashMap<Pair, i64>> {
    let rows = sqlx::query(
        "SELECT policy_name, host_id, MAX(received_at) AS at FROM snapshots \
         WHERE incomplete = 0 AND policy_name IS NOT NULL AND host_id IS NOT NULL \
         GROUP BY policy_name, host_id",
    )
    .fetch_all(&state.db.pool)
    .await
    .context("reading the last successful backups")?;
    rows.iter()
        .map(|row| {
            Ok((
                (row.try_get("policy_name")?, row.try_get("host_id")?),
                row.try_get("at")?,
            ))
        })
        .collect()
}

async fn last_failures(state: &AppState) -> anyhow::Result<HashMap<Pair, (i64, String)>> {
    let rows = sqlx::query(
        "SELECT j.policy_name, j.host_id, j.state, j.finished_at, j.error_message \
         FROM jobs j JOIN ( \
             SELECT policy_name, host_id, MAX(finished_at) AS at FROM jobs \
             WHERE kind = 'backup' AND policy_name IS NOT NULL AND finished_at IS NOT NULL \
             GROUP BY policy_name, host_id \
         ) l ON l.policy_name = j.policy_name AND l.host_id = j.host_id AND l.at = j.finished_at \
         WHERE j.kind = 'backup'",
    )
    .fetch_all(&state.db.pool)
    .await
    .context("reading the last backup jobs")?;
    let mut out = HashMap::new();
    for row in &rows {
        let job_state: String = row.try_get("state")?;
        let message: Option<String> = row.try_get("error_message")?;
        let message = match job_state.as_str() {
            "failed" => message.unwrap_or_else(|| "The backup failed.".into()),
            "interrupted" => message.unwrap_or_else(|| "The backup was interrupted.".into()),
            _ => continue,
        };
        out.entry((row.try_get("policy_name")?, row.try_get("host_id")?))
            .or_insert((row.try_get("finished_at")?, message));
    }
    Ok(out)
}

async fn last_snapshots(state: &AppState) -> anyhow::Result<HashMap<String, SnapshotFacts>> {
    let rows = sqlx::query(
        "SELECT s.host_id, s.received_at, s.stats_json FROM snapshots s JOIN ( \
             SELECT host_id, MAX(received_at) AS at FROM snapshots \
             WHERE incomplete = 0 AND host_id IS NOT NULL GROUP BY host_id \
         ) l ON l.host_id = s.host_id AND l.at = s.received_at \
         WHERE s.incomplete = 0",
    )
    .fetch_all(&state.db.pool)
    .await
    .context("reading the last snapshots")?;
    let mut out = HashMap::new();
    for row in &rows {
        let stats: Value =
            serde_json::from_str(&row.try_get::<String, _>("stats_json")?).unwrap_or(Value::Null);
        out.entry(row.try_get("host_id")?).or_insert(SnapshotFacts {
            time_ms: row.try_get("received_at")?,
            bytes_total: stats["bytes_total"].as_u64().unwrap_or(0),
            bytes_added: stats["bytes_added"].as_u64().unwrap_or(0),
        });
    }
    Ok(out)
}
