use std::collections::HashSet;

use lodestore_format::Error as FormatError;
use lodestore_format::crypto::Entropy;
use lodestore_format::locks::{LOCKS_PREFIX, lock_id_hex, new_lock_id, read_gc_locks};
use lodestore_format::store::{BlobDelete, BlobError, BlobStore};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use utoipa::ToSchema;

use super::db;
use crate::catalog::{OpenRepository, open_repository};
use crate::db::now_ms;
use crate::grpc::registry::CAP_GC;
use crate::repos::checks::open_store;
use crate::repos::db::server_access;
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PreflightKind {
    OldAgents,
    ServerKey,
    AgentKey,
    ForeignCycle,
    Unreadable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HostRef {
    pub id: String,
    pub hostname: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PreflightProblem {
    pub kind: PreflightKind,
    pub message: String,
    #[serde(default)]
    pub hosts: Vec<HostRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Preflight {
    pub problems: Vec<PreflightProblem>,
    pub dry_run_allowed: bool,
    pub checked_at: i64,
}

fn problem(kind: PreflightKind, message: impl Into<String>) -> PreflightProblem {
    PreflightProblem {
        kind,
        message: message.into(),
        hosts: Vec::new(),
    }
}

/// Agents that have not said they support `cap`; one that has not connected
/// since the server learned about capabilities counts as old.
pub(crate) async fn old_agents(
    state: &AppState,
    repository_id: &str,
    cap: &str,
) -> anyhow::Result<Vec<HostRef>> {
    let rows = sqlx::query(
        "SELECT id, hostname, capabilities_json FROM agents \
         WHERE repository_id = $1 AND status <> 'revoked' ORDER BY hostname, id",
    )
    .bind(repository_id)
    .fetch_all(&state.db.pool)
    .await?;
    let mut old = Vec::new();
    for row in rows {
        let capabilities: Vec<String> = row
            .try_get::<Option<String>, _>("capabilities_json")?
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        if !capabilities.iter().any(|c| c == cap) {
            old.push(HostRef {
                id: row.try_get("id")?,
                hostname: row.try_get("hostname")?,
            });
        }
    }
    Ok(old)
}

pub(crate) async fn old_agents_problem(
    state: &AppState,
    repository_id: &str,
) -> anyhow::Result<Option<PreflightProblem>> {
    let old = old_agents(state, repository_id, CAP_GC).await?;
    Ok((!old.is_empty()).then(|| PreflightProblem {
        hosts: old,
        ..problem(
            PreflightKind::OldAgents,
            "Some agents in this repository don't support cleanup yet. Update or revoke them.",
        )
    }))
}

/// Every marker this server published for the repository. One left behind
/// by a cycle that ended is taken down by the next cycle, so it is not
/// another server's.
pub(super) async fn own_gc_ids(
    state: &AppState,
    repository_id: &str,
) -> anyhow::Result<HashSet<String>> {
    sqlx::query("SELECT gc_id FROM gc_cycles WHERE repository_id = $1 AND gc_id IS NOT NULL")
        .bind(repository_id)
        .fetch_all(&state.db.pool)
        .await?
        .iter()
        .map(|row| Ok(row.try_get("gc_id")?))
        .collect()
}

async fn foreign_cycle(
    state: &AppState,
    repository_id: &str,
    source: &OpenRepository,
) -> anyhow::Result<Option<PreflightProblem>> {
    let locks = match read_gc_locks(&source.store, source.keys.k_idx(), source.keys.repo_id()).await
    {
        Ok(locks) => locks,
        Err(FormatError::Corrupt { object, .. }) => {
            return Ok(Some(problem(
                PreflightKind::ForeignCycle,
                format!("{object} can't be read, so another cleanup may be marked in this bucket."),
            )));
        }
        Err(error) => {
            return Ok(Some(problem(
                PreflightKind::Unreadable,
                format!("The locks folder can't be read: {error}"),
            )));
        }
    };
    if locks.denied {
        return Ok(Some(problem(
            PreflightKind::Unreadable,
            "The server's storage key can't read the locks folder, so another cleanup can't be \
             ruled out.",
        )));
    }
    let ours = own_gc_ids(state, repository_id).await?;
    let foreign = locks
        .manifests
        .iter()
        .any(|m| !ours.contains(&lock_id_hex(&m.gc_id)));
    Ok(foreign.then(|| {
        problem(
            PreflightKind::ForeignCycle,
            "Another cleanup is marked in this bucket and was not started by this server.",
        )
    }))
}

/// Why a probe that should have succeeded (or found nothing) did not; an
/// answer that proves nothing counts as a problem, not as permission.
fn refused<T>(result: &Result<T, BlobError>) -> Option<String> {
    match result {
        Ok(_) | Err(BlobError::NotFound) => None,
        Err(BlobError::Denied { .. }) => Some(String::new()),
        Err(error) => Some(format!(" (the storage answered: {error})")),
    }
}

/// Asks the storage what the two keys may do without writing anything: a
/// DELETE and a GET of a key that does not exist, and a LIST of `locks/`.
async fn probe_keys(
    state: &AppState,
    repository_id: &str,
    source: &OpenRepository,
) -> anyhow::Result<Vec<PreflightProblem>> {
    let access = server_access(state, repository_id).await?;
    if access.location.backend != "s3" {
        return Ok(Vec::new());
    }
    let key = format!(
        "{LOCKS_PREFIX}probe-{}",
        lock_id_hex(&new_lock_id(&mut Entropy::os()))
    );
    let mut out = Vec::new();
    if let Some(why) = refused(&source.store.delete(&key).await) {
        out.push(problem(
            PreflightKind::ServerKey,
            format!("The server's storage key cannot delete objects{why}."),
        ));
    }
    let agent = open_store(&access.location, access.agent_creds.as_ref())?;
    let why = refused(&agent.list(LOCKS_PREFIX).await).or(refused(&agent.get(&key).await));
    if let Some(why) = why {
        out.push(problem(
            PreflightKind::AgentKey,
            format!("The agent key cannot read the locks folder{why}."),
        ));
    }
    Ok(out)
}

async fn stored_probe(state: &AppState, repository_id: &str) -> Vec<PreflightProblem> {
    match db::load_probe(state, repository_id).await {
        Ok(Some((json, _))) => serde_json::from_str(&json).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Everything that makes it unsafe to turn cleanup on. `probe_storage`
/// re-asks the storage about the keys and remembers the answer; without it
/// the last answer is shown. A dry run never calls this with probing.
pub async fn run_preflight(
    state: &AppState,
    repository_id: &str,
    probe_storage: bool,
) -> anyhow::Result<Preflight> {
    let mut problems = Vec::new();
    let mut dry_run_allowed = true;

    problems.extend(old_agents_problem(state, repository_id).await?);

    match open_repository(state, repository_id).await {
        Err(error) => {
            dry_run_allowed = false;
            problems.push(problem(
                PreflightKind::Unreadable,
                format!("The repository can't be opened: {error:#}"),
            ));
        }
        Ok(source) => {
            problems.extend(foreign_cycle(state, repository_id, &source).await?);
            let probed = if probe_storage {
                let probed = probe_keys(state, repository_id, &source).await?;
                db::save_probe(state, repository_id, &serde_json::to_string(&probed)?).await?;
                probed
            } else {
                stored_probe(state, repository_id).await
            };
            problems.extend(probed);
        }
    }

    Ok(Preflight {
        problems,
        dry_run_allowed,
        checked_at: now_ms(),
    })
}
