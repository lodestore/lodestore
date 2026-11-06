use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use lodestore_format::index::{hex32, parse_hex32};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::tokens::{Scoped, SnapshotsDelete, SnapshotsRead};
use crate::catalog::retention::{ForgetOutcome, forget_snapshot};
use crate::catalog::{SNAPSHOT_COLUMNS, SnapshotItem, item_from_row};
use crate::error::{ApiError, ErrorBody};
use crate::hosts::HostStatus;
use crate::hosts::labels::Label;
use crate::repos::db::list_repositories;
use crate::state::AppState;

pub mod tree;

pub(crate) const AGENTLESS: &str = "agentless:";

/// `repository_id`, `host_id`, `verified_at` and `hostname` of snapshot `$1`. `hostname` is
/// the name the restore host list shows: the agent's name when the snapshot
/// belongs to an enrolled agent, else the hostname stored in the snapshot.
pub(crate) const SNAPSHOT_HOST: &str = "SELECT s.repository_id, s.host_id, s.verified_at, \
    COALESCE(a.hostname, s.hostname) AS hostname \
    FROM snapshots s LEFT JOIN agents a ON a.id = s.host_id WHERE s.id = $1";
const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 200;

#[derive(Serialize, ToSchema)]
pub struct RestoreHost {
    pub key: String,
    pub host_id: Option<String>,
    pub hostname: String,
    pub agent: bool,
    pub status: Option<HostStatus>,
    pub labels: Vec<Label>,
    pub repository_id: Option<String>,
    pub repository_name: Option<String>,
    pub snapshot_count: u64,
    /// Server time of the newest signed snapshot; null when the host has none.
    pub latest_snapshot_ms: Option<i64>,
    /// Snapshots of this host that nothing signed; they are never offered as the latest.
    pub unverified_count: u64,
    /// Folders the agent restores into, as it reports them; the agent checks them itself.
    pub restore_roots: Vec<String>,
    /// Whether the agent allows restores in place; null when unknown.
    pub allow_in_place: Option<bool>,
}

fn reported_restore_settings(facts_json: Option<&str>) -> (Vec<String>, Option<bool>) {
    let restore = facts_json
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .and_then(|facts| facts.get("restore").cloned());
    let Some(restore) = restore else {
        return (Vec::new(), None);
    };
    let roots = restore["roots"]
        .as_array()
        .map(|roots| {
            roots
                .iter()
                .filter_map(|root| root.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    (roots, restore["allow_in_place"].as_bool())
}

#[derive(Serialize, ToSchema)]
pub struct RestoreSnapshot {
    #[serde(flatten)]
    pub item: SnapshotItem,
    pub repository_id: String,
    pub host_key: String,
}

#[derive(Serialize, ToSchema)]
pub struct RestoreSnapshotPage {
    pub items: Vec<RestoreSnapshot>,
    pub total: u64,
    pub next: Option<String>,
    /// Id of the newest signed snapshot of the host by server time, whatever the page; null when the host has none.
    pub latest_verified_id: Option<String>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RestoreQuery {
    /// Host key from the host list: an agent id or `agentless:{hostname}`.
    pub host: Option<String>,
    /// Cursor from the previous page.
    pub before: Option<String>,
    /// Page size, 1 to 200; 50 by default.
    pub limit: Option<u32>,
}

fn count(row: &sqlx::any::AnyRow, column: &str) -> Result<u64, ApiError> {
    Ok(row.try_get::<i64, _>(column)?.max(0) as u64)
}

#[utoipa::path(
    get,
    tag = "Restore",
    summary = "List hosts to restore from",
    security(("session" = []), ("bearer" = ["snapshots:read"])),
    path = "/api/v1/restore/hosts",
    responses((status = 200, description = "Hosts with an agent, then hosts known only from snapshots", body = Vec<RestoreHost>))
)]
async fn list_restore_hosts(
    State(state): State<AppState>,
    _auth: Scoped<SnapshotsRead>,
) -> Result<Json<Vec<RestoreHost>>, ApiError> {
    let names: HashMap<String, String> = list_repositories(&state)
        .await?
        .into_iter()
        .map(|r| (r.id, r.name))
        .collect();
    let mut hosts = Vec::new();
    let agents = sqlx::query(
        "SELECT a.id, a.hostname, a.repository_id, a.labels_json, a.status, a.facts_json, \
         COUNT(s.id) AS n, \
         MAX(CASE WHEN s.verified_at IS NOT NULL THEN s.received_at END) AS latest, \
         CAST(COALESCE(SUM(CASE WHEN s.id IS NOT NULL AND s.verified_at IS NULL THEN 1 ELSE 0 END), 0) AS BIGINT) AS unverified \
         FROM agents a LEFT JOIN snapshots s ON s.host_id = a.id \
         GROUP BY a.id, a.hostname, a.repository_id, a.labels_json, a.status, a.facts_json \
         ORDER BY a.hostname, a.id",
    )
    .fetch_all(&state.db.pool)
    .await?;
    for row in &agents {
        let id: String = row.try_get("id")?;
        let status = if row.try_get::<String, _>("status")? == "revoked" {
            HostStatus::Revoked
        } else if state.registry.is_online(&id) {
            HostStatus::Online
        } else {
            HostStatus::Offline
        };
        let repository_id: String = row.try_get("repository_id")?;
        let (restore_roots, allow_in_place) =
            reported_restore_settings(row.try_get::<Option<String>, _>("facts_json")?.as_deref());
        hosts.push(RestoreHost {
            key: id.clone(),
            host_id: Some(id),
            hostname: row.try_get("hostname")?,
            agent: true,
            status: Some(status),
            labels: serde_json::from_str(&row.try_get::<String, _>("labels_json")?)
                .map_err(|e| ApiError::Internal(e.into()))?,
            repository_name: names.get(&repository_id).cloned(),
            repository_id: Some(repository_id),
            snapshot_count: count(row, "n")?,
            latest_snapshot_ms: row.try_get("latest")?,
            unverified_count: count(row, "unverified")?,
            restore_roots,
            allow_in_place,
        });
    }
    let groups = sqlx::query(
        "SELECT hostname, COUNT(*) AS n, \
         MAX(CASE WHEN verified_at IS NOT NULL THEN received_at END) AS latest, \
         CAST(SUM(CASE WHEN verified_at IS NULL THEN 1 ELSE 0 END) AS BIGINT) AS unverified, \
         MIN(repository_id) AS repo_first, MAX(repository_id) AS repo_last \
         FROM snapshots WHERE host_id IS NULL GROUP BY hostname ORDER BY hostname",
    )
    .fetch_all(&state.db.pool)
    .await?;
    for row in &groups {
        let hostname: String = row.try_get("hostname")?;
        let first: String = row.try_get("repo_first")?;
        let last: String = row.try_get("repo_last")?;
        let repository_id = (first == last).then_some(first);
        hosts.push(RestoreHost {
            key: format!("{AGENTLESS}{hostname}"),
            host_id: None,
            hostname,
            agent: false,
            status: None,
            labels: Vec::new(),
            repository_name: repository_id.as_ref().and_then(|id| names.get(id).cloned()),
            repository_id,
            snapshot_count: count(row, "n")?,
            latest_snapshot_ms: row.try_get("latest")?,
            unverified_count: count(row, "unverified")?,
            restore_roots: Vec::new(),
            allow_in_place: None,
        });
    }
    Ok(Json(hosts))
}

enum HostFilter {
    Agent(String),
    Hostname(String),
}

impl HostFilter {
    fn condition(&self) -> &'static str {
        match self {
            HostFilter::Agent(_) => "host_id = $1",
            HostFilter::Hostname(_) => "host_id IS NULL AND hostname = $1",
        }
    }

    fn value(&self) -> &str {
        match self {
            HostFilter::Agent(v) | HostFilter::Hostname(v) => v,
        }
    }
}

async fn resolve_host(state: &AppState, key: &str) -> Result<HostFilter, ApiError> {
    let unknown = || ApiError::NotFound("host not found".into());
    if let Some(hostname) = key.strip_prefix(AGENTLESS) {
        sqlx::query("SELECT 1 FROM snapshots WHERE host_id IS NULL AND hostname = $1 LIMIT 1")
            .bind(hostname)
            .fetch_optional(&state.db.pool)
            .await?
            .ok_or_else(unknown)?;
        return Ok(HostFilter::Hostname(hostname.to_string()));
    }
    sqlx::query("SELECT 1 FROM agents WHERE id = $1")
        .bind(key)
        .fetch_optional(&state.db.pool)
        .await?
        .ok_or_else(unknown)?;
    Ok(HostFilter::Agent(key.to_string()))
}

fn parse_cursor(cursor: &str) -> Result<(i64, String), ApiError> {
    let invalid = || ApiError::Validation("The page cursor is not valid.".into());
    let (time, id) = cursor.split_once('.').ok_or_else(invalid)?;
    let time = time.parse::<i64>().map_err(|_| invalid())?;
    let id = hex32(&parse_hex32(id).ok_or_else(invalid)?);
    Ok((time, id))
}

fn restore_snapshot(row: &sqlx::any::AnyRow) -> Result<RestoreSnapshot, ApiError> {
    let host_key = match row.try_get::<Option<String>, _>("host_id")? {
        Some(id) => id,
        None => format!("{AGENTLESS}{}", row.try_get::<String, _>("hostname")?),
    };
    Ok(RestoreSnapshot {
        item: item_from_row(row)?,
        repository_id: row.try_get("repository_id")?,
        host_key,
    })
}

#[utoipa::path(
    get,
    tag = "Restore",
    summary = "List the snapshots of a host, newest first",
    security(("session" = []), ("bearer" = ["snapshots:read"])),
    path = "/api/v1/restore/snapshots",
    params(RestoreQuery),
    responses(
        (status = 200, description = "One page of snapshots", body = RestoreSnapshotPage),
        (status = 404, description = "Unknown host", body = ErrorBody),
        (status = 422, description = "Missing host, bad cursor or limit", body = ErrorBody),
    )
)]
async fn list_restore_snapshots(
    State(state): State<AppState>,
    _auth: Scoped<SnapshotsRead>,
    Query(query): Query<RestoreQuery>,
) -> Result<Json<RestoreSnapshotPage>, ApiError> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ApiError::Validation(format!(
            "The limit must be between 1 and {MAX_LIMIT}."
        )));
    }
    let cursor = query.before.as_deref().map(parse_cursor).transpose()?;
    let key = query
        .host
        .filter(|h| !h.is_empty())
        .ok_or_else(|| ApiError::Validation("The host is required.".into()))?;
    let filter = resolve_host(&state, &key).await?;

    let total = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) AS n FROM snapshots WHERE {}",
        filter.condition()
    )))
    .bind(filter.value())
    .fetch_one(&state.db.pool)
    .await?;
    let total = count(&total, "n")?;

    let select = format!(
        "SELECT {SNAPSHOT_COLUMNS}, repository_id, host_id FROM snapshots WHERE {}",
        filter.condition()
    );
    let rows = match cursor {
        Some((time, id)) => {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "{select} AND (received_at < $2 OR (received_at = $2 AND id < $3)) \
                 ORDER BY received_at DESC, id DESC LIMIT $4"
            )))
            .bind(filter.value())
            .bind(time)
            .bind(id)
            .bind(i64::from(limit) + 1)
            .fetch_all(&state.db.pool)
            .await?
        }
        None => {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "{select} ORDER BY received_at DESC, id DESC LIMIT $2"
            )))
            .bind(filter.value())
            .bind(i64::from(limit) + 1)
            .fetch_all(&state.db.pool)
            .await?
        }
    };

    let mut items = rows
        .iter()
        .map(restore_snapshot)
        .collect::<Result<Vec<_>, _>>()?;
    let more = items.len() > limit as usize;
    items.truncate(limit as usize);
    let next = items
        .last()
        .filter(|_| more)
        .map(|last| format!("{}.{}", last.item.received_at, last.item.id));
    let latest_verified_id = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT id FROM snapshots WHERE {} AND verified_at IS NOT NULL \
         ORDER BY received_at DESC, id DESC LIMIT 1",
        filter.condition()
    )))
    .bind(filter.value())
    .fetch_optional(&state.db.pool)
    .await?
    .map(|row| row.try_get("id"))
    .transpose()?;
    Ok(Json(RestoreSnapshotPage {
        items,
        total,
        next,
        latest_verified_id,
    }))
}

#[utoipa::path(
    get,
    tag = "Restore",
    summary = "Get one snapshot",
    security(("session" = []), ("bearer" = ["snapshots:read"])),
    path = "/api/v1/snapshots/{id}",
    params(("id" = String, Path, description = "Snapshot id")),
    responses(
        (status = 200, description = "The snapshot with its repository and host key", body = RestoreSnapshot),
        (status = 404, description = "Unknown snapshot", body = ErrorBody),
    )
)]
async fn get_snapshot(
    State(state): State<AppState>,
    _auth: Scoped<SnapshotsRead>,
    Path(id): Path<String>,
) -> Result<Json<RestoreSnapshot>, ApiError> {
    let unknown = || ApiError::NotFound("snapshot not found".into());
    let id = hex32(&parse_hex32(&id).ok_or_else(unknown)?);
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {SNAPSHOT_COLUMNS}, repository_id, host_id FROM snapshots WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(&state.db.pool)
    .await?
    .ok_or_else(unknown)?;
    Ok(Json(restore_snapshot(&row)?))
}

#[derive(Serialize, ToSchema)]
pub struct DeletedSnapshot {
    pub id: String,
    pub storage_kept: bool,
    pub reason: Option<String>,
}

#[utoipa::path(
    delete,
    tag = "Restore",
    summary = "Delete a snapshot",
    security(("session" = [], "csrf" = []), ("bearer" = ["snapshots:delete"])),
    path = "/api/v1/snapshots/{id}",
    params(("id" = String, Path, description = "Snapshot id")),
    responses(
        (status = 200, description = "The snapshot left the catalog; storage_kept tells whether the manifest is still in the bucket", body = DeletedSnapshot),
        (status = 403, description = "The token needs the snapshots:delete scope", body = ErrorBody),
        (status = 404, description = "Unknown snapshot", body = ErrorBody),
        (status = 502, description = "The repository storage is unavailable", body = ErrorBody),
    )
)]
async fn delete_snapshot(
    State(state): State<AppState>,
    auth: Scoped<SnapshotsDelete>,
    Path(id): Path<String>,
) -> Result<Json<DeletedSnapshot>, ApiError> {
    let unknown = || ApiError::NotFound("snapshot not found".into());
    let id = hex32(&parse_hex32(&id).ok_or_else(unknown)?);
    let repository_id: String = sqlx::query("SELECT repository_id FROM snapshots WHERE id = $1")
        .bind(&id)
        .fetch_optional(&state.db.pool)
        .await?
        .ok_or_else(unknown)?
        .try_get("repository_id")?;
    let outcome = forget_snapshot(&state, &repository_id, &id).await?;
    tracing::info!(snapshot = %id, actor = %auth.actor(), "snapshot delete requested");
    match outcome {
        ForgetOutcome::Deleted => Ok(Json(DeletedSnapshot {
            id,
            storage_kept: false,
            reason: None,
        })),
        ForgetOutcome::Blocked { reason } => Ok(Json(DeletedSnapshot {
            id,
            storage_kept: true,
            reason: Some(reason),
        })),
        ForgetOutcome::Failed { reason } => Err(ApiError::Backend(format!(
            "the repository storage failed: {reason}"
        ))),
    }
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_restore_hosts))
        .routes(routes!(list_restore_snapshots))
        .routes(routes!(get_snapshot, delete_snapshot))
        .merge(tree::routes())
}
