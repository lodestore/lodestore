use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::CACHE_CONTROL;
use axum::response::{IntoResponse, Response};
use lodestore_format::config::config_hash;
use lodestore_format::crypto::Entropy;
use lodestore_format::keys::{MasterKey, RecoveryKey};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::store::policy::{agent_iam_policy_json, agent_minio_commands};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use zeroize::Zeroizing;

use crate::auth::AuthUser;
use crate::db::now_ms;
use crate::error::{ApiError, ErrorBody};
use crate::repos::checks::{
    Opened, open_draft, run_checks, run_probe, split_secret_options, to_result,
};
use crate::repos::db::{self, NewRepository, RepoExtras, RepoLocation};
use crate::repos::{
    CheckResult, RepositoryDraft, RepositorySummary, WIZARD_TTL_MS, WizardEntry, summary_of,
};
use crate::state::AppState;

const MIN_PASSWORD_CHARS: usize = 12;
const MAX_NAME_CHARS: usize = 48;

#[derive(Serialize, ToSchema)]
pub struct WizardStarted {
    pub wizard_id: String,
    pub recovery_key: String,
    pub expires_at: i64,
}

#[derive(Deserialize, ToSchema)]
pub struct CommitRequest {
    pub password: String,
    pub recovery_key_last_group: String,
}

#[derive(Serialize, ToSchema)]
pub struct ConflictBody {
    pub code: String,
    pub message: String,
    pub check: CheckResult,
}

fn conflict_with(message: &str, check: CheckResult) -> Response {
    (
        StatusCode::CONFLICT,
        Json(ConflictBody {
            code: "conflict".to_string(),
            message: message.to_string(),
            check,
        }),
    )
        .into_response()
}

pub(crate) fn storage_error(e: lodestore_format::Error) -> ApiError {
    use lodestore_format::Error;
    match e {
        Error::InvalidInput(_) | Error::AlreadyInitialized => ApiError::Conflict(e.to_string()),
        other => ApiError::Backend(other.to_string()),
    }
}

fn validate_name(name: &str) -> Result<(), ApiError> {
    let ok = !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(ApiError::Validation(
            "Use 1 to 48 characters: lowercase letters, digits and hyphens.".into(),
        ))
    }
}

async fn ensure_free_name(state: &AppState, name: &str) -> Result<(), ApiError> {
    validate_name(name)?;
    if db::name_exists(state, name).await? {
        return Err(ApiError::Conflict(
            "A repository with this name already exists.".into(),
        ));
    }
    Ok(())
}

fn last_group_matches(key: &RecoveryKey, typed: &str) -> bool {
    let encoded = key.encode();
    let last = encoded.rsplit('-').next().unwrap_or_default();
    last.eq_ignore_ascii_case(typed.trim())
}

fn random_id() -> String {
    data_encoding::HEXLOWER.encode(&Entropy::os().bytes::<16>())
}

#[utoipa::path(
    post,
    path = "/api/v1/repositories/check",
    request_body = RepositoryDraft,
    responses(
        (status = 200, description = "Capability checks of the storage", body = CheckResult),
        (status = 422, description = "The location or keys are malformed", body = ErrorBody),
    )
)]
async fn check_storage(
    _user: AuthUser,
    Json(draft): Json<RepositoryDraft>,
) -> Result<Json<CheckResult>, ApiError> {
    Ok(Json(to_result(&run_checks(&draft).await?)))
}

#[utoipa::path(
    post,
    path = "/api/v1/repositories/wizard",
    request_body = RepositoryDraft,
    responses(
        (status = 201, description = "A new recovery key; nothing is written to the storage yet", body = WizardStarted),
        (status = 409, description = "The storage failed a check or already holds a repository", body = ConflictBody),
        (status = 422, description = "The draft is malformed", body = ErrorBody),
    )
)]
async fn start_wizard(
    State(state): State<AppState>,
    _user: AuthUser,
    Json(draft): Json<RepositoryDraft>,
) -> Result<Response, ApiError> {
    ensure_free_name(&state, &draft.name).await?;
    let opened = open_draft(&draft)?;
    let report = run_probe(&opened).await;
    let result = to_result(&report);
    if report.blocking() {
        return Ok(conflict_with(
            "The storage did not pass the required checks.",
            result,
        ));
    }
    if report.existing_repository {
        return Ok(conflict_with(
            "This storage already holds a Lodestore repository. Connect to it instead.",
            result,
        ));
    }
    PendingInit::ensure_uninitialized(&opened.server)
        .await
        .map_err(storage_error)?;

    let pending = PendingInit::generate(&mut Entropy::os(), InitOptions::default())
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("{e}")))?;
    let recovery_key = pending.recovery_key().encode();
    let wizard_id = random_id();
    let expires_at = now_ms() + WIZARD_TTL_MS;
    {
        let mut inner = state.repos.lock();
        inner.purge_expired();
        inner.wizards.retain(|_, w| w.draft.name != draft.name);
        inner.wizards.insert(
            wizard_id.clone(),
            WizardEntry {
                pending,
                draft,
                checks: result,
                expires_at,
            },
        );
    }
    Ok((
        StatusCode::CREATED,
        [(CACHE_CONTROL, "no-store")],
        Json(WizardStarted {
            wizard_id,
            recovery_key: recovery_key.to_string(),
            expires_at,
        }),
    )
        .into_response())
}

#[utoipa::path(
    post,
    path = "/api/v1/repositories/wizard/{id}/commit",
    params(("id" = String, Path, description = "Wizard id from the start response")),
    request_body = CommitRequest,
    responses(
        (status = 201, description = "The repository was created", body = RepositorySummary),
        (status = 404, description = "The setup session expired", body = ErrorBody),
        (status = 422, description = "Weak password or wrong last group", body = ErrorBody),
    )
)]
async fn commit_wizard(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<String>,
    Json(req): Json<CommitRequest>,
) -> Result<(StatusCode, Json<RepositorySummary>), ApiError> {
    let password = Zeroizing::new(req.password);
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err(ApiError::Validation("At least 12 characters.".into()));
    }
    let entry = {
        let mut inner = state.repos.lock();
        inner.purge_expired();
        let entry = inner
            .wizards
            .get(&id)
            .ok_or_else(|| ApiError::NotFound("This setup session expired. Start again.".into()))?;
        if !last_group_matches(entry.pending.recovery_key(), &req.recovery_key_last_group) {
            return Err(ApiError::Validation(
                "That does not match the last group of the key.".into(),
            ));
        }
        inner.wizards.remove(&id)
    }
    .ok_or_else(|| ApiError::NotFound("This setup session expired. Start again.".into()))?;
    let WizardEntry {
        pending,
        draft,
        checks: probe_result,
        ..
    } = entry;

    ensure_free_name(&state, &draft.name).await?;
    let opened = open_draft(&draft)?;
    let keys = pending.read_keys();
    let master = keys.master().clone();
    let format_repo_id = *keys.repo_id();
    let hash = config_hash(pending.config_bytes());

    let mut entropy = Entropy::os();
    pending
        .commit(&opened.server, password.as_bytes(), &mut entropy)
        .await
        .map_err(storage_error)?;
    let repository = Repository::open(&opened.server, password.as_bytes())
        .await
        .map_err(storage_error)?;
    let server_secret = Zeroizing::new(entropy.bytes::<32>().to_vec());
    repository
        .add_password_slot(&opened.server, &server_secret, &mut entropy)
        .await
        .map_err(storage_error)?;

    let summary = register(
        &state,
        Registration {
            name: draft.name,
            opened,
            master,
            format_repo_id,
            config_hash: hash,
            server_secret,
            checks: probe_result,
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(summary)))
}

struct Registration {
    name: String,
    opened: Opened,
    master: MasterKey,
    format_repo_id: [u8; 16],
    config_hash: [u8; 32],
    server_secret: Zeroizing<Vec<u8>>,
    checks: CheckResult,
}

async fn register(state: &AppState, r: Registration) -> Result<RepositorySummary, ApiError> {
    let Opened {
        location,
        server_creds,
        agent_creds,
        ..
    } = r.opened;
    let experimental = location.is_experimental();
    let (public, secret_options) = split_secret_options(&location);
    let checks_json =
        serde_json::to_string(&r.checks.checks).map_err(|e| ApiError::Internal(e.into()))?;
    let id = db::create_repository_ext(
        state,
        NewRepository {
            name: r.name,
            location: RepoLocation {
                backend: public.backend,
                options: public.options,
            },
            experimental,
            format_repo_id: r.format_repo_id,
            config_hash: r.config_hash,
            master: r.master,
            server_slot_secret: Some(r.server_secret),
            server_creds,
            agent_creds,
        },
        RepoExtras {
            secret_options,
            checks_json: Some(checks_json),
            agent_key_state: Some(r.checks.agent_key_state),
            checked_at: Some(now_ms()),
        },
    )
    .await?;
    let row = db::find_repository(state, &id)
        .await?
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("the new repository vanished")))?;
    state.repos.lock().unlocked.insert(id, true);
    Ok(summary_of(&row, 0, Some(true)))
}

#[derive(Deserialize, ToSchema)]
pub struct ConnectRequest {
    pub draft: RepositoryDraft,
    pub password: Option<String>,
    pub recovery_key: Option<String>,
}

const CONNECT_REFUSED: &str =
    "That password or recovery key does not open this repository. Check it and try again.";

#[utoipa::path(
    post,
    path = "/api/v1/repositories/connect",
    request_body = ConnectRequest,
    responses(
        (status = 201, description = "The existing repository was connected", body = RepositorySummary),
        (status = 409, description = "The storage failed a check or holds no repository", body = ConflictBody),
        (status = 422, description = "The secret does not open the repository", body = ErrorBody),
    )
)]
async fn connect_repository(
    State(state): State<AppState>,
    _user: AuthUser,
    Json(req): Json<ConnectRequest>,
) -> Result<Response, ApiError> {
    use lodestore_format::Error as FormatError;

    let password = req.password.filter(|p| !p.is_empty()).map(Zeroizing::new);
    let recovery = req
        .recovery_key
        .filter(|k| !k.trim().is_empty())
        .map(Zeroizing::new);
    if password.is_some() == recovery.is_some() {
        return Err(ApiError::Validation(
            "Enter either the repository password or the recovery key.".into(),
        ));
    }
    let draft = req.draft;
    ensure_free_name(&state, &draft.name).await?;
    let opened = open_draft(&draft)?;
    let report = run_probe(&opened).await;
    let checks = to_result(&report);
    if report.blocking() {
        return Ok(conflict_with(
            "The storage did not pass the required checks.",
            checks,
        ));
    }
    if !report.existing_repository {
        return Ok(conflict_with(
            "There is no Lodestore repository in this storage.",
            checks,
        ));
    }

    let opened_repository = match (&password, &recovery) {
        (Some(password), _) => Repository::open(&opened.server, password.as_bytes()).await,
        (_, Some(key)) => match RecoveryKey::parse(key) {
            Ok(key) => Repository::open_recovery(&opened.server, &key).await,
            Err(e) => Err(e),
        },
        _ => unreachable!("exactly one secret is present"),
    };
    let repository = opened_repository.map_err(|e| match e {
        FormatError::WrongSecret | FormatError::InvalidInput(_) => {
            ApiError::Validation(CONNECT_REFUSED.into())
        }
        other => storage_error(other),
    })?;

    let mut entropy = Entropy::os();
    let server_secret = Zeroizing::new(entropy.bytes::<32>().to_vec());
    repository
        .add_password_slot(&opened.server, &server_secret, &mut entropy)
        .await
        .map_err(storage_error)?;
    let summary = register(
        &state,
        Registration {
            name: draft.name,
            opened,
            master: repository.read_keys().master().clone(),
            format_repo_id: *repository.read_keys().repo_id(),
            config_hash: repository.config_hash(),
            server_secret,
            checks,
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(summary)).into_response())
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct PolicyQuery {
    bucket: String,
    #[serde(default)]
    prefix: String,
}

#[derive(Serialize, ToSchema)]
pub struct AgentPolicy {
    pub iam_json: String,
    pub minio_commands: String,
}

#[utoipa::path(
    get,
    path = "/api/v1/agent-policy",
    params(PolicyQuery),
    responses(
        (status = 200, description = "The IAM policy for a write-only agent key", body = AgentPolicy),
        (status = 422, description = "No bucket given", body = ErrorBody),
    )
)]
async fn agent_policy(
    _user: AuthUser,
    Query(q): Query<PolicyQuery>,
) -> Result<Json<AgentPolicy>, ApiError> {
    if q.bucket.trim().is_empty() {
        return Err(ApiError::Validation("Enter the bucket name.".into()));
    }
    Ok(Json(AgentPolicy {
        iam_json: agent_iam_policy_json(&q.bucket, &q.prefix),
        minio_commands: agent_minio_commands(&q.bucket, &q.prefix),
    }))
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(check_storage))
        .routes(routes!(start_wizard))
        .routes(routes!(commit_wizard))
        .routes(routes!(connect_repository))
        .routes(routes!(agent_policy))
}
