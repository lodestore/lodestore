use axum::Json;
use axum::extract::{Path, State};
use lodestore_format::store::SftpStore;
use lodestore_format::store::probe::{CheckId, CheckStatus, GuaranteeLevel, ProbeReport};
use lodestore_proto::agent::v1::server_message::Body;
use lodestore_proto::agent::v1::{S3Credentials, ServerMessage, UpdateS3Credentials};
use serde::{Deserialize, Serialize};
use tracing::Instrument;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::AuthUser;
use crate::db::now_ms;
use crate::error::{ApiError, ErrorBody};
use crate::grpc::agent::{CAP_HOST_KEY, CAP_LOCATION, host_key_message, location_message};
use crate::repos::checks::{
    PROBE_TIMEOUT, agent_key_state_name, cap_exposed_neighbours, check_agent_options, invalid,
    open_parts, open_parts_with, run_probe, run_probe_in, to_result,
};
use crate::repos::db::{self, AccessKey, AgentOptions, ServerAccess};
use crate::repos::{CheckDto, CheckResult, Credentials, unlock_with_server_slot};
use crate::state::AppState;

#[derive(Serialize, ToSchema)]
pub struct KeyVerification {
    pub agent_key_state: String,
    pub check: CheckDto,
}

#[derive(Serialize, ToSchema)]
pub struct GuaranteeState {
    /// The level the last check measured.
    pub guarantee_level: String,
    /// The level the user accepted; it rises on its own only when a check measures a higher one.
    pub guarantee_accepted: String,
    pub guarantee_checked_at: Option<i64>,
}

#[derive(Deserialize, ToSchema)]
pub struct AcceptGuarantee {
    /// The measured level the user was shown; the accept is refused when the
    /// stored level is another one by now.
    pub expected_level: String,
}

#[derive(Serialize, ToSchema)]
pub struct KeyReplaced {
    pub agent_key_state: String,
    pub online_notified: u32,
    pub offline: u32,
}

#[derive(Serialize, ToSchema)]
pub struct HostKeyOffer {
    /// The host key the server presents now, one OpenSSH public key line.
    pub key: String,
    pub fingerprint: String,
    /// The fingerprint of the pinned key, `unknown` when it cannot be read.
    pub pinned_fingerprint: String,
    pub matches: bool,
}

#[derive(Deserialize, ToSchema)]
pub struct HostKeyUpdate {
    /// The new host key, one OpenSSH public key line.
    pub host_key: String,
}

#[derive(Serialize, ToSchema)]
pub struct HostKeyReplaced {
    pub fingerprint: String,
    /// Online agents that were sent the key.
    pub online_notified: u32,
    /// Online agents too old to take the key; update them.
    pub needs_update: u32,
    /// Active agents that are not online and get the key when they connect.
    pub offline: u32,
}

#[derive(Deserialize, ToSchema)]
pub struct AgentOptionsUpdate {
    /// The whole new set of the agents' own credentials; it replaces the old set.
    /// Azure Blob: `sas_token`; GCS: `credential`; SFTP: `user`, `key`. Never returned.
    pub agent_options: std::collections::BTreeMap<String, String>,
}

impl std::fmt::Debug for AgentOptionsUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentOptionsUpdate").finish_non_exhaustive()
    }
}

#[derive(Serialize, ToSchema)]
pub struct AgentOptionsReplaced {
    pub agent_key_state: String,
    pub guarantee_level: String,
    /// Online agents that were sent the new location.
    pub online_notified: u32,
    /// Online agents too old to take it; update them.
    pub needs_update: u32,
    /// Active agents that are not online and get it when they connect.
    pub offline: u32,
}

#[derive(Serialize, ToSchema)]
pub struct StorageAccess {
    /// What happens to the access of revoked hosts: `expires` (the issued SAS runs out),
    /// `until_rotation` (until the agent credentials are replaced and the old ones are
    /// disabled at the provider), `new_repository` (shared credentials; only a new
    /// repository and new credentials at the provider close it), `server_key_shared`
    /// (the agents hold the server's own key) or `outside_lodestore` (a local path).
    pub after_revoke: String,
    /// `no_revoked_hosts`, `revoked_hosts_hold_access`, `rotated_disable_old_key_at_provider`,
    /// `access_expires` or `access_expired`. Replacing credentials in Lodestore stops handing
    /// the old ones out; it does not disable them at the provider.
    pub status: String,
    /// Hosts revoked after the credentials were last replaced (all of them when never replaced).
    pub revoked_hosts_since_rotation: u32,
    /// Hosts revoked before the last replacement; the old credentials still work at the
    /// provider until they are disabled there.
    pub revoked_hosts_before_rotation: u32,
    /// Latest expiry of a SAS issued to a revoked host, unix ms; only while it is in the future.
    pub access_ends_by: Option<i64>,
    /// The endpoint that replaces the agents' credentials: `agent-key` or `agent-options`.
    pub rotate_via: Option<String>,
    /// Active hosts that cannot refresh a SAS; update them (SAS-issuing repositories only).
    pub hosts_need_agent_update: u32,
}

pub fn storage_access_of(f: &db::StorageAccessFacts, now_ms: i64) -> StorageAccess {
    let split = |hold_all: bool| {
        let changed = f.changed_at.filter(|_| !hold_all).unwrap_or(i64::MIN);
        let since = f.revoked.iter().filter(|(at, _)| *at > changed).count() as u32;
        (since, f.revoked.len() as u32 - since)
    };
    let (after_revoke, rotate_via, hold_all) = match f.backend.as_str() {
        "local" => ("outside_lodestore", None, true),
        "s3" if f.agent_key_is_server_key => ("server_key_shared", None, true),
        "s3" => ("until_rotation", Some("agent-key"), false),
        _ if f.sas_vending => ("expires", None, true),
        _ if f.has_agent_creds => ("until_rotation", Some("agent-options"), false),
        _ => ("new_repository", None, true),
    };
    let mut out = StorageAccess {
        after_revoke: after_revoke.into(),
        status: "no_revoked_hosts".into(),
        revoked_hosts_since_rotation: 0,
        revoked_hosts_before_rotation: 0,
        access_ends_by: None,
        rotate_via: rotate_via.map(Into::into),
        hosts_need_agent_update: f.outdated,
    };
    if f.sas_vending && after_revoke == "expires" {
        let issued: Vec<i64> = f.revoked.iter().filter_map(|(_, se)| *se).collect();
        let live = issued.iter().filter(|se| **se > now_ms).count() as u32;
        if live > 0 {
            out.status = "access_expires".into();
            out.revoked_hosts_since_rotation = live;
            out.access_ends_by = issued.into_iter().max();
        } else if !issued.is_empty() {
            out.status = "access_expired".into();
        }
        return out;
    }
    let (since, before) = split(hold_all);
    out.revoked_hosts_since_rotation = since;
    out.revoked_hosts_before_rotation = before;
    if since > 0 {
        out.status = "revoked_hosts_hold_access".into();
    } else if before > 0 {
        out.status = "rotated_disable_old_key_at_provider".into();
    }
    out
}

#[utoipa::path(
    get,
    tag = "Repositories",
    summary = "Storage access of revoked hosts",
    description = "Tells whether revoked hosts of the repository still hold access to its storage \
        and what to do about it. `after_revoke` is `expires`, `until_rotation`, `new_repository`, \
        `server_key_shared` or `outside_lodestore`. `status` is `no_revoked_hosts`, \
        `revoked_hosts_hold_access`, `rotated_disable_old_key_at_provider`, `access_expires` or \
        `access_expired`. `rotate_via` names the endpoint that replaces the agents' credentials: \
        `agent-key` for S3 repositories, `agent-options` for the other backends with their own \
        agent credentials. Replacing credentials in Lodestore stops handing out the old ones; the \
        old key, service account key, SFTP key or static SAS keeps working at the provider until \
        it is disabled there.",
    security(("session" = [])),
    path = "/api/v1/repositories/{id}/storage-access",
    params(("id" = String, Path, description = "Repository id")),
    responses(
        (status = 200, description = "What revoked hosts of this repository can still reach", body = StorageAccess),
        (status = 404, description = "Unknown repository", body = ErrorBody),
    )
)]
async fn storage_access(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<StorageAccess>, ApiError> {
    let facts = db::storage_access_facts(&state, &id)
        .await?
        .ok_or_else(|| ApiError::NotFound("repository not found".into()))?;
    Ok(Json(storage_access_of(&facts, now_ms())))
}

async fn load(state: &AppState, id: &str) -> Result<ServerAccess, ApiError> {
    if db::find_repository(state, id).await?.is_none() {
        return Err(ApiError::NotFound("repository not found".into()));
    }
    Ok(db::server_access(state, id).await?)
}

fn require_s3(access: &ServerAccess) -> Result<(), ApiError> {
    if access.location.backend == "s3" {
        Ok(())
    } else {
        Err(ApiError::Conflict(
            "Only S3 repositories have a separate agent key.".into(),
        ))
    }
}

fn require_sftp(access: &ServerAccess) -> Result<(), ApiError> {
    if access.location.backend == "sftp" {
        Ok(())
    } else {
        Err(ApiError::Conflict(
            "Only SFTP repositories have a host key.".into(),
        ))
    }
}

/// Whether the repository's config no longer matches the recorded one, so
/// nothing is sent to its agents. Read before taking the access lock: it
/// reads the storage, and a slow storage must not stall starting sessions.
fn unavailable(opened: anyhow::Result<crate::catalog::OpenRepository>) -> bool {
    matches!(
        opened,
        Err(error) if error.downcast_ref::<crate::catalog::RepositoryUnavailable>().is_some()
    )
}

fn agent_check(report: &ProbeReport) -> CheckDto {
    to_result(report)
        .checks
        .into_iter()
        .find(|c| c.id == CheckId::AgentDelete.as_str())
        .expect("every report has an agent_delete row")
}

fn rejection(report: &ProbeReport) -> Option<String> {
    let check = report.check(CheckId::AgentDelete);
    (check.status == CheckStatus::Fail).then(|| {
        check
            .error
            .clone()
            .unwrap_or_else(|| "it cannot write".into())
    })
}

fn server_key_problem(report: &ProbeReport) -> Option<ApiError> {
    let check = report.check(CheckId::Write);
    (check.status == CheckStatus::Fail).then(|| {
        ApiError::Validation(format!(
            "The server key cannot write to the storage ({}), so the new agent key could not be checked. Fix the server key and try again. The current agent key is unchanged.",
            check.error.as_deref().unwrap_or("unknown error")
        ))
    })
}

/// The accepted level rises when a check measures a higher one and never falls
/// on its own; a weak level is never accepted from `unchecked` (D-01, D-09).
fn next_accepted(accepted: GuaranteeLevel, measured: GuaranteeLevel) -> GuaranteeLevel {
    match (accepted.rank(), measured.rank()) {
        (None, Some(m)) if m > 0 => measured,
        (Some(a), Some(m)) if m > a => measured,
        _ => accepted,
    }
}

/// What to store after a check. A level that could not be measured changes
/// nothing, except right after the credentials changed: the stored level was
/// measured with the old key, so it becomes `unchecked`.
fn guarantee_write(
    result: &CheckResult,
    accepted: &str,
    credentials_changed: bool,
) -> Result<Option<db::GuaranteeWrite>, ApiError> {
    let measured =
        GuaranteeLevel::parse(&result.guarantee_level).unwrap_or(GuaranteeLevel::Unchecked);
    let read_accepted = accepted.to_string();
    let accepted = GuaranteeLevel::parse(accepted).unwrap_or(GuaranteeLevel::Unchecked);
    if measured == GuaranteeLevel::Unchecked {
        return Ok(credentials_changed.then(|| db::GuaranteeWrite {
            level: measured.as_str().to_string(),
            notes_json: "[]".to_string(),
            accepted: accepted.as_str().to_string(),
            read_accepted,
        }));
    }
    Ok(Some(db::GuaranteeWrite {
        level: measured.as_str().to_string(),
        notes_json: serde_json::to_string(&result.guarantee_notes)
            .map_err(|e| ApiError::Internal(e.into()))?,
        accepted: next_accepted(accepted, measured).as_str().to_string(),
        read_accepted,
    }))
}

const GUARANTEE_ATTEMPTS: usize = 5;

fn accept_race() -> ApiError {
    ApiError::Conflict(
        "The accepted level kept changing while the check was stored. Check the repository again."
            .into(),
    )
}

async fn accepted_now(state: &AppState, id: &str) -> Result<String, ApiError> {
    Ok(db::find_repository(state, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("repository not found".into()))?
        .guarantee_accepted)
}

async fn probe_new_key(
    id: &str,
    access: &ServerAccess,
    key: AccessKey,
) -> Result<ProbeReport, ApiError> {
    let opened = open_parts(
        access.location.clone(),
        access.server_creds.clone(),
        Some(key),
        AgentOptions::new(),
    )?;
    let report = run_probe(&opened)
        .instrument(tracing::info_span!("agent_key_probe", repository_id = %id))
        .await;
    match server_key_problem(&report) {
        Some(problem) => Err(problem),
        None => Ok(report),
    }
}

#[utoipa::path(
    post,
    tag = "Repositories",
    summary = "Check a repository again",
    description = "Measures the storage again with the stored keys and stores the measured level. \
        The accepted level rises when a higher level is measured and stays put otherwise; a check \
        that could not measure anything changes neither level.",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/repositories/{id}/check",
    params(("id" = String, Path, description = "Repository id")),
    responses(
        (status = 200, description = "Fresh capability checks with the stored keys", body = CheckResult),
        (status = 404, description = "Unknown repository", body = ErrorBody),
        (status = 409, description = "The storage access changed, or a newer check was stored, while this one ran", body = ErrorBody),
    )
)]
async fn recheck(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<CheckResult>, ApiError> {
    recheck_repository(&state, &id).await.map(Json)
}

pub(crate) async fn recheck_repository(
    state: &AppState,
    id: &str,
) -> Result<CheckResult, ApiError> {
    recheck_repository_at(state, id, now_ms(), false).await
}

/// `at_ms` is when the probe starts; the daily tick and the tests pass their
/// own clock. `daily` marks the tick's own run.
pub async fn recheck_repository_at(
    state: &AppState,
    id: &str,
    at_ms: i64,
    daily: bool,
) -> Result<CheckResult, ApiError> {
    let started = std::time::Instant::now();
    let access = load(state, id).await?;
    let opened = open_parts_with(
        access.location.clone(),
        access.server_creds.clone(),
        access.agent_creds.clone(),
        access.agent_options.clone(),
        access.sas_vending,
    )?;
    let report = run_probe_in(state, &opened, Some(id))
        .instrument(tracing::info_span!("recheck", repository_id = %id))
        .await?;
    let unlocked = match unlock_with_server_slot(&access).await {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(repository_id = %id, error = %error, "the server cannot open the repository");
            false
        }
    };
    state.repos.lock().unlocked.insert(id.to_string(), unlocked);
    let result = to_result(&report);
    let checks_json =
        serde_json::to_string(&result.checks).map_err(|e| ApiError::Internal(e.into()))?;
    let host_key_mismatch = (access.location.backend == "sftp")
        .then(|| {
            db::mismatch_update(
                report.host_key_changed.as_ref(),
                report.check(CheckId::Write).status != CheckStatus::Fail,
            )
        })
        .flatten();
    let mut stored = false;
    for _ in 0..GUARANTEE_ATTEMPTS {
        let write = guarantee_write(&result, &accepted_now(state, id).await?, false)?;
        let outcome = db::update_checks(
            state,
            id,
            &db::CheckWrite {
                generation: access.generation,
                at_ms,
                now_ms: at_ms.saturating_add(started.elapsed().as_millis() as i64),
                checks_json: &checks_json,
                agent_key_state: &result.agent_key_state,
                guarantee: write.as_ref(),
                daily,
                host_key_mismatch: host_key_mismatch.clone(),
            },
        )
        .await?;
        match outcome {
            db::Stored::Written => {
                stored = true;
                break;
            }
            db::Stored::AcceptedChanged => {}
            db::Stored::Stale => {
                return Err(ApiError::Conflict(
                    "The storage access changed or a newer check was stored during this check. Check the repository again."
                        .into(),
                ));
            }
            db::Stored::Missing => {
                return Err(ApiError::NotFound("repository not found".into()));
            }
        }
    }
    if !stored {
        return Err(accept_race());
    }
    if let Err(error) = crate::catalog::open_repository(state, id).await {
        tracing::warn!(repository_id = %id, error = %format!("{error:#}"), "repository is not available");
    }
    Ok(result)
}

#[utoipa::path(
    post,
    tag = "Repositories",
    summary = "Check new agent credentials",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/repositories/{id}/agent-key/verify",
    params(("id" = String, Path, description = "Repository id")),
    request_body = Credentials,
    responses(
        (status = 200, description = "What the new key can do; nothing is changed", body = KeyVerification),
        (status = 404, description = "Unknown repository", body = ErrorBody),
        (status = 409, description = "The repository has no separate agent key", body = ErrorBody),
    )
)]
async fn verify_key(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<String>,
    Json(new_key): Json<Credentials>,
) -> Result<Json<KeyVerification>, ApiError> {
    let access = load(&state, &id).await?;
    require_s3(&access)?;
    let report = probe_new_key(&id, &access, new_key.into()).await?;
    Ok(Json(KeyVerification {
        agent_key_state: agent_key_state_name(&report).to_string(),
        check: agent_check(&report),
    }))
}

#[utoipa::path(
    put,
    tag = "Repositories",
    summary = "Replace agent credentials",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/repositories/{id}/agent-key",
    params(("id" = String, Path, description = "Repository id")),
    request_body = Credentials,
    responses(
        (status = 200, description = "The key was replaced and sent to the online agents", body = KeyReplaced),
        (status = 404, description = "Unknown repository", body = ErrorBody),
        (status = 409, description = "The repository has no separate agent key", body = ErrorBody),
        (status = 422, description = "The new key could not be verified", body = ErrorBody),
    )
)]
async fn replace_key(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<String>,
    Json(new_key): Json<Credentials>,
) -> Result<Json<KeyReplaced>, ApiError> {
    let access = load(&state, &id).await?;
    require_s3(&access)?;
    let key: AccessKey = new_key.into();
    let report = probe_new_key(&id, &access, key.clone()).await?;
    if let Some(reason) = rejection(&report) {
        return Err(ApiError::Validation(format!(
            "The new key could not be verified: {reason}. The current key is unchanged."
        )));
    }
    let unavailable = unavailable(crate::catalog::open_repository(&state, &id).await);

    let access_lock = state.repos.access_lock(&id);
    let held = access_lock.lock().await;
    let state_name = agent_key_state_name(&report);
    let row = db::find_repository(&state, &id)
        .await?
        .ok_or_else(|| ApiError::NotFound("repository not found".into()))?;
    let mut checks = row
        .checks_json
        .as_deref()
        .map(serde_json::from_str::<Vec<CheckDto>>)
        .transpose()
        .map_err(|e| ApiError::Internal(e.into()))?
        .unwrap_or_default();
    let fresh = agent_check(&report);
    match checks.iter_mut().find(|c| c.id == fresh.id) {
        Some(row) => *row = fresh,
        None => checks.push(fresh),
    }
    let checks_json = serde_json::to_string(&checks).map_err(|e| ApiError::Internal(e.into()))?;
    let mut accepted = row.guarantee_accepted;
    let mut stored = false;
    for _ in 0..GUARANTEE_ATTEMPTS {
        let guarantee = guarantee_write(&to_result(&report), &accepted, true)?
            .expect("a credential change always writes a level");
        stored = db::replace_agent_creds(&state, &id, &key, state_name, &checks_json, &guarantee)
            .await?;
        if stored {
            break;
        }
        accepted = accepted_now(&state, &id).await?;
    }
    if !stored {
        return Err(accept_race());
    }

    let online = if unavailable {
        Vec::new()
    } else {
        state.registry.online_in_repository(&id)
    };
    let notified = online
        .iter()
        .filter(|agent| {
            state.registry.send(
                agent,
                ServerMessage {
                    body: Some(Body::UpdateS3Credentials(UpdateS3Credentials {
                        credentials: Some(S3Credentials {
                            access_key_id: key.access_key_id.clone(),
                            secret_access_key: key.secret_access_key.clone(),
                        }),
                    })),
                },
            )
        })
        .count() as u32;
    drop(held);
    let active = db::active_hosts(&state)
        .await?
        .get(&id)
        .copied()
        .unwrap_or(0)
        .max(0) as u32;
    Ok(Json(KeyReplaced {
        agent_key_state: state_name.to_string(),
        online_notified: notified,
        offline: active.saturating_sub(notified),
    }))
}

#[utoipa::path(
    post,
    tag = "Repositories",
    summary = "Show the host key an SFTP server presents",
    description = "Runs the SSH handshake only and compares the key the server presents now with \
        the pinned one. Nothing is authenticated, written or changed.",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/repositories/{id}/host-key/check",
    params(("id" = String, Path, description = "Repository id")),
    responses(
        (status = 200, description = "The presented key and whether it is the pinned one", body = HostKeyOffer),
        (status = 404, description = "Unknown repository", body = ErrorBody),
        (status = 409, description = "The repository is not on SFTP", body = ErrorBody),
        (status = 422, description = "The stored endpoint is malformed", body = ErrorBody),
        (status = 502, description = "The server could not be reached", body = ErrorBody),
    )
)]
async fn check_host_key(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<HostKeyOffer>, ApiError> {
    let access = load(&state, &id).await?;
    require_sftp(&access)?;
    let endpoint = access
        .location
        .options
        .get("endpoint")
        .map_or("", String::as_str);
    let offered = tokio::time::timeout(PROBE_TIMEOUT, SftpStore::offered_host_key(endpoint))
        .await
        .map_err(|_| ApiError::Backend("the SFTP server did not answer in time".into()))?
        .map_err(|error| match error {
            lodestore_format::Error::InvalidInput(message) => ApiError::Validation(message),
            other => ApiError::Backend(other.to_string()),
        })?;
    let pinned_fingerprint = access
        .location
        .options
        .get("host_key")
        .and_then(|key| SftpStore::host_key_fingerprint(key).ok())
        .unwrap_or_else(|| "unknown".to_string());
    Ok(Json(HostKeyOffer {
        matches: pinned_fingerprint == offered.fingerprint,
        key: offered.openssh,
        fingerprint: offered.fingerprint,
        pinned_fingerprint,
    }))
}

#[utoipa::path(
    put,
    tag = "Repositories",
    summary = "Pin a new SFTP host key",
    description = "Needs the password entered in the last 15 minutes. Pins the key only after a \
        full check with it reached the storage, then sends it to the online agents. Nothing \
        changes when the check fails.",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/repositories/{id}/host-key",
    params(("id" = String, Path, description = "Repository id")),
    request_body = HostKeyUpdate,
    responses(
        (status = 200, description = "The key was pinned", body = HostKeyReplaced),
        (status = 403, description = "`step_up_required`: the password was not entered in the last 15 minutes", body = ErrorBody),
        (status = 404, description = "Unknown repository", body = ErrorBody),
        (status = 409, description = "The repository is not on SFTP, or its storage access changed during the check", body = ErrorBody),
        (status = 422, description = "The key is malformed or the server does not present it", body = ErrorBody),
    )
)]
async fn replace_host_key(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    Json(update): Json<HostKeyUpdate>,
) -> Result<Json<HostKeyReplaced>, ApiError> {
    crate::auth::require_recent_password(&user)?;
    let access = load(&state, &id).await?;
    require_sftp(&access)?;
    let host_key = update.host_key.trim().to_string();
    let fingerprint = SftpStore::host_key_fingerprint(&host_key).map_err(invalid)?;
    let mut candidate = access.location.clone();
    candidate
        .options
        .insert("host_key".to_string(), host_key.clone());
    let opened = open_parts(
        candidate.clone(),
        access.server_creds.clone(),
        access.agent_creds.clone(),
        access.agent_options.clone(),
    )?;
    let report = run_probe(&opened)
        .instrument(tracing::info_span!("host_key_probe", repository_id = %id))
        .await;
    let write = report.check(CheckId::Write);
    if write.status == CheckStatus::Fail {
        return Err(ApiError::Validation(format!(
            "The new host key could not be verified: {}. The pinned key is unchanged.",
            write.error.as_deref().unwrap_or("unknown error")
        )));
    }

    let result = to_result(&report);
    let checks_json =
        serde_json::to_string(&result.checks).map_err(|e| ApiError::Internal(e.into()))?;
    let generation = access.generation;
    let unavailable = unavailable(
        crate::catalog::open_with_access(
            &state,
            &id,
            ServerAccess {
                location: candidate,
                ..access
            },
        )
        .await,
    );
    let access_lock = state.repos.access_lock(&id);
    let held = access_lock.lock().await;
    let mut accepted = accepted_now(&state, &id).await?;
    let mut stored = false;
    for _ in 0..GUARANTEE_ATTEMPTS {
        let guarantee = guarantee_write(&result, &accepted, true)?
            .expect("a credential change always writes a level");
        match db::replace_host_key(
            &state,
            &id,
            generation,
            &host_key,
            &result.agent_key_state,
            &checks_json,
            &guarantee,
        )
        .await?
        {
            db::Stored::Written => {
                stored = true;
                break;
            }
            db::Stored::AcceptedChanged => accepted = accepted_now(&state, &id).await?,
            db::Stored::Stale => {
                return Err(ApiError::Conflict(
                    "The storage access changed while the new host key was checked. Try again."
                        .into(),
                ));
            }
            db::Stored::Missing => {
                return Err(ApiError::NotFound("repository not found".into()));
            }
        }
    }
    if !stored {
        return Err(accept_race());
    }

    let (mut online_notified, mut needs_update) = (0u32, 0u32);
    if !unavailable {
        for agent in state.registry.online_in_repository(&id) {
            if !state.registry.supports(&agent, CAP_HOST_KEY) {
                needs_update += 1;
            } else if state.registry.send(&agent, host_key_message(&host_key)) {
                online_notified += 1;
            }
        }
    }
    drop(held);
    let active = db::active_hosts(&state)
        .await?
        .get(&id)
        .copied()
        .unwrap_or(0)
        .max(0) as u32;
    Ok(Json(HostKeyReplaced {
        fingerprint,
        online_notified,
        needs_update,
        offline: active.saturating_sub(online_notified + needs_update),
    }))
}

#[utoipa::path(
    put,
    tag = "Repositories",
    summary = "Replace the agents' own credentials",
    description = "For Azure Blob, GCS and SFTP repositories created with separate agent credentials. Needs the password entered in the last \
        15 minutes. Replaces the whole set of agent options after a full check with them reached the \
        storage, then sends the new location to the online agents; offline agents get it when they \
        connect. Nothing changes when the check fails. The values are never returned.",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/repositories/{id}/agent-options",
    params(("id" = String, Path, description = "Repository id")),
    request_body = AgentOptionsUpdate,
    responses(
        (status = 200, description = "The options were replaced", body = AgentOptionsReplaced),
        (status = 403, description = "`step_up_required`: the password was not entered in the last 15 minutes", body = ErrorBody),
        (status = 404, description = "Unknown repository", body = ErrorBody),
        (status = 409, description = "The repository is on S3 or local storage, was created with shared credentials, hands out server-signed SAS tokens, or its storage access changed during the check", body = ErrorBody),
        (status = 422, description = "An option is not allowed or empty, or the new options could not be verified", body = ErrorBody),
    )
)]
async fn replace_agent_options(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    Json(update): Json<AgentOptionsUpdate>,
) -> Result<Json<AgentOptionsReplaced>, ApiError> {
    crate::auth::require_recent_password(&user)?;
    let access = load(&state, &id).await?;
    let backend = access.location.backend.clone();
    if matches!(backend.as_str(), "s3" | "local") {
        return Err(ApiError::Conflict(
            "Only Azure, GCS and SFTP repositories take agent options; an S3 repository takes PUT \
             /api/v1/repositories/{id}/agent-key."
                .into(),
        ));
    }
    if access.sas_vending {
        return Err(ApiError::Conflict(
            "This repository gives its agents short-lived SAS tokens signed by the server; there \
             are no agent options to replace."
                .into(),
        ));
    }
    if access.agent_options.is_empty() {
        return Err(ApiError::Conflict(
            "This repository was created with shared credentials, so every enrolled host already \
             holds the server's credentials. Create a new repository with separate agent \
             credentials."
                .into(),
        ));
    }
    if update.agent_options.is_empty() {
        return Err(ApiError::Validation(
            "Send at least one agent option.".into(),
        ));
    }
    check_agent_options(&backend, &update.agent_options)?;
    let new: AgentOptions = update
        .agent_options
        .into_iter()
        .map(|(k, v)| (k, zeroize::Zeroizing::new(v)))
        .collect();
    let opened = open_parts(
        access.location.clone(),
        access.server_creds.clone(),
        access.agent_creds.clone(),
        new.clone(),
    )?;
    let report = run_probe_in(&state, &opened, Some(&id))
        .instrument(tracing::info_span!("agent_options_probe", repository_id = %id))
        .await?;
    let write = report.check(CheckId::Write);
    if write.status == CheckStatus::Fail {
        return Err(ApiError::Validation(format!(
            "The server credentials cannot write to the storage ({}), so the new agent options could not be checked. The current agent options are unchanged.",
            write.error.as_deref().unwrap_or("unknown error")
        )));
    }
    if let Some(reason) = rejection(&report) {
        return Err(ApiError::Validation(format!(
            "The new agent options could not be verified: {reason}. The current agent options are unchanged."
        )));
    }

    cap_exposed_neighbours(&state, &access.location, &new, false, Some(&id)).await?;

    let result = to_result(&report);
    let checks_json =
        serde_json::to_string(&result.checks).map_err(|e| ApiError::Internal(e.into()))?;
    let unavailable = unavailable(crate::catalog::open_repository(&state, &id).await);
    let access_lock = state.repos.access_lock(&id);
    let held = access_lock.lock().await;
    let mut accepted = accepted_now(&state, &id).await?;
    let mut stored = false;
    let mut level = result.guarantee_level.clone();
    for _ in 0..GUARANTEE_ATTEMPTS {
        let guarantee = guarantee_write(&result, &accepted, true)?
            .expect("a credential change always writes a level");
        level = guarantee.level.clone();
        match db::replace_agent_options(
            &state,
            &id,
            access.generation,
            &new,
            &result.agent_key_state,
            &checks_json,
            &guarantee,
        )
        .await?
        {
            db::Stored::Written => {
                stored = true;
                break;
            }
            db::Stored::AcceptedChanged => accepted = accepted_now(&state, &id).await?,
            db::Stored::Stale => {
                return Err(ApiError::Conflict(
                    "The storage access changed while the new agent options were checked. Try again."
                        .into(),
                ));
            }
            db::Stored::Missing => {
                return Err(ApiError::NotFound("repository not found".into()));
            }
        }
    }
    if !stored {
        return Err(accept_race());
    }

    let (mut online_notified, mut needs_update) = (0u32, 0u32);
    if !unavailable {
        match db::writer_material(&state, &id).await {
            Ok(material) => {
                for agent in state.registry.online_in_repository(&id) {
                    if !state.registry.supports(&agent, CAP_LOCATION) {
                        needs_update += 1;
                    } else if state
                        .registry
                        .send(&agent, location_message(&material.location, &id))
                    {
                        online_notified += 1;
                    }
                }
            }
            Err(error) => {
                tracing::warn!(repository_id = %id, error = %format!("{error:#}"), "could not load the new agent location")
            }
        }
    }
    drop(held);
    let active = db::active_hosts(&state)
        .await?
        .get(&id)
        .copied()
        .unwrap_or(0)
        .max(0) as u32;
    Ok(Json(AgentOptionsReplaced {
        agent_key_state: result.agent_key_state,
        guarantee_level: level,
        online_notified,
        needs_update,
        offline: active.saturating_sub(online_notified + needs_update),
    }))
}

#[utoipa::path(
    post,
    tag = "Repositories",
    summary = "Accept the measured level",
    description = "Makes the level the last check measured the accepted one, provided it is still \
        `expected_level`, the level the user was shown.",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/repositories/{id}/guarantee/accept",
    params(("id" = String, Path, description = "Repository id")),
    request_body = AcceptGuarantee,
    responses(
        (status = 200, description = "The measured level is now the accepted one", body = GuaranteeState),
        (status = 404, description = "Unknown repository", body = ErrorBody),
        (status = 409, description = "No level was measured, or the measured level is no longer `expected_level`", body = ErrorBody),
    )
)]
async fn accept_guarantee(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<String>,
    Json(body): Json<AcceptGuarantee>,
) -> Result<Json<GuaranteeState>, ApiError> {
    if db::find_repository(&state, &id).await?.is_none() {
        return Err(ApiError::NotFound("repository not found".into()));
    }
    let accepted = db::accept_measured_guarantee(&state, &id, &body.expected_level).await?;
    let row = db::find_repository(&state, &id)
        .await?
        .ok_or_else(|| ApiError::NotFound("repository not found".into()))?;
    if !accepted {
        return Err(ApiError::Conflict(if row.guarantee_level == "unchecked" {
            "Nothing was measured, so there is no level to accept. Check the repository again."
                .into()
        } else {
            "The measured level has changed since it was shown. Review the new level and accept \
             it again."
                .into()
        }));
    }
    Ok(Json(GuaranteeState {
        guarantee_level: row.guarantee_level,
        guarantee_accepted: row.guarantee_accepted,
        guarantee_checked_at: row.guarantee_checked_at,
    }))
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(recheck))
        .routes(routes!(accept_guarantee))
        .routes(routes!(verify_key))
        .routes(routes!(replace_key))
        .routes(routes!(check_host_key))
        .routes(routes!(replace_host_key))
        .routes(routes!(replace_agent_options))
        .routes(routes!(storage_access))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repos::checks::timed_out_report;

    #[test]
    fn next_accepted_follows_the_table() {
        use GuaranteeLevel::*;
        for (accepted, measured, expected) in [
            (Unchecked, CreateOnlyEnforced, CreateOnlyEnforced),
            (
                Unchecked,
                OverwriteKeepsOldVersion,
                OverwriteKeepsOldVersion,
            ),
            (Unchecked, HostCanOverwriteOrDelete, Unchecked),
            (
                OverwriteKeepsOldVersion,
                CreateOnlyEnforced,
                CreateOnlyEnforced,
            ),
            (
                OverwriteKeepsOldVersion,
                HostCanOverwriteOrDelete,
                OverwriteKeepsOldVersion,
            ),
            (
                HostCanOverwriteOrDelete,
                OverwriteKeepsOldVersion,
                OverwriteKeepsOldVersion,
            ),
            (
                CreateOnlyEnforced,
                OverwriteKeepsOldVersion,
                CreateOnlyEnforced,
            ),
            (CreateOnlyEnforced, Unchecked, CreateOnlyEnforced),
            (
                OverwriteKeepsOldVersion,
                Unchecked,
                OverwriteKeepsOldVersion,
            ),
            (
                HostCanOverwriteOrDelete,
                Unchecked,
                HostCanOverwriteOrDelete,
            ),
            (Unchecked, Unchecked, Unchecked),
        ] {
            assert_eq!(
                next_accepted(accepted, measured),
                expected,
                "{accepted:?} {measured:?}"
            );
        }
    }

    #[test]
    fn failed_server_write_is_a_server_key_problem() {
        let message = match server_key_problem(&timed_out_report()) {
            Some(ApiError::Validation(message)) => message,
            _ => panic!("a failed write check must be reported"),
        };
        assert!(
            message.starts_with("The server key cannot write"),
            "{message}"
        );
        assert!(
            !message.contains("new key could not be verified"),
            "{message}"
        );
    }
}
