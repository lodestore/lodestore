pub mod agent_key;
pub mod checks;
pub mod db;
pub mod object_lock;
pub mod wizard;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};

use axum::Json;
use axum::extract::{Path, State};
use lodestore_format::repo::{PendingInit, Repository};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::AuthUser;
use crate::db::now_ms;
use crate::error::{ApiError, ErrorBody};
use crate::repos::db::{AccessKey, RepoFull};
use crate::state::AppState;

pub(crate) const WIZARD_TTL_MS: i64 = 30 * 60 * 1000;

pub(crate) struct WizardEntry {
    pub pending: PendingInit,
    pub draft: RepositoryDraft,
    pub checks: CheckResult,
    pub expires_at: i64,
}

#[derive(Default)]
pub(crate) struct Inner {
    pub wizards: HashMap<String, WizardEntry>,
    pub unlocked: HashMap<String, bool>,
}

impl Inner {
    pub(crate) fn purge_expired(&mut self) {
        let now = now_ms();
        self.wizards.retain(|_, w| w.expires_at > now);
    }
}

#[derive(Clone, Default)]
pub struct ReposState {
    inner: Arc<Mutex<Inner>>,
}

impl ReposState {
    pub(crate) fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LocationDto {
    pub backend: String,
    #[serde(default)]
    pub options: BTreeMap<String, String>,
}

#[derive(Clone, Deserialize, ToSchema)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials").finish_non_exhaustive()
    }
}

impl From<Credentials> for AccessKey {
    fn from(c: Credentials) -> Self {
        AccessKey {
            access_key_id: c.access_key_id,
            secret_access_key: c.secret_access_key,
        }
    }
}

#[derive(Clone, Deserialize, ToSchema)]
pub struct RepositoryDraft {
    #[serde(default)]
    pub name: String,
    pub location: LocationDto,
    pub server_credentials: Option<Credentials>,
    pub agent_credentials: Option<Credentials>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CheckDto {
    pub id: String,
    pub status: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CheckResult {
    pub existing_repository: bool,
    pub blocking: bool,
    pub agent_key_state: String,
    pub checks: Vec<CheckDto>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RepositorySummary {
    pub id: String,
    pub name: String,
    pub backend: String,
    pub experimental: bool,
    pub location_display: String,
    pub hosts_count: i64,
    pub agent_key_state: String,
    pub checked_at: Option<i64>,
    pub unlocked: Option<bool>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RepositoryDetail {
    #[serde(flatten)]
    pub summary: RepositorySummary,
    pub location: LocationDto,
    pub checks: Vec<CheckDto>,
}

pub(crate) fn summary_of(
    row: &RepoFull,
    hosts_count: i64,
    unlocked: Option<bool>,
) -> RepositorySummary {
    let location = lodestore_format::store::RepoLocation {
        backend: row.backend.clone(),
        options: row.options.clone(),
    };
    RepositorySummary {
        id: row.id.clone(),
        name: row.name.clone(),
        backend: row.backend.clone(),
        experimental: row.experimental,
        location_display: location.display(),
        hosts_count,
        agent_key_state: row.agent_key_state.clone(),
        checked_at: row.checked_at,
        unlocked,
    }
}

pub(crate) async fn unlock_with_server_slot(access: &db::ServerAccess) -> anyhow::Result<()> {
    let secret = access
        .server_slot_secret
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("the repository has no server key slot"))?;
    let store = checks::open_store(&access.location, access.server_creds.as_ref())?;
    let repo = Repository::open(&store, secret).await?;
    anyhow::ensure!(
        repo.config_hash() == access.config_hash,
        "the repository config differs from the registered one"
    );
    anyhow::ensure!(
        repo.read_keys().master().as_bytes() == access.master.as_slice(),
        "the repository master key differs from the registered one"
    );
    Ok(())
}

#[utoipa::path(
    get,
    tag = "Repositories",
    summary = "List repositories",
    path = "/api/v1/repositories",
    responses((status = 200, description = "All repositories", body = Vec<RepositorySummary>))
)]
async fn list_repositories(
    State(state): State<AppState>,
    _user: AuthUser,
) -> Result<Json<Vec<RepositorySummary>>, ApiError> {
    let rows = db::list_repositories(&state).await?;
    let hosts = db::active_hosts(&state).await?;
    let cache = state.repos.lock().unlocked.clone();
    Ok(Json(
        rows.iter()
            .map(|r| {
                summary_of(
                    r,
                    hosts.get(&r.id).copied().unwrap_or(0),
                    cache.get(&r.id).copied(),
                )
            })
            .collect(),
    ))
}

#[utoipa::path(
    get,
    tag = "Repositories",
    summary = "Get a repository",
    path = "/api/v1/repositories/{id}",
    params(("id" = String, Path, description = "Repository id")),
    responses(
        (status = 200, description = "Repository details", body = RepositoryDetail),
        (status = 404, description = "Unknown repository", body = ErrorBody),
    )
)]
async fn get_repository(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<RepositoryDetail>, ApiError> {
    let row = db::find_repository(&state, &id)
        .await?
        .ok_or_else(|| ApiError::NotFound("repository not found".into()))?;
    let unlocked = state.repos.lock().unlocked.get(&id).copied();
    let hosts = db::active_hosts(&state).await?;
    let checks = row
        .checks_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|e| ApiError::Internal(e.into()))?
        .unwrap_or_default();
    Ok(Json(RepositoryDetail {
        summary: summary_of(&row, hosts.get(&id).copied().unwrap_or(0), unlocked),
        location: LocationDto {
            backend: row.backend.clone(),
            options: row.options.clone(),
        },
        checks,
    }))
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_repositories))
        .routes(routes!(get_repository))
        .merge(wizard::routes())
        .merge(agent_key::routes())
}
