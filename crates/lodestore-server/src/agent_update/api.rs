use std::collections::{HashMap, HashSet};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use semver::Version;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::{manifest, rollout};
use crate::auth::tokens::{AgentsUpdate, HostsRead, Scoped};
use crate::db::now_ms;
use crate::error::{ApiError, ErrorBody};
use crate::grpc::registry::CAP_UPDATE;
use crate::state::AppState;

pub const ROLLBACK_AFTER_SECS: u64 = 120;
const FINISHED_VISIBLE_MS: i64 = 86_400_000;
const ROLLBACKS_SHOWN: i64 = 5;
const ALREADY_RUNNING: &str = "An update is already running.";

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AgentBinaryDto {
    pub arch: String,
    pub size: u64,
    pub sha256: Option<String>,
    /// The version in the signature; empty when the file has no readable one.
    pub version: Option<String>,
    pub signed: bool,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RolloutSettings {
    pub canary: u32,
    pub wave: u32,
    pub pause_after: u32,
    /// How long a switched agent has to come back before it rolls itself back.
    pub rollback_after_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostUpdateBlocker {
    /// The agent does not support self-update; it moves once through `install.sh`.
    NoSelfUpdate,
    /// The server's agent directory holds no readable signature.
    Unsigned,
    /// No signed binary of the target version for the host's architecture.
    NoBinary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostUpdateState {
    Queued,
    WaitingForJob,
    OfflineQueued,
    Updating,
    RolledBack,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct HostUpdate {
    pub id: String,
    pub hostname: String,
    pub version: String,
    pub arch: String,
    pub outdated: bool,
    pub updatable: bool,
    /// Why an outdated host cannot be updated; empty otherwise.
    pub blocker: Option<HostUpdateBlocker>,
    pub state: Option<HostUpdateState>,
    pub reason: Option<String>,
    pub update_id: Option<String>,
    /// The version this host's update goes to; a rollout keeps it when a newer build appears.
    pub target_version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RolloutState {
    Running,
    Paused,
    Done,
    Cancelled,
}

impl RolloutState {
    fn parse(text: &str) -> anyhow::Result<Self> {
        Ok(match text {
            "running" => RolloutState::Running,
            "paused" => RolloutState::Paused,
            "done" => RolloutState::Done,
            "cancelled" => RolloutState::Cancelled,
            other => anyhow::bail!("unknown rollout state {other}"),
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct RolloutWave {
    pub wave: u32,
    pub total: u64,
    pub updated: u64,
    pub rolled_back: u64,
    pub waiting: u64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RolloutRollback {
    pub update_id: String,
    pub host_id: String,
    pub hostname: String,
    pub reason: Option<String>,
    pub finished_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Rollout {
    pub id: String,
    pub target_version: String,
    pub state: RolloutState,
    pub canary: u32,
    pub wave: u32,
    pub pause_after: u32,
    pub created_by: String,
    pub created_at: i64,
    pub finished_at: Option<i64>,
    /// Why the server paused it, when something other than rollbacks did.
    pub pause_reason: Option<String>,
    pub total: u64,
    pub updated: u64,
    pub rolled_back: u64,
    /// Rollbacks that count towards `pause_after`.
    pub rollbacks_since_resume: u64,
    pub waves: Vec<RolloutWave>,
    /// The latest rollbacks, newest first.
    pub rollbacks: Vec<RolloutRollback>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AgentUpdateStatus {
    pub agent_dir: String,
    /// The highest signed version in the agent directory.
    pub target_version: Option<String>,
    pub signed: bool,
    pub binaries: Vec<AgentBinaryDto>,
    pub settings: RolloutSettings,
    pub hosts: Vec<HostUpdate>,
    /// The running or paused rollout, or one that finished in the last 24 hours and was not dismissed.
    pub rollout: Option<Rollout>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct StartRollout {
    /// Update these hosts.
    pub hosts: Option<Vec<String>>,
    /// Update every outdated host that can be updated.
    pub outdated: Option<bool>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RolloutStarted {
    pub id: String,
    /// Hosts queued for the update.
    pub hosts: u64,
    /// Hosts left out: current, revoked, not able to update or already in an update.
    pub skipped: u64,
}

struct Assessment {
    outdated: bool,
    updatable: bool,
    blocker: Option<HostUpdateBlocker>,
}

fn assess(
    version: &str,
    arch: &str,
    self_update: bool,
    target: Option<&Version>,
    binaries: &[manifest::AgentBinary],
) -> Assessment {
    let outdated = match target {
        Some(target) => !Version::parse(version).is_ok_and(|v| v >= *target),
        None => !super::reached(version, env!("CARGO_PKG_VERSION")),
    };
    let has_binary = target.is_some_and(|t| manifest::binary_for(binaries, arch, t).is_some());
    let updatable = self_update && has_binary;
    let blocker = (outdated && !updatable).then(|| {
        if !self_update {
            HostUpdateBlocker::NoSelfUpdate
        } else if target.is_none() {
            HostUpdateBlocker::Unsigned
        } else {
            HostUpdateBlocker::NoBinary
        }
    });
    Assessment {
        outdated,
        updatable,
        blocker,
    }
}

struct AgentRow {
    id: String,
    hostname: String,
    arch: String,
    version: String,
    revoked: bool,
    self_update: bool,
}

async fn load_agents(state: &AppState) -> Result<Vec<AgentRow>, ApiError> {
    let rows = sqlx::query(
        "SELECT id, hostname, arch, version, status, capabilities_json FROM agents \
         ORDER BY hostname, id",
    )
    .fetch_all(&state.db.pool)
    .await?;
    rows.iter()
        .map(|row| {
            let caps: Vec<String> = row
                .try_get::<Option<String>, _>("capabilities_json")?
                .and_then(|text| serde_json::from_str(&text).ok())
                .unwrap_or_default();
            Ok(AgentRow {
                id: row.try_get("id")?,
                hostname: row.try_get("hostname")?,
                arch: row.try_get("arch")?,
                version: row.try_get("version")?,
                revoked: row.try_get::<String, _>("status")? == "revoked",
                self_update: caps.iter().any(|c| c == CAP_UPDATE),
            })
        })
        .collect()
}

fn count(n: i64) -> u64 {
    n.max(0) as u64
}

async fn load_rollout(state: &AppState, now: i64) -> Result<Option<Rollout>, ApiError> {
    let Some(row) = sqlx::query(
        "SELECT id, target_version, state, canary, wave_size, pause_after, created_by, \
         created_at, finished_at, pause_reason, pause_baseline FROM agent_rollouts \
         WHERE state IN ('running', 'paused') \
         OR (dismissed_at IS NULL AND finished_at > $1) \
         ORDER BY CASE WHEN state IN ('running', 'paused') THEN 0 ELSE 1 END, created_at DESC \
         LIMIT 1",
    )
    .bind(now - FINISHED_VISIBLE_MS)
    .fetch_optional(&state.db.pool)
    .await?
    else {
        return Ok(None);
    };
    let id: String = row.try_get("id")?;
    let rollout_state = RolloutState::parse(&row.try_get::<String, _>("state")?)?;
    let mut waves: HashMap<u32, RolloutWave> = HashMap::new();
    let counts = sqlx::query(
        "SELECT wave, state, COUNT(*) AS n FROM agent_updates WHERE rollout_id = $1 \
         GROUP BY wave, state",
    )
    .bind(&id)
    .fetch_all(&state.db.pool)
    .await?;
    for row in &counts {
        let wave = row.try_get::<i64, _>("wave")?.max(0) as u32;
        let n = count(row.try_get("n")?);
        let entry = waves.entry(wave).or_insert_with(|| RolloutWave {
            wave,
            ..RolloutWave::default()
        });
        entry.total += n;
        match row.try_get::<String, _>("state")?.as_str() {
            "succeeded" => entry.updated += n,
            "rolled_back" => entry.rolled_back += n,
            "queued" | "assigned" => entry.waiting += n,
            _ => {}
        }
    }
    let mut waves: Vec<RolloutWave> = waves.into_values().collect();
    waves.sort_by_key(|w| w.wave);
    let rollbacks = sqlx::query(
        "SELECT u.id, u.host_id, a.hostname, u.reason, u.finished_at FROM agent_updates u \
         JOIN agents a ON a.id = u.host_id WHERE u.rollout_id = $1 AND u.state = 'rolled_back' \
         ORDER BY u.finished_at DESC, u.id LIMIT $2",
    )
    .bind(&id)
    .bind(ROLLBACKS_SHOWN)
    .fetch_all(&state.db.pool)
    .await?
    .iter()
    .map(|row| {
        Ok(RolloutRollback {
            update_id: row.try_get("id")?,
            host_id: row.try_get("host_id")?,
            hostname: row.try_get("hostname")?,
            reason: row.try_get("reason")?,
            finished_at: row.try_get("finished_at")?,
        })
    })
    .collect::<Result<Vec<_>, sqlx::Error>>()?;
    let rolled_back = waves.iter().map(|w| w.rolled_back).sum::<u64>();
    Ok(Some(Rollout {
        id,
        target_version: row.try_get("target_version")?,
        state: rollout_state,
        canary: count(row.try_get("canary")?) as u32,
        wave: count(row.try_get("wave_size")?) as u32,
        pause_after: count(row.try_get("pause_after")?) as u32,
        created_by: row.try_get("created_by")?,
        created_at: row.try_get("created_at")?,
        finished_at: row.try_get("finished_at")?,
        pause_reason: row
            .try_get::<Option<String>, _>("pause_reason")?
            .filter(|_| rollout_state == RolloutState::Paused),
        total: waves.iter().map(|w| w.total).sum(),
        updated: waves.iter().map(|w| w.updated).sum(),
        rolled_back,
        rollbacks_since_resume: rolled_back.saturating_sub(count(row.try_get("pause_baseline")?)),
        waves,
        rollbacks,
    }))
}

async fn host_states(
    state: &AppState,
) -> Result<HashMap<String, (HostUpdateState, Option<String>, String, String)>, ApiError> {
    let busy: HashSet<String> =
        sqlx::query("SELECT DISTINCT host_id FROM jobs WHERE state IN ('assigned', 'running')")
            .fetch_all(&state.db.pool)
            .await?
            .iter()
            .map(|row| row.try_get("host_id"))
            .collect::<Result<_, _>>()?;
    let rows = sqlx::query(
        "SELECT u.id, u.host_id, u.state, u.reason, u.target_version FROM agent_updates u \
         WHERE u.state IN ('queued', 'assigned') OR (u.state = 'rolled_back' AND NOT EXISTS \
         (SELECT 1 FROM agent_updates n WHERE n.host_id = u.host_id AND n.queued_at > u.queued_at)) \
         ORDER BY u.queued_at",
    )
    .fetch_all(&state.db.pool)
    .await?;
    let mut states = HashMap::new();
    for row in &rows {
        let host: String = row.try_get("host_id")?;
        let target: String = row.try_get("target_version")?;
        let update_id: String = row.try_get("id")?;
        let (host_state, reason) = match row.try_get::<String, _>("state")?.as_str() {
            "assigned" => (HostUpdateState::Updating, None),
            "rolled_back" => (HostUpdateState::RolledBack, row.try_get("reason")?),
            _ if !state.registry.is_online(&host) => (HostUpdateState::OfflineQueued, None),
            _ if busy.contains(&host) => (HostUpdateState::WaitingForJob, None),
            _ => (HostUpdateState::Queued, None),
        };
        states.insert(host, (host_state, reason, update_id, target));
    }
    Ok(states)
}

#[utoipa::path(
    get,
    tag = "Hosts",
    operation_id = "agent_update_status",
    summary = "Show which agents are outdated and how an update is going",
    description = "The signed agent versions on this server, the version, architecture and \
        update state of every host, and the current or just finished rollout with its waves \
        and latest rollbacks.",
    security(("session" = []), ("bearer" = ["hosts:read"])),
    path = "/api/v1/agent-updates",
    responses((status = 200, description = "The update status", body = AgentUpdateStatus))
)]
pub async fn agent_update_status(
    State(state): State<AppState>,
    _auth: Scoped<HostsRead>,
) -> Result<Json<AgentUpdateStatus>, ApiError> {
    let now = now_ms();
    let binaries = manifest::binaries(&state.config).await;
    let target = manifest::target_version(&binaries);
    let states = host_states(&state).await?;
    let hosts = load_agents(&state)
        .await?
        .into_iter()
        .filter(|a| !a.revoked)
        .map(|a| {
            let verdict = assess(
                &a.version,
                &a.arch,
                a.self_update,
                target.as_ref(),
                &binaries,
            );
            let current = states.get(&a.id).filter(|(s, _, _, to)| {
                *s != HostUpdateState::RolledBack || !super::reached(&a.version, to)
            });
            HostUpdate {
                outdated: verdict.outdated,
                updatable: verdict.updatable,
                blocker: verdict.blocker,
                state: current.map(|(s, ..)| *s),
                reason: current.and_then(|(_, reason, ..)| reason.clone()),
                update_id: current.map(|(_, _, id, _)| id.clone()),
                target_version: current.map(|(.., to)| to.clone()),
                id: a.id,
                hostname: a.hostname,
                version: a.version,
                arch: a.arch,
            }
        })
        .collect();
    let updates = &state.config.agent_updates;
    Ok(Json(AgentUpdateStatus {
        agent_dir: state.config.agent_dir.display().to_string(),
        target_version: target.as_ref().map(Version::to_string),
        signed: target.is_some(),
        binaries: binaries
            .iter()
            .map(|b| AgentBinaryDto {
                arch: b.arch.to_string(),
                size: b.size,
                sha256: b.sha256.clone(),
                version: b.version.as_ref().map(Version::to_string),
                signed: b.signed,
            })
            .collect(),
        settings: RolloutSettings {
            canary: updates.canary,
            wave: updates.wave,
            pause_after: updates.pause_after,
            rollback_after_secs: ROLLBACK_AFTER_SECS,
        },
        hosts,
        rollout: load_rollout(&state, now).await?,
    }))
}

#[utoipa::path(
    post,
    tag = "Hosts",
    operation_id = "start_rollout",
    summary = "Start updating agents",
    description = "Give `hosts` to update those hosts or `outdated: true` to update every outdated \
        host. Only hosts that are behind, not revoked, support self-update and have a signed \
        binary of the target version for their architecture are queued; the rest are counted \
        as skipped. The first hosts form the canary wave, connected hosts first, and the \
        others follow in waves. The token needs the `agents:update` scope; `hosts:write` is not enough.",
    security(("session" = [], "csrf" = []), ("bearer" = ["agents:update"])),
    path = "/api/v1/agent-updates",
    request_body = StartRollout,
    responses(
        (status = 202, description = "The update started", body = RolloutStarted),
        (status = 403, description = "The token lacks agents:update, or the session has not entered its password in the last 15 minutes (code step_up_required)", body = ErrorBody),
        (status = 409, description = "Another update is running or paused", body = ErrorBody),
        (status = 422, description = "No signed target, no host to update or a bad request", body = ErrorBody),
    )
)]
pub async fn start_rollout(
    State(state): State<AppState>,
    auth: Scoped<AgentsUpdate>,
    Json(body): Json<StartRollout>,
) -> Result<(StatusCode, Json<RolloutStarted>), ApiError> {
    auth.require_step_up()?;
    let selected: Option<HashSet<String>> = match (body.hosts, body.outdated == Some(true)) {
        (Some(hosts), false) if !hosts.is_empty() => Some(hosts.into_iter().collect()),
        (None, true) => None,
        _ => {
            return Err(ApiError::Validation(
                "Give either a list of hosts or outdated: true.".into(),
            ));
        }
    };
    let active = sqlx::query("SELECT 1 FROM agent_rollouts WHERE state IN ('running', 'paused')")
        .fetch_optional(&state.db.pool)
        .await?;
    if active.is_some() {
        return Err(ApiError::Conflict(ALREADY_RUNNING.into()));
    }
    let binaries = manifest::binaries(&state.config).await;
    let target = manifest::target_version(&binaries).ok_or_else(|| {
        ApiError::Validation("There is no signed agent on this server to update to.".into())
    })?;
    let agents = load_agents(&state).await?;
    let considered = match &selected {
        Some(ids) => ids.len(),
        None => agents.len(),
    };
    let busy: HashSet<String> =
        sqlx::query("SELECT host_id FROM agent_updates WHERE state IN ('queued', 'assigned')")
            .fetch_all(&state.db.pool)
            .await?
            .iter()
            .map(|row| row.try_get("host_id"))
            .collect::<Result<_, _>>()?;
    let mut candidates: Vec<AgentRow> = agents
        .into_iter()
        .filter(|a| selected.as_ref().is_none_or(|ids| ids.contains(&a.id)))
        .filter(|a| !a.revoked && !busy.contains(&a.id))
        .filter(|a| {
            let verdict = assess(&a.version, &a.arch, a.self_update, Some(&target), &binaries);
            verdict.outdated && verdict.updatable
        })
        .collect();
    // Canaries gate the later waves, so they should be hosts that can act now.
    candidates.sort_by_key(|a| !rollout::ready_for_update(&state, &a.id));
    if candidates.is_empty() {
        return Err(ApiError::Validation(
            "No host can be updated: they are current, revoked, already updating or cannot update themselves."
                .into(),
        ));
    }

    let settings = &state.config.agent_updates;
    let now = now_ms();
    let rollout_id = uuid::Uuid::now_v7().to_string();
    let mut tx = state.db.pool.begin().await?;
    let created = sqlx::query(
        "INSERT INTO agent_rollouts (id, target_version, state, canary, wave_size, pause_after, \
         created_by, created_at) VALUES ($1, $2, 'running', $3, $4, $5, $6, $7) \
         ON CONFLICT DO NOTHING",
    )
    .bind(&rollout_id)
    .bind(target.to_string())
    .bind(i64::from(settings.canary))
    .bind(i64::from(settings.wave))
    .bind(i64::from(settings.pause_after))
    .bind(auth.actor())
    .bind(now)
    .execute(&mut *tx)
    .await?;
    if created.rows_affected() == 0 {
        return Err(ApiError::Conflict(ALREADY_RUNNING.into()));
    }
    sqlx::query(
        "UPDATE agent_rollouts SET dismissed_at = $1 \
         WHERE dismissed_at IS NULL AND state IN ('done', 'cancelled')",
    )
    .bind(now)
    .execute(&mut *tx)
    .await?;
    let canary = settings.canary as usize;
    let mut queued = 0u64;
    for (index, agent) in candidates.iter().enumerate() {
        let wave = if index < canary {
            0
        } else {
            1 + (index - canary) / settings.wave as usize
        };
        let inserted = sqlx::query(
            "INSERT INTO agent_updates (id, rollout_id, host_id, arch, from_version, \
             target_version, wave, state, queued_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'queued', $8) ON CONFLICT DO NOTHING",
        )
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(&rollout_id)
        .bind(&agent.id)
        .bind(&agent.arch)
        .bind(&agent.version)
        .bind(target.to_string())
        .bind(wave as i64)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        queued += inserted.rows_affected();
    }
    if queued == 0 {
        return Err(ApiError::Validation(
            "No host can be updated: they are already updating.".into(),
        ));
    }
    tx.commit().await?;
    rollout::kick(&state);
    Ok((
        StatusCode::ACCEPTED,
        Json(RolloutStarted {
            id: rollout_id,
            hosts: queued,
            skipped: (considered as u64).saturating_sub(queued),
        }),
    ))
}

async fn missing_or_wrong_state(
    state: &AppState,
    id: &str,
    verb: &str,
) -> Result<ApiError, ApiError> {
    let current: Option<String> = sqlx::query("SELECT state FROM agent_rollouts WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db.pool)
        .await?
        .map(|row| row.try_get("state"))
        .transpose()?;
    Ok(match current {
        None => ApiError::NotFound("Update not found.".into()),
        Some(current) => {
            ApiError::Conflict(format!("The update is {current}; it can't be {verb}."))
        }
    })
}

async fn move_rollout(
    state: &AppState,
    id: &str,
    verb: &str,
    sql: &'static str,
    bind_now: bool,
) -> Result<(), ApiError> {
    let mut query = sqlx::query(sql);
    if bind_now {
        query = query.bind(now_ms());
    }
    if query
        .bind(id)
        .execute(&state.db.pool)
        .await?
        .rows_affected()
        == 0
    {
        return Err(missing_or_wrong_state(state, id, verb).await?);
    }
    Ok(())
}

#[utoipa::path(
    post,
    tag = "Hosts",
    operation_id = "pause_rollout",
    summary = "Pause an update",
    description = "Nothing new is sent to hosts while it is paused. Updates already sent finish.",
    security(("session" = [], "csrf" = []), ("bearer" = ["agents:update"])),
    path = "/api/v1/agent-updates/{id}/pause",
    params(("id" = String, Path, description = "Update id")),
    responses(
        (status = 204, description = "The update is paused"),
        (status = 404, description = "Unknown update", body = ErrorBody),
        (status = 409, description = "The update is not running", body = ErrorBody),
    )
)]
pub async fn pause_rollout(
    State(state): State<AppState>,
    _auth: Scoped<AgentsUpdate>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    move_rollout(
        &state,
        &id,
        "paused",
        "UPDATE agent_rollouts SET state = 'paused' WHERE id = $1 AND state = 'running'",
        false,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    tag = "Hosts",
    operation_id = "resume_rollout",
    summary = "Continue a paused update",
    description = "Rollbacks so far no longer count towards the pause threshold. When the \
        update paused because no canary host updated, the next waves start without one.",
    security(("session" = [], "csrf" = []), ("bearer" = ["agents:update"])),
    path = "/api/v1/agent-updates/{id}/resume",
    params(("id" = String, Path, description = "Update id")),
    responses(
        (status = 204, description = "The update continues"),
        (status = 403, description = "The token lacks agents:update, or the session has not entered its password in the last 15 minutes (code step_up_required)", body = ErrorBody),
        (status = 404, description = "Unknown update", body = ErrorBody),
        (status = 409, description = "The update is not paused", body = ErrorBody),
    )
)]
pub async fn resume_rollout(
    State(state): State<AppState>,
    auth: Scoped<AgentsUpdate>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    auth.require_step_up()?;
    let resumed = sqlx::query(
        "UPDATE agent_rollouts SET state = 'running', pause_reason = NULL, \
         canary_waived = CASE WHEN pause_reason = $1 THEN 1 ELSE canary_waived END, \
         pause_baseline = (SELECT COUNT(*) FROM agent_updates \
         WHERE rollout_id = $2 AND state = 'rolled_back'), resumed_at = $3 \
         WHERE id = $4 AND state = 'paused'",
    )
    .bind(rollout::NO_CANARY)
    .bind(&id)
    .bind(now_ms())
    .bind(&id)
    .execute(&state.db.pool)
    .await?;
    if resumed.rows_affected() == 0 {
        return Err(missing_or_wrong_state(&state, &id, "resumed").await?);
    }
    rollout::kick(&state);
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    tag = "Hosts",
    operation_id = "cancel_rollout",
    summary = "Cancel an update",
    description = "Hosts still waiting are dropped. Hosts that already got the update finish it.",
    security(("session" = [], "csrf" = []), ("bearer" = ["agents:update"])),
    path = "/api/v1/agent-updates/{id}/cancel",
    params(("id" = String, Path, description = "Update id")),
    responses(
        (status = 204, description = "The update is cancelled"),
        (status = 404, description = "Unknown update", body = ErrorBody),
        (status = 409, description = "The update is already over", body = ErrorBody),
    )
)]
pub async fn cancel_rollout(
    State(state): State<AppState>,
    _auth: Scoped<AgentsUpdate>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let now = now_ms();
    let mut tx = state.db.pool.begin().await?;
    let cancelled = sqlx::query(
        "UPDATE agent_rollouts SET state = 'cancelled', finished_at = $1 \
         WHERE id = $2 AND state IN ('running', 'paused')",
    )
    .bind(now)
    .bind(&id)
    .execute(&mut *tx)
    .await?;
    if cancelled.rows_affected() == 0 {
        drop(tx);
        return Err(missing_or_wrong_state(&state, &id, "cancelled").await?);
    }
    sqlx::query(
        "UPDATE agent_updates SET state = 'cancelled', reason = 'The update was cancelled.', \
         finished_at = $1 WHERE rollout_id = $2 AND state = 'queued'",
    )
    .bind(now)
    .bind(&id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    tag = "Hosts",
    operation_id = "dismiss_rollout",
    summary = "Hide a finished update",
    description = "A finished update shows for a day; dismissing it hides it at once.",
    security(("session" = [], "csrf" = []), ("bearer" = ["agents:update"])),
    path = "/api/v1/agent-updates/{id}/dismiss",
    params(("id" = String, Path, description = "Update id")),
    responses(
        (status = 204, description = "The update is hidden"),
        (status = 404, description = "Unknown update", body = ErrorBody),
        (status = 409, description = "The update is still going", body = ErrorBody),
    )
)]
pub async fn dismiss_rollout(
    State(state): State<AppState>,
    _auth: Scoped<AgentsUpdate>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    move_rollout(
        &state,
        &id,
        "dismissed",
        "UPDATE agent_rollouts SET dismissed_at = $1 WHERE id = $2 AND state IN ('done', 'cancelled')",
        true,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(agent_update_status, start_rollout))
        .routes(routes!(pause_rollout))
        .routes(routes!(resume_rollout))
        .routes(routes!(cancel_rollout))
        .routes(routes!(dismiss_rollout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assess_without_a_target_compares_versions() {
        let server: Version = env!("CARGO_PKG_VERSION").parse().unwrap();
        let outdated = |v: &str| assess(v, "x86_64", true, None, &[]).outdated;
        assert!(!outdated(&server.to_string()));
        assert!(!outdated(&Version::new(server.major + 1, 0, 0).to_string()));
        let lowest_pre = format!("{}.{}.{}-0", server.major, server.minor, server.patch);
        assert!(outdated(&lowest_pre));
        assert!(outdated("not a version"));
    }
}
