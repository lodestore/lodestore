use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use lodestore_format::index::{hex32, parse_hex32};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::AuthUser;
use crate::catalog::{SNAPSHOT_COLUMNS, SnapshotItem, item_from_row};
use crate::error::{ApiError, ErrorBody};
use crate::hosts::HostStatus;
use crate::hosts::labels::Label;
use crate::repos::db::list_repositories;
use crate::state::AppState;

pub mod tree;

pub(crate) const AGENTLESS: &str = "agentless:";

/// `repository_id`, `host_id` and `hostname` of snapshot `$1`. `hostname` is
/// the name the restore host list shows: the agent's name when the snapshot
/// belongs to an enrolled agent, else the hostname stored in the snapshot.
pub(crate) const SNAPSHOT_HOST: &str = "SELECT s.repository_id, s.host_id, \
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
    pub latest_snapshot_ms: Option<i64>,
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
    path = "/api/v1/restore/hosts",
    responses((status = 200, description = "Hosts with an agent, then hosts known only from snapshots", body = Vec<RestoreHost>))
)]
async fn list_restore_hosts(
    State(state): State<AppState>,
    _user: AuthUser,
) -> Result<Json<Vec<RestoreHost>>, ApiError> {
    let names: HashMap<String, String> = list_repositories(&state)
        .await?
        .into_iter()
        .map(|r| (r.id, r.name))
        .collect();
    let mut hosts = Vec::new();
    let agents = sqlx::query(
        "SELECT a.id, a.hostname, a.repository_id, a.labels_json, a.status, \
         COUNT(s.id) AS n, MAX(s.time_ms) AS latest \
         FROM agents a LEFT JOIN snapshots s ON s.host_id = a.id \
         GROUP BY a.id, a.hostname, a.repository_id, a.labels_json, a.status \
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
        });
    }
    let groups = sqlx::query(
        "SELECT hostname, COUNT(*) AS n, MAX(time_ms) AS latest, \
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
    _user: AuthUser,
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
                "{select} AND (time_ms < $2 OR (time_ms = $2 AND id < $3)) \
                 ORDER BY time_ms DESC, id DESC LIMIT $4"
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
                "{select} ORDER BY time_ms DESC, id DESC LIMIT $2"
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
        .map(|last| format!("{}.{}", last.item.time, last.item.id));
    Ok(Json(RestoreSnapshotPage { items, total, next }))
}

#[utoipa::path(
    get,
    tag = "Restore",
    summary = "Get one snapshot",
    path = "/api/v1/snapshots/{id}",
    params(("id" = String, Path, description = "Snapshot id")),
    responses(
        (status = 200, description = "The snapshot with its repository and host key", body = RestoreSnapshot),
        (status = 404, description = "Unknown snapshot", body = ErrorBody),
    )
)]
async fn get_snapshot(
    State(state): State<AppState>,
    _user: AuthUser,
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

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_restore_hosts))
        .routes(routes!(list_restore_snapshots))
        .routes(routes!(get_snapshot))
        .merge(tree::routes())
}
