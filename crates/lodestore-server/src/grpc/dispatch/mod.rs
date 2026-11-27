pub mod live;
pub mod messages;

use std::collections::HashSet;
use std::time::Duration;

use lodestore_format::keys::{MasterKey, ReadKeys};
use lodestore_format::locks::{DEFAULT_MAX_RUNTIME, RUNTIME_MARGIN};
use lodestore_policy::source::Source;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{
    JobAssign, JobCancel, JobSpec, Limits, RestoreSpec, ServerMessage, SourceInstance,
};
use prost::Message;
use sqlx::Row;
use tokio_util::sync::CancellationToken;

use crate::catalog::RepositoryUnavailable;
use crate::db::now_ms;
use crate::grpc::registry::{CAP_GC, CAP_LIMITS, CAP_RESTORE};
use crate::jobs::restore::RestoreRecord;
use crate::jobs::source_of;
use crate::maintenance::attest::load_distrusted;
use crate::maintenance::gc::db as gc_db;
use crate::repos::db::server_access_checked;
use crate::sources::{self, Guest, is_fresh, select::Selection};
use crate::state::AppState;

const KIND_RESTORE: &str = "restore";
const LIMITS_IGNORED_SEQ: i64 = -1;
const LIMITS_IGNORED_MESSAGE: &str =
    "The agent on this host is too old to apply the policy limits. Update the agent.";
const TIME_LIMIT_MESSAGE: &str = "The backup ran longer than its time limit and was stopped.";
const TICK: Duration = Duration::from_secs(1);
const JANITOR_EVERY: Duration = Duration::from_secs(60);
const PROGRESS_FLUSH: Duration = Duration::from_secs(10);
pub(crate) const GUEST_NOT_LOCAL: &str = "guest_not_local";
const GUEST_FACTS_WAIT_MS: i64 = 30 * 60 * 1000;
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

async fn host_open_error(state: &AppState, host_id: &str) -> anyhow::Result<Option<String>> {
    let row = sqlx::query(
        "SELECT r.open_error FROM agents a JOIN repositories r ON r.id = a.repository_id \
         WHERE a.id = $1",
    )
    .bind(host_id)
    .fetch_optional(&state.db.pool)
    .await?;
    Ok(row
        .map(|row| row.try_get::<Option<String>, _>("open_error"))
        .transpose()?
        .flatten())
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

/// Serializes the job claim and the agent update claim of one host: on
/// Postgres the second claimer waits for this row lock and then sees the
/// first claim committed.
pub async fn lock_host(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    host_id: &str,
) -> sqlx::Result<()> {
    sqlx::query("UPDATE agents SET status = status WHERE id = $1")
        .bind(host_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn distrusted_for_host(state: &AppState, host_id: &str) -> anyhow::Result<Vec<Vec<u8>>> {
    let repository: String = sqlx::query("SELECT repository_id FROM agents WHERE id = $1")
        .bind(host_id)
        .fetch_one(&state.db.pool)
        .await?
        .try_get("repository_id")?;
    Ok(load_distrusted(state, &repository)
        .await?
        .into_iter()
        .map(|id| id.to_vec())
        .collect())
}

pub async fn claim_job(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    job_id: &str,
    host_id: &str,
    now: i64,
) -> sqlx::Result<bool> {
    let done = sqlx::query(
        "UPDATE jobs SET state = 'assigned', assigned_at = $1 WHERE id = $2 AND state = 'queued' \
         AND NOT EXISTS (SELECT 1 FROM jobs WHERE host_id = $3 AND source = 'manual' \
         AND state IN ('assigned', 'running')) \
         AND NOT EXISTS (SELECT 1 FROM agent_updates WHERE host_id = $3 AND state = 'assigned')",
    )
    .bind(now)
    .bind(job_id)
    .bind(host_id)
    .execute(&mut **tx)
    .await?;
    Ok(done.rows_affected() == 1)
}

pub async fn assign_queued(state: &AppState, only_agent: Option<&str>) -> anyhow::Result<()> {
    let rows = match only_agent {
        Some(agent) => {
            sqlx::query(
                "SELECT id, host_id, spec, kind, restore_json, queued_at, source_ref, \
                 source_meta_json FROM jobs \
                 WHERE state = 'queued' AND source = 'manual' \
                 AND host_id = $1 ORDER BY queued_at, id",
            )
            .bind(agent)
            .fetch_all(&state.db.pool)
            .await?
        }
        None => {
            sqlx::query(
                "SELECT id, host_id, spec, kind, restore_json, queued_at, source_ref, \
                 source_meta_json FROM jobs \
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
            let record = row
                .try_get::<Option<String>, _>("restore_json")?
                .and_then(|json| serde_json::from_str::<RestoreRecord>(&json).ok());
            if record
                .as_ref()
                .is_some_and(|r| r.started_by.starts_with("token:"))
                && !crate::jobs::restore::agent_declares_roots(state, &host_id).await?
            {
                fail_job(
                    state,
                    &job_id,
                    "restore_agent_outdated",
                    crate::jobs::restore::ROOTS_OUTDATED,
                )
                .await?;
                continue;
            }
            let source = record.map(|record| record.source_repository_id);
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
                    match source_of(&spec) {
                        Ok(Some(source)) if matches!(source, Source::ProxmoxCt(_)) => {
                            let source_ref: String = row.try_get("source_ref")?;
                            let meta: Option<String> = row.try_get("source_meta_json")?;
                            let queued_at: i64 = row.try_get("queued_at")?;
                            match prepare_guest_job(
                                state,
                                &job_id,
                                &host_id,
                                &source,
                                (&source_ref, meta.as_deref()),
                                queued_at,
                            )
                            .await?
                            {
                                GuestStep::Wait => {
                                    seen.remove(&host_id);
                                    continue;
                                }
                                GuestStep::Done => continue,
                                GuestStep::Unreadable => {
                                    fail_unreadable(state, &job_id, &kind).await?;
                                    continue;
                                }
                                GuestStep::Go(instance) => spec.instance = Some(instance),
                            }
                        }
                        Ok(Some(source)) => {
                            if let Some((code, message)) =
                                source_gate(state, &host_id, &source, row.try_get("queued_at")?)
                                    .await?
                            {
                                fail_job_with_detail(
                                    state,
                                    &job_id,
                                    code,
                                    &message,
                                    source.type_name(),
                                )
                                .await?;
                                continue;
                            }
                        }
                        Ok(None) => {}
                        Err(_) => {
                            fail_unreadable(state, &job_id, &kind).await?;
                            continue;
                        }
                    }
                    if let Some(reason) = host_open_error(state, &host_id).await? {
                        fail_job(
                            state,
                            &job_id,
                            "repository_unavailable",
                            &format!(
                                "The repository of this host is unavailable: {reason}. Open the repository page to see why."
                            ),
                        )
                        .await?;
                        continue;
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
                    spec.distrusted_shards = distrusted_for_host(state, &host_id).await?;
                    (Some(spec), None)
                }
                Err(_) => {
                    fail_unreadable(state, &job_id, &kind).await?;
                    continue;
                }
            }
        };
        let claimed = async {
            let mut tx = state.db.pool.begin().await?;
            lock_host(&mut tx, &host_id).await?;
            let won = claim_job(&mut tx, &job_id, &host_id, now_ms()).await?;
            tx.commit().await?;
            Ok::<_, sqlx::Error>(won)
        }
        .await;
        match claimed {
            Ok(true) => {}
            Ok(false) => continue,
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
                        match error.downcast_ref::<RepositoryUnavailable>() {
                            Some(unavailable) => {
                                fail_job(
                                    state,
                                    &job_id,
                                    "repository_unavailable",
                                    &format!(
                                        "The source repository is unavailable: {}. Open the repository page to see why.",
                                        unavailable.reason
                                    ),
                                )
                                .await?
                            }
                            None => {
                                fail_job(
                                    state,
                                    &job_id,
                                    "restore_failed",
                                    "The source repository could not be opened.",
                                )
                                .await?
                            }
                        }
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
    let access = server_access_checked(state, &record.source_repository_id).await?;
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
    fail_with(state, job_id, code, message, None).await
}

async fn fail_job_with_detail(
    state: &AppState,
    job_id: &str,
    code: &str,
    message: &str,
    source_type: &str,
) -> anyhow::Result<()> {
    let detail = serde_json::json!({ "source_type": source_type }).to_string();
    fail_with(state, job_id, code, message, Some(detail)).await
}

async fn fail_with(
    state: &AppState,
    job_id: &str,
    code: &str,
    message: &str,
    detail: Option<String>,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE jobs SET state = 'failed', finished_at = $1, error_code = $2, error_message = $3, \
         error_detail_json = $4 WHERE id = $5 AND state IN ('queued', 'assigned')",
    )
    .bind(now_ms())
    .bind(code)
    .bind(message)
    .bind(detail)
    .bind(job_id)
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

fn source_noun(source_type: &str) -> &'static str {
    match source_type {
        "proxmox-vm" => "virtual machines",
        "proxmox-ct" => "containers",
        "postgres" => "PostgreSQL databases",
        _ => "MySQL schemas",
    }
}

fn capability_refusal(
    state: &AppState,
    host_id: &str,
    source: &Source,
) -> Option<(&'static str, String)> {
    (!state
        .registry
        .supports(host_id, &sources::capability(source.type_name())))
    .then(|| {
        let noun = source_noun(source.type_name());
        (
            "source_agent_outdated",
            format!("The agent on this host does not back up {noun} yet. Update the agent."),
        )
    })
}

enum GuestStep {
    /// No list collected since the job was queued yet; try again on the next pass.
    Wait,
    /// The job was failed or cancelled here.
    Done,
    Unreadable,
    Go(SourceInstance),
}

/// A container job is one instance. The job of a selector is expanded into one
/// job per selected container once the node has listed its containers after the
/// job was queued; the keys come from the stored list, never from the agent.
async fn prepare_guest_job(
    state: &AppState,
    job_id: &str,
    host_id: &str,
    source: &Source,
    (source_ref, meta_json): (&str, Option<&str>),
    queued_at: i64,
) -> anyhow::Result<GuestStep> {
    let source_type = source.type_name();
    let fail = |code: &'static str, message: String| async move {
        fail_job_with_detail(state, job_id, code, &message, source_type).await?;
        Ok::<_, anyhow::Error>(GuestStep::Done)
    };
    if let Some((code, message)) = capability_refusal(state, host_id, source) {
        return fail(code, message).await;
    }
    let now = now_ms();
    let instance = !source_ref.is_empty();
    let stored = sources::load(state, host_id).await?;
    let listed = stored.as_ref().filter(|s| {
        is_fresh(s.collected_at, now)
            && (instance || s.collected_at.is_some_and(|at| at >= queued_at))
    });
    if let Some(node) = listed.and_then(|s| s.discovery.proxmox.as_ref())
        && !node.plugin_installed
    {
        let hostname: String = sqlx::query("SELECT hostname FROM agents WHERE id = $1")
            .bind(host_id)
            .fetch_one(&state.db.pool)
            .await?
            .try_get("hostname")?;
        return fail(
            "guest_plugin_missing",
            format!(
                "The Lodestore plugin is missing or was changed on {hostname}. Run lodestore-agent pve install there."
            ),
        )
        .await;
    }
    if instance {
        let vmid = meta_json
            .and_then(|json| {
                serde_json::from_str::<std::collections::BTreeMap<String, String>>(json).ok()
            })
            .and_then(|meta| meta.get("vmid")?.parse().ok());
        return Ok(match vmid {
            Some(vmid) => GuestStep::Go(SourceInstance {
                key: source_ref.to_string(),
                vmid,
            }),
            None => GuestStep::Unreadable,
        });
    }
    match sources::select::select(source, listed, now) {
        Selection::NoSource | Selection::Nothing => {
            fail(
                "source_not_found",
                "No containers were found on this host, or the policy selects none of them."
                    .to_string(),
            )
            .await
        }
        Selection::Unknown if now.saturating_sub(queued_at) > GUEST_FACTS_WAIT_MS => {
            fail(
                "source_unknown",
                "The agent has not listed its containers since the job was queued.".to_string(),
            )
            .await
        }
        Selection::Unknown => Ok(GuestStep::Wait),
        Selection::Some { keys, .. } => {
            let guests: Vec<&Guest> = listed
                .and_then(|s| s.discovery.proxmox.as_ref())
                .into_iter()
                .flat_map(|p| &p.guests)
                .filter(|g| keys.contains(&g.key))
                .collect();
            Ok(match expand_guest_job(state, job_id, &guests).await? {
                Some(instance) => GuestStep::Go(instance),
                None => GuestStep::Done,
            })
        }
    }
}

pub(crate) fn guest_meta(guest: &Guest) -> String {
    let mut meta = std::collections::BTreeMap::new();
    meta.insert("vmid", guest.vmid.to_string());
    if let Some(name) = &guest.name {
        meta.insert("name", name.clone());
    }
    serde_json::to_string(&meta).unwrap_or_default()
}

/// The selector job becomes the job of the first selected container without an
/// active job; the others get queued jobs of their own with the same policy,
/// spec, origin and queue time. `None`: every selected container is already in
/// progress, so the job is cancelled, or it is no longer queued.
async fn expand_guest_job(
    state: &AppState,
    job_id: &str,
    guests: &[&Guest],
) -> anyhow::Result<Option<SourceInstance>> {
    let mut tx = state.db.pool.begin().await?;
    let Some(row) = sqlx::query(
        "SELECT host_id, policy_id, policy_name, spec, queued_at, origin FROM jobs \
         WHERE id = $1 AND state = 'queued'",
    )
    .bind(job_id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(None);
    };
    let host_id: String = row.try_get("host_id")?;
    let policy_id: Option<String> = row.try_get("policy_id")?;
    let mut free = Vec::new();
    for guest in guests {
        let busy = sqlx::query(
            "SELECT 1 FROM jobs WHERE host_id = $1 AND policy_id = $2 AND source_ref = $3 \
             AND state IN ('queued', 'assigned', 'running')",
        )
        .bind(&host_id)
        .bind(&policy_id)
        .bind(&guest.key)
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !busy {
            free.push(*guest);
        }
    }
    let Some((first, rest)) = free.split_first() else {
        sqlx::query(
            "UPDATE jobs SET state = 'cancelled', finished_at = $1, error_code = 'coalesced', \
             error_message = 'Every selected container already has a job in progress.' \
             WHERE id = $2 AND state = 'queued'",
        )
        .bind(now_ms())
        .bind(job_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok(None);
    };
    let spec: Vec<u8> = row.try_get("spec")?;
    let policy_name: Option<String> = row.try_get("policy_name")?;
    let queued_at: i64 = row.try_get("queued_at")?;
    let origin: String = row.try_get("origin")?;
    sqlx::query("UPDATE jobs SET source_ref = $1, source_meta_json = $2 WHERE id = $3")
        .bind(&first.key)
        .bind(guest_meta(first))
        .bind(job_id)
        .execute(&mut *tx)
        .await?;
    for guest in rest {
        insert_guest_job(
            &mut tx,
            (&host_id, &policy_id, &policy_name),
            (&spec, queued_at, &origin),
            guest,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(Some(SourceInstance {
        key: first.key.clone(),
        vmid: first.vmid,
    }))
}

/// The agent answered `guest_not_local`: the container lives on another node.
/// Every other host of the policy whose fresh list shows a container with this
/// VMID gets a job of its own for it, once per run: a host that already has a
/// job for the instance from this run or a later one is skipped, so two agents
/// that disown the container cannot pass it back and forth.
async fn redispatch_guest(state: &AppState, job_id: &str) -> anyhow::Result<()> {
    let Some(row) = sqlx::query(
        "SELECT host_id, policy_id, policy_name, spec, queued_at, origin, source_ref, \
         source_meta_json FROM jobs WHERE id = $1",
    )
    .bind(job_id)
    .fetch_optional(&state.db.pool)
    .await?
    else {
        return Ok(());
    };
    let source_ref: String = row.try_get("source_ref")?;
    let policy_id: Option<String> = row.try_get("policy_id")?;
    let meta: Option<String> = row.try_get("source_meta_json")?;
    let vmid: Option<u32> = meta
        .and_then(|json| {
            serde_json::from_str::<std::collections::BTreeMap<String, String>>(&json).ok()
        })
        .and_then(|meta| meta.get("vmid")?.parse().ok());
    let (Some(policy_id), Some(vmid), false) = (&policy_id, vmid, source_ref.is_empty()) else {
        return Ok(());
    };
    let from_host: String = row.try_get("host_id")?;
    let policy_name: Option<String> = row.try_get("policy_name")?;
    let spec: Vec<u8> = row.try_get("spec")?;
    let queued_at: i64 = row.try_get("queued_at")?;
    let origin: String = row.try_get("origin")?;
    let now = now_ms();
    for host_id in crate::policies::hosts_of_policy(state, policy_id).await? {
        if host_id == from_host {
            continue;
        }
        let Some(stored) = sources::load(state, &host_id).await? else {
            continue;
        };
        let Some(guest) = stored
            .discovery
            .proxmox
            .as_ref()
            .filter(|_| is_fresh(stored.collected_at, now))
            .and_then(|node| {
                node.guests
                    .iter()
                    .find(|g| g.kind == sources::GuestType::Ct && g.vmid == vmid)
            })
        else {
            continue;
        };
        let seen = sqlx::query(
            "SELECT 1 FROM jobs WHERE host_id = $1 AND policy_id = $2 AND source_ref = $3 \
             AND queued_at >= $4",
        )
        .bind(&host_id)
        .bind(policy_id)
        .bind(&guest.key)
        .bind(queued_at)
        .fetch_optional(&state.db.pool)
        .await?
        .is_some();
        if seen {
            continue;
        }
        let mut tx = state.db.pool.begin().await?;
        insert_guest_job(
            &mut tx,
            (&host_id, &Some(policy_id.clone()), &policy_name),
            (&spec, queued_at, &origin),
            guest,
        )
        .await?;
        tx.commit().await?;
    }
    Ok(())
}

pub(crate) async fn insert_guest_job(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    (host_id, policy_id, policy_name): (&str, &Option<String>, &Option<String>),
    (spec, queued_at, origin): (&[u8], i64, &str),
    guest: &Guest,
) -> sqlx::Result<bool> {
    let inserted = sqlx::query(
        "INSERT INTO jobs (id, host_id, policy_id, policy_name, source, state, spec, \
         queued_at, origin, source_ref, source_meta_json) \
         VALUES ($1, $2, $3, $4, 'manual', 'queued', $5, $6, $7, $8, $9) ON CONFLICT DO NOTHING",
    )
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(host_id)
    .bind(policy_id)
    .bind(policy_name)
    .bind(spec.to_vec())
    .bind(queued_at)
    .bind(origin)
    .bind(&guest.key)
    .bind(guest_meta(guest))
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(inserted == 1)
}

/// Why a job for a source that is not files must not reach the agent: the agent
/// cannot back the type up, or a fresh, complete list collected after the job
/// was queued shows nothing to back up. Stale, unknown, partial or older data
/// sends the job on: a guest or database made just before the job may not be
/// listed yet, and the agent checks again.
async fn source_gate(
    state: &AppState,
    host_id: &str,
    source: &Source,
    queued_at: i64,
) -> anyhow::Result<Option<(&'static str, String)>> {
    if let Some(refusal) = capability_refusal(state, host_id, source) {
        return Ok(Some(refusal));
    }
    let noun = source_noun(source.type_name());
    let stored = sources::load(state, host_id)
        .await?
        .filter(|s| s.collected_at.is_some_and(|at| at >= queued_at));
    Ok(
        match sources::select::select(source, stored.as_ref(), now_ms()) {
            Selection::NoSource | Selection::Nothing => Some((
                "source_not_found",
                format!("No {noun} were found on this host, or the policy selects none of them."),
            )),
            Selection::Unknown | Selection::Some { .. } => None,
        },
    )
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
