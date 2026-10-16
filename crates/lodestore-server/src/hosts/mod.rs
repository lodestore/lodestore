pub mod labels;
pub mod tokens;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::header::{CACHE_CONTROL, HeaderName};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::AuthUser;
use crate::db::now_ms;
use crate::error::{ApiError, ErrorBody};
use crate::hosts::labels::Label;
use crate::repos::db::get_repository;
use crate::state::AppState;

#[derive(Deserialize, ToSchema)]
pub struct CreateJoinToken {
    pub repository_id: String,
    #[serde(default)]
    pub labels: Vec<Label>,
}

#[derive(Serialize, ToSchema)]
pub struct JoinTokenResponse {
    pub id: String,
    pub token: String,
    pub command: String,
    pub expires_at: i64,
    pub server_time: i64,
}

#[derive(Serialize, ToSchema, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HostStatus {
    Online,
    Offline,
    Revoked,
}

#[derive(Serialize, ToSchema)]
pub struct Host {
    pub id: String,
    pub hostname: String,
    pub status: HostStatus,
    pub last_seen_at: Option<i64>,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub distro: Option<String>,
    pub labels: Vec<Label>,
    pub repository_id: String,
    pub cert_not_after: i64,
    pub enrolled_at: i64,
}

#[utoipa::path(
    post,
    tag = "Hosts",
    summary = "Create a join token",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/join-tokens",
    request_body = CreateJoinToken,
    responses(
        (status = 201, description = "A one-time join token and the install command", body = JoinTokenResponse),
        (status = 404, description = "Unknown repository", body = ErrorBody),
        (status = 422, description = "Invalid labels", body = ErrorBody),
    )
)]
async fn create_join_token(
    State(state): State<AppState>,
    _user: AuthUser,
    Json(req): Json<CreateJoinToken>,
) -> Result<
    (
        StatusCode,
        [(HeaderName, &'static str); 1],
        Json<JoinTokenResponse>,
    ),
    ApiError,
> {
    labels::validate(&req.labels)?;
    get_repository(&state, &req.repository_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("repository not found".into()))?;
    let issued = tokens::issue(&state, &req.repository_id, &req.labels).await?;
    let command = crate::install::install_command(&state.config.public_url, &issued.token);
    Ok((
        StatusCode::CREATED,
        [(CACHE_CONTROL, "no-store")],
        Json(JoinTokenResponse {
            id: issued.id,
            token: issued.token,
            command,
            expires_at: issued.expires_at,
            server_time: now_ms(),
        }),
    ))
}

const HOST_COLUMNS: &str = "a.id, a.hostname, a.repository_id, a.labels_json, a.os, a.arch, a.version, \
     a.distro, a.status, a.enrolled_at, a.cert_not_after, a.last_seen_at";

fn host_from_row(state: &AppState, row: &sqlx::any::AnyRow) -> Result<Host, ApiError> {
    let id: String = row.try_get("id")?;
    let status = if row.try_get::<String, _>("status")? == "revoked" {
        HostStatus::Revoked
    } else if state.registry.is_online(&id) {
        HostStatus::Online
    } else {
        HostStatus::Offline
    };
    Ok(Host {
        labels: serde_json::from_str(&row.try_get::<String, _>("labels_json")?)
            .map_err(|e| ApiError::Internal(e.into()))?,
        hostname: row.try_get("hostname")?,
        repository_id: row.try_get("repository_id")?,
        os: row.try_get("os")?,
        arch: row.try_get("arch")?,
        version: row.try_get("version")?,
        distro: row.try_get("distro")?,
        enrolled_at: row.try_get("enrolled_at")?,
        cert_not_after: row.try_get("cert_not_after")?,
        last_seen_at: row.try_get("last_seen_at")?,
        id,
        status,
    })
}

#[derive(Serialize, ToSchema)]
pub struct HostMount {
    pub path: String,
    pub fs_type: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Serialize, ToSchema)]
pub struct HostDetail {
    #[serde(flatten)]
    pub host: Host,
    pub kernel: Option<String>,
    pub uptime_s: Option<i64>,
    pub mounts: Vec<HostMount>,
    pub cert_issued_at: i64,
    pub next_renewal_at: i64,
    pub repository_name: String,
}

async fn load_detail(state: &AppState, id: &str) -> Result<HostDetail, ApiError> {
    let sql = format!(
        "SELECT {HOST_COLUMNS}, a.kernel, a.facts_json, a.cert_issued_at, r.name AS repository_name \
         FROM agents a JOIN repositories r ON r.id = a.repository_id WHERE a.id = $1"
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(&state.db.pool)
        .await?
        .ok_or_else(|| ApiError::NotFound("host not found".into()))?;
    let host = host_from_row(state, &row)?;
    let facts: Value = row
        .try_get::<Option<String>, _>("facts_json")?
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null);
    let mounts = facts["mounts"]
        .as_array()
        .map(|all| {
            all.iter()
                .map(|m| HostMount {
                    path: m["path"].as_str().unwrap_or_default().to_string(),
                    fs_type: m["fs_type"].as_str().unwrap_or_default().to_string(),
                    total_bytes: m["total_bytes"].as_u64().unwrap_or(0),
                    free_bytes: m["free_bytes"].as_u64().unwrap_or(0),
                })
                .collect()
        })
        .unwrap_or_default();
    let cert_issued_at: i64 = row.try_get("cert_issued_at")?;
    Ok(HostDetail {
        next_renewal_at: cert_issued_at + (host.cert_not_after - cert_issued_at) * 2 / 3,
        kernel: row.try_get("kernel")?,
        uptime_s: facts["uptime_s"].as_i64(),
        mounts,
        cert_issued_at,
        repository_name: row.try_get("repository_name")?,
        host,
    })
}

#[utoipa::path(
    get,
    tag = "Hosts",
    summary = "List hosts",
    path = "/api/v1/hosts",
    responses((status = 200, description = "All hosts", body = Vec<Host>))
)]
async fn list_hosts(
    State(state): State<AppState>,
    _user: AuthUser,
) -> Result<Json<Vec<Host>>, ApiError> {
    let sql = format!("SELECT {HOST_COLUMNS} FROM agents a ORDER BY a.enrolled_at, a.id");
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(&state.db.pool)
        .await?;
    let hosts = rows
        .iter()
        .map(|row| host_from_row(&state, row))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(hosts))
}

#[utoipa::path(
    get,
    tag = "Hosts",
    summary = "Get a host",
    path = "/api/v1/hosts/{id}",
    params(("id" = String, Path, description = "Host id")),
    responses(
        (status = 200, description = "Host details", body = HostDetail),
        (status = 404, description = "Unknown host", body = ErrorBody),
    )
)]
async fn get_host(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<HostDetail>, ApiError> {
    Ok(Json(load_detail(&state, &id).await?))
}

#[derive(Deserialize, ToSchema)]
pub struct SetLabels {
    pub labels: Vec<Label>,
}

#[utoipa::path(
    put,
    tag = "Hosts",
    summary = "Replace host labels",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/hosts/{id}/labels",
    params(("id" = String, Path, description = "Host id")),
    request_body = SetLabels,
    responses(
        (status = 200, description = "Host with the new labels", body = HostDetail),
        (status = 404, description = "Unknown host", body = ErrorBody),
        (status = 409, description = "The host is revoked", body = ErrorBody),
        (status = 422, description = "Invalid labels", body = ErrorBody),
    )
)]
async fn set_labels(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<String>,
    Json(req): Json<SetLabels>,
) -> Result<Json<HostDetail>, ApiError> {
    labels::validate(&req.labels)?;
    let current = load_detail(&state, &id).await?;
    if current.host.status == HostStatus::Revoked {
        return Err(ApiError::Conflict("the host is revoked".into()));
    }
    sqlx::query("UPDATE agents SET labels_json = $1 WHERE id = $2 AND status <> 'revoked'")
        .bind(serde_json::to_string(&req.labels).map_err(|e| ApiError::Internal(e.into()))?)
        .bind(&id)
        .execute(&state.db.pool)
        .await?;
    Ok(Json(load_detail(&state, &id).await?))
}

#[utoipa::path(
    post,
    tag = "Hosts",
    summary = "Revoke a host",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/hosts/{id}/revoke",
    params(("id" = String, Path, description = "Host id")),
    responses(
        (status = 204, description = "The host is revoked and disconnected"),
        (status = 404, description = "Unknown host", body = ErrorBody),
    )
)]
async fn revoke_host(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    sqlx::query("UPDATE agents SET status = 'revoked', revoked_at = $1 WHERE id = $2 AND status <> 'revoked'")
        .bind(now_ms())
        .bind(&id)
        .execute(&state.db.pool)
        .await?;
    let known = sqlx::query("SELECT 1 FROM agents WHERE id = $1")
        .bind(&id)
        .fetch_optional(&state.db.pool)
        .await?
        .is_some();
    if !known {
        return Err(ApiError::NotFound("host not found".into()));
    }
    let active = sqlx::query(
        "SELECT id FROM jobs WHERE host_id = $1 AND state IN ('queued', 'assigned', 'running')",
    )
    .bind(&id)
    .fetch_all(&state.db.pool)
    .await?;
    sqlx::query(
        "UPDATE jobs SET state = CASE WHEN state = 'running' THEN 'interrupted' ELSE 'cancelled' END, \
         finished_at = $1, phase = NULL, error_code = 'host_revoked', \
         error_message = 'The host was revoked.' \
         WHERE host_id = $2 AND state IN ('queued', 'assigned', 'running')",
    )
    .bind(now_ms())
    .bind(&id)
    .execute(&state.db.pool)
    .await?;
    for row in &active {
        let job_id: String = row.try_get("id")?;
        state.registry.live.remove(&job_id);
        state.registry.clear_issued(&job_id);
    }
    state.registry.revoke(&id);
    Ok(StatusCode::NO_CONTENT)
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_join_token))
        .routes(routes!(list_hosts))
        .routes(routes!(get_host))
        .routes(routes!(set_labels))
        .routes(routes!(tokens::get_token))
        .routes(routes!(revoke_host))
}
