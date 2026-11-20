use axum::Json;
use axum::extract::{Path, State};
use lodestore_format::store::probe::{CheckId, CheckStatus, GuaranteeLevel, ProbeReport};
use lodestore_proto::agent::v1::server_message::Body;
use lodestore_proto::agent::v1::{S3Credentials, ServerMessage, UpdateS3Credentials};
use serde::Serialize;
use tracing::Instrument;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::AuthUser;
use crate::error::{ApiError, ErrorBody};
use crate::repos::checks::{agent_key_state_name, open_parts, run_probe, to_result};
use crate::repos::db::{self, AccessKey, ServerAccess};
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

#[derive(Serialize, ToSchema)]
pub struct KeyReplaced {
    pub agent_key_state: String,
    pub online_notified: u32,
    pub offline: u32,
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
    let accepted = GuaranteeLevel::parse(accepted).unwrap_or(GuaranteeLevel::Unchecked);
    if measured == GuaranteeLevel::Unchecked {
        return Ok(credentials_changed.then(|| db::GuaranteeWrite {
            level: measured.as_str().to_string(),
            notes_json: "[]".to_string(),
            accepted: accepted.as_str().to_string(),
        }));
    }
    Ok(Some(db::GuaranteeWrite {
        level: measured.as_str().to_string(),
        notes_json: serde_json::to_string(&result.guarantee_notes)
            .map_err(|e| ApiError::Internal(e.into()))?,
        accepted: next_accepted(accepted, measured).as_str().to_string(),
    }))
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
    let access = load(state, id).await?;
    let opened = open_parts(
        access.location.clone(),
        access.server_creds.clone(),
        access.agent_creds.clone(),
    )?;
    let report = run_probe(&opened)
        .instrument(tracing::info_span!("recheck", repository_id = %id))
        .await;
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
    let accepted = db::find_repository(state, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("repository not found".into()))?
        .guarantee_accepted;
    let write = guarantee_write(&result, &accepted, false)?;
    db::update_checks(
        state,
        id,
        &checks_json,
        &result.agent_key_state,
        write.as_ref(),
    )
    .await?;
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
    let guarantee = guarantee_write(&to_result(&report), &row.guarantee_accepted, true)?
        .expect("a credential change always writes a level");
    db::replace_agent_creds(&state, &id, &key, state_name, &checks_json, &guarantee).await?;

    let unavailable = matches!(
        crate::catalog::open_repository(&state, &id).await,
        Err(error) if error.downcast_ref::<crate::catalog::RepositoryUnavailable>().is_some()
    );
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
    summary = "Accept the measured level",
    description = "Makes the level the last check measured the accepted one.",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/repositories/{id}/guarantee/accept",
    params(("id" = String, Path, description = "Repository id")),
    responses(
        (status = 200, description = "The measured level is now the accepted one", body = GuaranteeState),
        (status = 404, description = "Unknown repository", body = ErrorBody),
        (status = 409, description = "No level was measured, so there is nothing to accept", body = ErrorBody),
    )
)]
async fn accept_guarantee(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<GuaranteeState>, ApiError> {
    if db::find_repository(&state, &id).await?.is_none() {
        return Err(ApiError::NotFound("repository not found".into()));
    }
    if !db::accept_measured_guarantee(&state, &id).await? {
        return Err(ApiError::Conflict(
            "Nothing was measured, so there is no level to accept. Check the repository again."
                .into(),
        ));
    }
    let row = db::find_repository(&state, &id)
        .await?
        .ok_or_else(|| ApiError::NotFound("repository not found".into()))?;
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
