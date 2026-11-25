use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::CACHE_CONTROL;
use axum::response::{IntoResponse, Response};
use lodestore_format::config::config_hash;
use lodestore_format::crypto::Entropy;
use lodestore_format::keys::{MasterKey, RecoveryKey};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::store::policy::{
    STORAGE_PRESETS, agent_iam_policy_json, agent_minio_commands, agent_rights,
};
use lodestore_format::store::{BlobStore, SftpStore};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use zeroize::Zeroizing;

use crate::auth::AuthUser;
use crate::db::now_ms;
use crate::error::{ApiError, ErrorBody};
use crate::maintenance::check::db::repo_checks_summary;
use crate::maintenance::gc::api::CleanupSummary;
use crate::repos::checks::{
    Opened, PROBE_TIMEOUT, consent_message, format_location, invalid, needs_consent,
    open_draft_with, run_probe, split_secret_options, timed_out_report, to_result,
    unchecked_report,
};
use crate::repos::db::{self, NewRepository, RepoExtras, RepoLocation};
use crate::repos::{
    CheckResult, OfferedHostKeyDto, RepositoryDraft, RepositorySummary, WIZARD_TTL_MS, WizardEntry,
    base_of,
};
use crate::state::AppState;

const TOFU_MESSAGE: &str =
    "Confirm the SFTP server's host key and send it back as the host_key option.";
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
    conflict_coded("conflict", message, check)
}

fn conflict_coded(code: &str, message: &str, check: CheckResult) -> Response {
    (
        StatusCode::CONFLICT,
        Json(ConflictBody {
            code: code.to_string(),
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
    tag = "Repositories",
    summary = "Check storage access",
    security(("session" = [], "csrf" = [])),
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
    let pinned = draft
        .location
        .options
        .get("host_key")
        .is_some_and(|k| !k.trim().is_empty());
    if draft.location.backend == "sftp" && !pinned {
        return offer_host_key(&draft).await.map(Json);
    }
    let opened = open_draft_with(&draft, true)?;
    Ok(Json(to_result(&run_probe(&opened).await)))
}

/// First contact with an SFTP server: only the handshake runs, so nothing is
/// authenticated or written; the client confirms the key and sends it back.
async fn offer_host_key(draft: &RepositoryDraft) -> Result<CheckResult, ApiError> {
    let location = format_location(&draft.location);
    location.validate_new_without_host_key().map_err(invalid)?;
    let endpoint = location.options.get("endpoint").map_or("", String::as_str);
    let offered = tokio::time::timeout(PROBE_TIMEOUT, SftpStore::offered_host_key(endpoint)).await;
    let report = match offered {
        Ok(Ok(offered)) => {
            return Ok(CheckResult {
                host_key: Some(OfferedHostKeyDto {
                    key: offered.openssh,
                    fingerprint: offered.fingerprint,
                }),
                ..to_result(&unchecked_report(TOFU_MESSAGE))
            });
        }
        Ok(Err(lodestore_format::Error::InvalidInput(message))) => return Err(invalid(message)),
        Ok(Err(error)) => unchecked_report(&error.to_string()),
        Err(_) => timed_out_report(),
    };
    Ok(to_result(&report))
}

#[utoipa::path(
    post,
    tag = "Repositories",
    summary = "Start creating a repository",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/repositories/wizard",
    request_body = RepositoryDraft,
    responses(
        (status = 201, description = "A new recovery key; nothing is written to the storage yet", body = WizardStarted),
        (status = 409, description = "The storage failed a check, already holds a repository, or needs `accept_weaker_guarantee` (code `weaker_guarantee`)", body = ConflictBody),
        (status = 422, description = "The draft is malformed", body = ErrorBody),
    )
)]
async fn start_wizard(
    State(state): State<AppState>,
    _user: AuthUser,
    Json(draft): Json<RepositoryDraft>,
) -> Result<Response, ApiError> {
    ensure_free_name(&state, &draft.name).await?;
    let opened = open_draft_with(&draft, true)?;
    let sas_vending = opened.sas_vending;
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
    if needs_consent(&result) && !draft.accept_weaker_guarantee {
        return Ok(conflict_coded(
            "weaker_guarantee",
            &consent_message(&result, "create"),
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
                sas_vending,
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
    tag = "Repositories",
    summary = "Finish creating a repository",
    security(("session" = [], "csrf" = [])),
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
        sas_vending,
        ..
    } = entry;

    ensure_free_name(&state, &draft.name).await?;
    let opened = open_draft_with(&draft, sas_vending)?;
    let keys = pending.read_keys();
    let master = keys.master().clone();
    let format_repo_id = *keys.repo_id();
    let hash = config_hash(pending.config_bytes());

    let mut entropy = Entropy::os();
    opened
        .server
        .ensure_prefixes(&[])
        .await
        .map_err(|e| storage_error(e.into()))?;
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

/// Also the entry point of the agent location differential test.
pub struct Registration {
    pub name: String,
    pub opened: Opened,
    pub master: MasterKey,
    pub format_repo_id: [u8; 16],
    pub config_hash: [u8; 32],
    pub server_secret: Zeroizing<Vec<u8>>,
    pub checks: CheckResult,
}

pub async fn register(state: &AppState, r: Registration) -> Result<RepositorySummary, ApiError> {
    let Opened {
        location,
        server_creds,
        agent_creds,
        agent_options,
        sas_vending,
        ..
    } = r.opened;
    let (public, secret_options) = split_secret_options(&location);
    let checks_json =
        serde_json::to_string(&r.checks.checks).map_err(|e| ApiError::Internal(e.into()))?;
    let guarantee_notes_json = serde_json::to_string(&r.checks.guarantee_notes)
        .map_err(|e| ApiError::Internal(e.into()))?;
    let id = db::create_repository_ext(
        state,
        NewRepository {
            name: r.name,
            location: RepoLocation {
                backend: public.backend,
                options: public.options,
            },
            format_repo_id: r.format_repo_id,
            config_hash: r.config_hash,
            master: r.master,
            server_slot_secret: Some(r.server_secret),
            server_creds,
            agent_creds,
        },
        RepoExtras {
            secret_options,
            agent_options,
            checks_json: Some(checks_json),
            agent_key_state: Some(r.checks.agent_key_state),
            checked_at: Some(now_ms()),
            guarantee_level: Some(r.checks.guarantee_level),
            guarantee_notes_json: Some(guarantee_notes_json),
            agent_sas_vending: sas_vending,
        },
    )
    .await?;
    let row = db::find_repository(state, &id)
        .await?
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("the new repository vanished")))?;
    state.repos.lock().unlocked.insert(id.clone(), true);
    if let Err(error) = crate::maintenance::attest::ensure_upgraded(state, &id).await {
        tracing::warn!(repository = %id, error = %format!("{error:#}"), "attestation upgrade failed");
    }
    Ok(RepositorySummary {
        base: base_of(&row, 0, Some(true), CleanupSummary::OFF),
        checks: repo_checks_summary(state, &id).await?,
    })
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
    tag = "Repositories",
    summary = "Connect an existing repository",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/repositories/connect",
    request_body = ConnectRequest,
    responses(
        (status = 201, description = "The existing repository was connected", body = RepositorySummary),
        (status = 409, description = "The storage failed a check, holds no repository, or needs `accept_weaker_guarantee` (code `weaker_guarantee`)", body = ConflictBody),
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
    let opened = open_draft_with(&draft, false)?;
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
    if needs_consent(&checks) && !draft.accept_weaker_guarantee {
        return Ok(conflict_coded(
            "weaker_guarantee",
            &consent_message(&checks, "connect"),
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
        e @ FormatError::KdfBudget { .. } => ApiError::Validation(e.to_string()),
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
    crate::catalog::start_rebuild(&state, &summary.base.id);
    Ok((StatusCode::CREATED, Json(summary)).into_response())
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct PolicyQuery {
    #[serde(default)]
    bucket: Option<String>,
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    preset: Option<String>,
    #[serde(default)]
    user: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct AgentRightsDto {
    pub preset: String,
    pub mechanism: String,
    pub text: String,
    pub server_text: Option<String>,
    pub verified: bool,
    pub note: String,
}

#[derive(Serialize, ToSchema)]
pub struct AgentPolicy {
    pub iam_json: String,
    pub minio_commands: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rights: Option<AgentRightsDto>,
}

#[utoipa::path(
    get,
    tag = "Repositories",
    summary = "Get a write-only IAM policy for agents",
    path = "/api/v1/agent-policy",
    params(PolicyQuery),
    responses(
        (status = 200, description = "The IAM policy for a write-only agent key", body = AgentPolicy),
        (status = 422, description = "Unsafe or missing bucket, prefix or user, or unknown preset", body = ErrorBody),
    )
)]
async fn agent_policy(
    _user: AuthUser,
    Query(q): Query<PolicyQuery>,
) -> Result<Json<AgentPolicy>, ApiError> {
    let bucket = q.bucket.as_deref().unwrap_or_default();
    let have_bucket = !bucket.trim().is_empty();
    if q.preset.is_none() && !have_bucket {
        return Err(ApiError::Validation("Enter the bucket name.".into()));
    }
    let rights = match &q.preset {
        None => None,
        Some(preset) => {
            let user = q.user.as_deref().unwrap_or_default();
            let found = agent_rights(preset, bucket, &q.prefix, user)
                .map_err(|e| ApiError::Validation(e.to_string()))?;
            Some(AgentRightsDto {
                preset: preset.clone(),
                mechanism: found.mechanism.as_str().into(),
                text: found.text,
                server_text: found.server_text,
                verified: found.verified,
                note: found.note.into(),
            })
        }
    };
    let (iam_json, minio_commands) = if have_bucket {
        (
            agent_iam_policy_json(bucket, &q.prefix),
            agent_minio_commands(bucket, &q.prefix),
        )
    } else {
        Default::default()
    };
    Ok(Json(AgentPolicy {
        iam_json,
        minio_commands,
        rights,
    }))
}

#[derive(Serialize, ToSchema)]
pub struct StoragePresetDto {
    pub id: String,
    pub label: String,
    pub backend: String,
    pub endpoint_template: Option<String>,
    pub region: Option<String>,
    pub path_style: bool,
    pub provider: Option<String>,
    pub mechanism: String,
    pub verified: bool,
    pub note: String,
}

#[utoipa::path(
    get,
    tag = "Repositories",
    summary = "List the storage presets",
    path = "/api/v1/storage-presets",
    responses((status = 200, description = "The storage presets", body = Vec<StoragePresetDto>))
)]
async fn storage_presets(_user: AuthUser) -> Json<Vec<StoragePresetDto>> {
    Json(
        STORAGE_PRESETS
            .iter()
            .map(|p| StoragePresetDto {
                id: p.id.into(),
                label: p.label.into(),
                backend: p.backend.into(),
                endpoint_template: p.endpoint_template.map(Into::into),
                region: p.region.map(Into::into),
                path_style: p.path_style,
                provider: p.provider.map(Into::into),
                mechanism: p.mechanism.as_str().into(),
                verified: p.verified,
                note: p.note.into(),
            })
            .collect(),
    )
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(check_storage))
        .routes(routes!(start_wizard))
        .routes(routes!(commit_wizard))
        .routes(routes!(connect_repository))
        .routes(routes!(agent_policy))
        .routes(routes!(storage_presets))
}
