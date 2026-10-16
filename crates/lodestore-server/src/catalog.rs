use std::collections::HashSet;
use std::time::Duration;

use anyhow::Context;
use axum::Json;
use axum::extract::{Path, Query, State};
use lodestore_format::index::{hex32, parse_hex32};
use lodestore_format::keys::{MasterKey, ReadKeys};
use lodestore_format::pb;
use lodestore_format::snapshot::{open_snapshot, snapshot_object_key};
use lodestore_format::store::{BlobStore, Store};
use lodestore_format::{Error as FormatError, Result as FormatResult};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tokio_util::sync::CancellationToken;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::AuthUser;
use crate::db::now_ms;
use crate::error::{ApiError, ErrorBody};
use crate::repos::checks::open_store;
use crate::repos::db::{get_repository, list_repositories, server_access};
use crate::state::AppState;

const SYNC_INTERVAL: Duration = Duration::from_secs(300);
const PAGE_SIZE: i64 = 50;
const SNAPSHOT_PREFIX: &str = "snapshots/";

pub struct JobLink<'a> {
    pub job_id: &'a str,
    pub host_id: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncReport {
    pub added: u64,
    pub skipped: u64,
    pub total: u64,
}

struct Source {
    store: Store,
    keys: ReadKeys,
}

async fn open_source(state: &AppState, repository_id: &str) -> anyhow::Result<Source> {
    let access = server_access(state, repository_id).await?;
    let master: [u8; 32] = access
        .master
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("master secret must be 32 bytes"))?;
    Ok(Source {
        store: open_store(&access.location, access.server_creds.as_ref())?,
        keys: ReadKeys::derive(MasterKey::from_bytes(master), access.format_repo_id),
    })
}

async fn read_snapshot(source: &Source, id: &[u8; 32]) -> FormatResult<pb::Snapshot> {
    let bytes = source.store.get(&snapshot_object_key(id)).await?;
    open_snapshot(
        source.keys.hpke_private_key(),
        source.keys.repo_id(),
        id,
        &bytes,
    )
}

fn is_unreadable(error: &FormatError) -> bool {
    matches!(
        error,
        FormatError::Corrupt { .. } | FormatError::UnsupportedVersion { .. }
    )
}

fn clamp(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn tag_value<'a>(tags: &'a [String], key: &str) -> Option<&'a str> {
    tags.iter()
        .find_map(|t| t.strip_prefix(key)?.strip_prefix('='))
}

async fn bind_host(
    state: &AppState,
    repository_id: &str,
    snapshot: &pb::Snapshot,
    claim: Option<&str>,
) -> anyhow::Result<Option<String>> {
    if let Some(claim) = claim {
        let row = sqlx::query("SELECT id FROM agents WHERE id = $1 AND repository_id = $2")
            .bind(claim)
            .bind(repository_id)
            .fetch_optional(&state.db.pool)
            .await?;
        if let Some(row) = row {
            return Ok(Some(row.try_get("id")?));
        }
    }
    let rows = sqlx::query(
        "SELECT id FROM agents WHERE repository_id = $1 AND hostname = $2 AND status <> 'revoked' \
         LIMIT 2",
    )
    .bind(repository_id)
    .bind(text(&snapshot.hostname))
    .fetch_all(&state.db.pool)
    .await?;
    match rows.as_slice() {
        [only] => Ok(Some(only.try_get("id")?)),
        _ => Ok(None),
    }
}

async fn insert_snapshot(
    state: &AppState,
    repository_id: &str,
    id: &[u8; 32],
    snapshot: &pb::Snapshot,
    link: Option<&JobLink<'_>>,
) -> anyhow::Result<bool> {
    let claim = std::str::from_utf8(&snapshot.agent_id)
        .ok()
        .filter(|c| !c.is_empty());
    let host_id = match link {
        Some(link) => Some(link.host_id.to_string()),
        None => bind_host(state, repository_id, snapshot, claim).await?,
    };
    let paths: Vec<String> = snapshot.paths.iter().map(|p| text(p)).collect();
    let time = snapshot.time.unwrap_or_default();
    let stats = SnapshotStatsDto::from(&snapshot.stats.unwrap_or_default());
    let parent = <[u8; 32]>::try_from(snapshot.parent.as_slice())
        .ok()
        .map(|p| hex32(&p));
    let result = sqlx::query(
        "INSERT INTO snapshots (id, repository_id, host_id, hostname, time_ms, paths_json, \
         tags_json, policy_name, stdin_name, job_id, parent_id, incomplete, errors_total, \
         warnings_total, stats_json, agent_id_claim, discovered_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(hex32(id))
    .bind(repository_id)
    .bind(&host_id)
    .bind(text(&snapshot.hostname))
    .bind(
        time.seconds
            .saturating_mul(1000)
            .saturating_add(i64::from(time.nanos) / 1_000_000),
    )
    .bind(serde_json::to_string(&paths)?)
    .bind(serde_json::to_string(&snapshot.tags)?)
    .bind(tag_value(&snapshot.tags, "policy"))
    .bind(tag_value(&snapshot.tags, "stdin"))
    .bind(link.map(|l| l.job_id))
    .bind(parent)
    .bind(i64::from(snapshot.incomplete))
    .bind(clamp(snapshot.errors_total))
    .bind(clamp(snapshot.warnings_total))
    .bind(serde_json::to_string(&stats)?)
    .bind(claim)
    .bind(now_ms())
    .execute(&state.db.pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn ingest_snapshot(
    state: &AppState,
    repository_id: &str,
    snapshot_id: &str,
    link: Option<JobLink<'_>>,
) -> anyhow::Result<bool> {
    let id = parse_hex32(snapshot_id).context("snapshot id must be 64 hex characters")?;
    let known = sqlx::query("SELECT 1 FROM snapshots WHERE id = $1 AND repository_id = $2")
        .bind(hex32(&id))
        .bind(repository_id)
        .fetch_optional(&state.db.pool)
        .await?
        .is_some();
    if known {
        if let Some(link) = &link {
            link_snapshot(state, repository_id, &id, link).await?;
        }
        return Ok(false);
    }
    let source = open_source(state, repository_id).await?;
    let snapshot = read_snapshot(&source, &id).await?;
    if let Some(link) = &link
        && std::str::from_utf8(&snapshot.agent_id)
            .is_ok_and(|claim| !claim.is_empty() && claim != link.host_id)
    {
        return Err(LinkRefused.into());
    }
    let inserted = insert_snapshot(state, repository_id, &id, &snapshot, link.as_ref()).await?;
    if !inserted && let Some(link) = &link {
        link_snapshot(state, repository_id, &id, link).await?;
    }
    Ok(inserted)
}

#[derive(Debug, thiserror::Error)]
#[error("the snapshot belongs to another host or job")]
pub struct LinkRefused;

async fn link_snapshot(
    state: &AppState,
    repository_id: &str,
    id: &[u8; 32],
    link: &JobLink<'_>,
) -> anyhow::Result<()> {
    let done = sqlx::query(
        "UPDATE snapshots SET job_id = $1, host_id = $2 WHERE id = $3 AND repository_id = $4 \
         AND (job_id IS NULL OR job_id = $1) AND (host_id IS NULL OR host_id = $2)",
    )
    .bind(link.job_id)
    .bind(link.host_id)
    .bind(hex32(id))
    .bind(repository_id)
    .execute(&state.db.pool)
    .await?;
    if done.rows_affected() == 0 {
        return Err(LinkRefused.into());
    }
    Ok(())
}

async fn sync_inner(
    state: &AppState,
    repository_id: &str,
) -> anyhow::Result<(SyncReport, Vec<String>)> {
    let source = open_source(state, repository_id).await?;
    let listed = source.store.list(SNAPSHOT_PREFIX).await?;
    let known: HashSet<String> = sqlx::query("SELECT id FROM snapshots WHERE repository_id = $1")
        .bind(repository_id)
        .fetch_all(&state.db.pool)
        .await?
        .iter()
        .map(|r| r.try_get("id"))
        .collect::<Result<_, _>>()?;
    let mut added = 0;
    let mut skipped = Vec::new();
    for entry in listed {
        let Some(id) = entry
            .key
            .strip_prefix(SNAPSHOT_PREFIX)
            .and_then(parse_hex32)
        else {
            skipped.push(entry.key);
            continue;
        };
        if known.contains(&hex32(&id)) {
            continue;
        }
        match read_snapshot(&source, &id).await {
            Ok(snapshot) => {
                if insert_snapshot(state, repository_id, &id, &snapshot, None).await? {
                    added += 1;
                }
            }
            Err(e) if is_unreadable(&e) => skipped.push(entry.key),
            Err(e) => {
                tracing::warn!(key = %entry.key, error = %e, "reading a snapshot failed");
                skipped.push(entry.key);
            }
        }
    }
    let total: i64 = sqlx::query("SELECT COUNT(*) AS n FROM snapshots WHERE repository_id = $1")
        .bind(repository_id)
        .fetch_one(&state.db.pool)
        .await?
        .try_get("n")?;
    let report = SyncReport {
        added,
        skipped: skipped.len() as u64,
        total: total.max(0) as u64,
    };
    Ok((report, skipped))
}

async fn record_state(
    state: &AppState,
    repository_id: &str,
    error: Option<&str>,
    skipped: &[String],
) -> anyhow::Result<()> {
    let at = error.is_none().then(now_ms);
    let skipped_json = error
        .is_none()
        .then(|| serde_json::to_string(skipped))
        .transpose()?;
    sqlx::query(
        "INSERT INTO catalog_state (repository_id, last_sync_at, last_error, skipped_json) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (repository_id) DO UPDATE SET \
         last_sync_at = COALESCE(excluded.last_sync_at, catalog_state.last_sync_at), \
         last_error = excluded.last_error, \
         skipped_json = COALESCE(excluded.skipped_json, catalog_state.skipped_json)",
    )
    .bind(repository_id)
    .bind(at)
    .bind(error)
    .bind(skipped_json)
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

pub async fn sync_repository(state: &AppState, repository_id: &str) -> anyhow::Result<SyncReport> {
    match sync_inner(state, repository_id).await {
        Ok((report, skipped)) => {
            record_state(state, repository_id, None, &skipped).await?;
            Ok(report)
        }
        Err(error) => {
            if let Err(e) = record_state(state, repository_id, Some(&error.to_string()), &[]).await
            {
                tracing::warn!(error = %e, "recording the catalog sync error failed");
            }
            Err(error)
        }
    }
}

pub async fn sync_loop(state: AppState, stop: CancellationToken) -> anyhow::Result<()> {
    let mut tick = tokio::time::interval(SYNC_INTERVAL);
    loop {
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            _ = tick.tick() => {}
        }
        let repositories = match list_repositories(&state).await {
            Ok(all) => all,
            Err(error) => {
                tracing::warn!(%error, "listing repositories for the catalog sync failed");
                continue;
            }
        };
        for repo in repositories {
            if let Err(error) = sync_repository(&state, &repo.id).await {
                tracing::warn!(repository = %repo.name, %error, "catalog sync failed");
            }
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct CatalogSyncResult {
    pub added: u64,
    pub skipped: u64,
    pub total: u64,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct SnapshotStatsDto {
    pub files_new: u64,
    pub files_changed: u64,
    pub files_unchanged: u64,
    pub dirs: u64,
    pub bytes_total: u64,
    pub bytes_read: u64,
    pub bytes_added: u64,
}

impl From<&pb::SnapshotStats> for SnapshotStatsDto {
    fn from(s: &pb::SnapshotStats) -> Self {
        SnapshotStatsDto {
            files_new: s.files_new,
            files_changed: s.files_changed,
            files_unchanged: s.files_unchanged,
            dirs: s.dirs,
            bytes_total: s.bytes_total,
            bytes_read: s.bytes_read,
            bytes_added: s.bytes_added,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct SnapshotItem {
    pub id: String,
    pub time: i64,
    pub hostname: String,
    pub paths: Vec<String>,
    pub tags: Vec<String>,
    pub policy_name: Option<String>,
    pub stdin_name: Option<String>,
    pub job_id: Option<String>,
    pub parent_id: Option<String>,
    pub incomplete: bool,
    pub errors_total: u64,
    pub warnings_total: u64,
    pub stats: SnapshotStatsDto,
}

#[derive(Serialize, ToSchema)]
pub struct SnapshotPage {
    pub items: Vec<SnapshotItem>,
    pub total: u64,
    pub page: u64,
    pub page_size: u64,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PageQuery {
    /// One-based page number.
    pub page: Option<u64>,
}

fn sync_error(error: anyhow::Error) -> ApiError {
    if error.downcast_ref::<FormatError>().is_some() {
        ApiError::Backend(format!("the repository storage failed: {error}"))
    } else {
        ApiError::Internal(error)
    }
}

#[utoipa::path(
    post,
    tag = "Snapshots",
    summary = "Read new snapshots from storage",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/repositories/{id}/catalog/sync",
    params(("id" = String, Path, description = "Repository id")),
    responses(
        (status = 200, description = "Snapshots found in the bucket", body = CatalogSyncResult),
        (status = 404, description = "Unknown repository", body = ErrorBody),
        (status = 502, description = "The repository storage is unavailable", body = ErrorBody),
    )
)]
async fn sync_catalog(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<CatalogSyncResult>, ApiError> {
    get_repository(&state, &id)
        .await?
        .ok_or_else(|| ApiError::NotFound("repository not found".into()))?;
    let report = sync_repository(&state, &id).await.map_err(sync_error)?;
    Ok(Json(CatalogSyncResult {
        added: report.added,
        skipped: report.skipped,
        total: report.total,
    }))
}

fn item_from_row(row: &sqlx::any::AnyRow) -> Result<SnapshotItem, ApiError> {
    let parse = |e: serde_json::Error| ApiError::Internal(e.into());
    Ok(SnapshotItem {
        id: row.try_get("id")?,
        time: row.try_get("time_ms")?,
        hostname: row.try_get("hostname")?,
        paths: serde_json::from_str(&row.try_get::<String, _>("paths_json")?).map_err(parse)?,
        tags: serde_json::from_str(&row.try_get::<String, _>("tags_json")?).map_err(parse)?,
        policy_name: row.try_get("policy_name")?,
        stdin_name: row.try_get("stdin_name")?,
        job_id: row.try_get("job_id")?,
        parent_id: row.try_get("parent_id")?,
        incomplete: row.try_get::<i64, _>("incomplete")? != 0,
        errors_total: row.try_get::<i64, _>("errors_total")?.max(0) as u64,
        warnings_total: row.try_get::<i64, _>("warnings_total")?.max(0) as u64,
        stats: serde_json::from_str(&row.try_get::<String, _>("stats_json")?).map_err(parse)?,
    })
}

#[utoipa::path(
    get,
    tag = "Snapshots",
    summary = "List snapshots of a host",
    path = "/api/v1/hosts/{id}/snapshots",
    params(("id" = String, Path, description = "Host id"), PageQuery),
    responses(
        (status = 200, description = "Snapshots of the host, newest first", body = SnapshotPage),
        (status = 404, description = "Unknown host", body = ErrorBody),
    )
)]
async fn list_host_snapshots(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<String>,
    Query(query): Query<PageQuery>,
) -> Result<Json<SnapshotPage>, ApiError> {
    sqlx::query("SELECT 1 FROM agents WHERE id = $1")
        .bind(&id)
        .fetch_optional(&state.db.pool)
        .await?
        .ok_or_else(|| ApiError::NotFound("host not found".into()))?;
    let page = query.page.unwrap_or(1).max(1);
    let total: i64 = sqlx::query("SELECT COUNT(*) AS n FROM snapshots WHERE host_id = $1")
        .bind(&id)
        .fetch_one(&state.db.pool)
        .await?
        .try_get("n")?;
    let rows = sqlx::query(
        "SELECT id, time_ms, hostname, paths_json, tags_json, policy_name, stdin_name, job_id, \
         parent_id, incomplete, errors_total, warnings_total, stats_json FROM snapshots \
         WHERE host_id = $1 ORDER BY time_ms DESC, id LIMIT $2 OFFSET $3",
    )
    .bind(&id)
    .bind(PAGE_SIZE)
    .bind(clamp(page - 1).saturating_mul(PAGE_SIZE))
    .fetch_all(&state.db.pool)
    .await?;
    Ok(Json(SnapshotPage {
        items: rows
            .iter()
            .map(item_from_row)
            .collect::<Result<Vec<_>, _>>()?,
        total: total.max(0) as u64,
        page,
        page_size: PAGE_SIZE as u64,
    }))
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(sync_catalog))
        .routes(routes!(list_host_snapshots))
}
