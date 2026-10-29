use std::collections::HashMap;

use anyhow::anyhow;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::model::{AlertFacts, AlertState, AlertType, CertKind, Severity};
use super::silences;
use crate::auth::tokens::{AlertsWrite, HostsRead, Scoped};
use crate::db::now_ms;
use crate::error::{ApiError, ErrorBody};
use crate::state::AppState;

const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 500;
const HOUR_MS: i64 = 3_600_000;

#[derive(Debug, Clone, Copy, Default, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AlertScope {
    #[default]
    Active,
    Resolved,
    All,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AlertQuery {
    /// `active` (default), `resolved` or `all`.
    #[param(inline)]
    pub state: Option<AlertScope>,
    /// Only alerts of this host.
    pub host_id: Option<String>,
    /// Page size, 1 to 500; 50 by default.
    pub limit: Option<u32>,
    /// The `id` of the last alert of the previous page; lists the alerts started before it.
    pub before: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct HostRef {
    pub id: String,
    pub hostname: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RepoRef {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AlertDto {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: AlertType,
    pub severity: Severity,
    pub state: AlertState,
    pub host: Option<HostRef>,
    pub repository: Option<RepoRef>,
    pub policy: Option<String>,
    pub check_level: Option<String>,
    pub cert: Option<CertKind>,
    pub title: String,
    pub detail: String,
    pub started_at: i64,
    pub resolved_at: Option<i64>,
    /// The host is silenced; only host alerts can be.
    pub silenced: bool,
    pub silenced_until: Option<i64>,
    pub facts: AlertFacts,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AlertList {
    pub alerts: Vec<AlertDto>,
    /// All alerts matching `state` and `host_id`, not only this page.
    pub total: u64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AlertCount {
    pub active: u64,
    /// Active alerts of hosts that are not silenced, and alerts without a host.
    pub unsilenced: u64,
}

#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SilenceDuration {
    Hour,
    Day,
    Week,
    Forever,
}

impl SilenceDuration {
    fn until(self, now_ms: i64) -> Option<i64> {
        let hours = match self {
            SilenceDuration::Hour => 1,
            SilenceDuration::Day => 24,
            SilenceDuration::Week => 7 * 24,
            SilenceDuration::Forever => return None,
        };
        Some(now_ms + hours * HOUR_MS)
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SilenceInput {
    pub duration: SilenceDuration,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SilenceDto {
    pub host_id: String,
    /// `null` until the silence is lifted.
    pub until: Option<i64>,
    pub created_at: i64,
    pub created_by: String,
}

fn alert_from_row(
    row: &sqlx::any::AnyRow,
    silenced: &HashMap<String, Option<i64>>,
) -> anyhow::Result<AlertDto> {
    let text = |name: &str| row.try_get::<String, _>(name);
    let opt = |name: &str| row.try_get::<Option<String>, _>(name);
    let kind = text("type")?;
    let severity = text("severity")?;
    let state = text("state")?;
    let host_id = opt("host_id")?;
    let repository_id = opt("repository_id")?;
    let silence = host_id.as_ref().and_then(|id| silenced.get(id));
    Ok(AlertDto {
        id: text("id")?,
        kind: AlertType::parse(&kind).ok_or_else(|| anyhow!("unknown alert type {kind}"))?,
        severity: Severity::parse(&severity)
            .ok_or_else(|| anyhow!("unknown alert severity {severity}"))?,
        state: AlertState::parse(&state).ok_or_else(|| anyhow!("unknown alert state {state}"))?,
        host: match (&host_id, opt("hostname")?) {
            (Some(id), Some(hostname)) => Some(HostRef {
                id: id.clone(),
                hostname,
            }),
            _ => None,
        },
        repository: match (repository_id, opt("repository_name")?) {
            (Some(id), Some(name)) => Some(RepoRef { id, name }),
            _ => None,
        },
        policy: opt("policy_name")?,
        check_level: opt("check_level")?,
        cert: opt("cert")?.as_deref().and_then(CertKind::parse),
        title: text("title")?,
        detail: text("detail")?,
        started_at: row.try_get("started_at")?,
        resolved_at: row.try_get("resolved_at")?,
        silenced: silence.is_some(),
        silenced_until: silence.copied().flatten(),
        facts: serde_json::from_str(&text("facts_json")?).unwrap_or_default(),
    })
}

#[utoipa::path(
    get,
    tag = "Alerts",
    operation_id = "list_alerts",
    summary = "List alerts",
    description = "Active alerts by default, or the history. Newest first. Alerts of silenced \
        hosts stay in the list and are marked.",
    security(("session" = []), ("bearer" = ["hosts:read"])),
    path = "/api/v1/alerts",
    params(AlertQuery),
    responses((status = 200, description = "A page of alerts", body = AlertList))
)]
async fn list_alerts(
    State(state): State<AppState>,
    _auth: Scoped<HostsRead>,
    Query(query): Query<AlertQuery>,
) -> Result<Json<AlertList>, ApiError> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let mut clauses = Vec::new();
    match query.state.unwrap_or_default() {
        AlertScope::Active => clauses.push("al.state = 'firing'".to_string()),
        AlertScope::Resolved => clauses.push("al.state = 'resolved'".to_string()),
        AlertScope::All => {}
    }
    if query.host_id.is_some() {
        clauses.push("al.host_id = $1".to_string());
    }
    let filter = |clauses: &[String]| {
        if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        }
    };

    let count_sql = format!("SELECT COUNT(*) AS n FROM alerts al{}", filter(&clauses));
    let mut count = sqlx::query(sqlx::AssertSqlSafe(count_sql));
    if let Some(host_id) = &query.host_id {
        count = count.bind(host_id);
    }
    let total: i64 = count.fetch_one(&state.db.pool).await?.try_get("n")?;

    if query.before.is_some() {
        let n = 1 + usize::from(query.host_id.is_some());
        clauses.push(format!(
            "(al.started_at < (SELECT started_at FROM alerts WHERE id = ${n}) OR \
             (al.started_at = (SELECT started_at FROM alerts WHERE id = ${n}) AND al.id < ${n}))"
        ));
    }
    let sql = format!(
        "SELECT al.id, al.type, al.severity, al.state, al.host_id, a.hostname, al.repository_id, \
         r.name AS repository_name, al.policy_name, al.check_level, al.cert, al.title, al.detail, \
         al.facts_json, al.started_at, al.resolved_at FROM alerts al \
         LEFT JOIN agents a ON a.id = al.host_id \
         LEFT JOIN repositories r ON r.id = al.repository_id{} \
         ORDER BY al.started_at DESC, al.id DESC LIMIT {limit}",
        filter(&clauses)
    );
    let mut page = sqlx::query(sqlx::AssertSqlSafe(sql));
    if let Some(host_id) = &query.host_id {
        page = page.bind(host_id);
    }
    if let Some(before) = &query.before {
        page = page.bind(before);
    }
    let rows = page.fetch_all(&state.db.pool).await?;
    let silenced = silences(&state, now_ms()).await?;
    let alerts = rows
        .iter()
        .map(|row| alert_from_row(row, &silenced))
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(Json(AlertList {
        alerts,
        total: u64::try_from(total).unwrap_or(0),
    }))
}

#[utoipa::path(
    get,
    tag = "Alerts",
    operation_id = "count_alerts",
    summary = "Count active alerts",
    security(("session" = []), ("bearer" = ["hosts:read"])),
    path = "/api/v1/alerts/count",
    responses((status = 200, description = "Active alerts, and those of hosts that are not silenced", body = AlertCount))
)]
async fn count_alerts(
    State(state): State<AppState>,
    _auth: Scoped<HostsRead>,
) -> Result<Json<AlertCount>, ApiError> {
    let rows = sqlx::query("SELECT host_id FROM alerts WHERE state = 'firing'")
        .fetch_all(&state.db.pool)
        .await?;
    let silenced = silences(&state, now_ms()).await?;
    let mut count = AlertCount {
        active: rows.len() as u64,
        unsilenced: 0,
    };
    for row in &rows {
        let host_id: Option<String> = row.try_get("host_id")?;
        if host_id.is_none_or(|id| !silenced.contains_key(&id)) {
            count.unsilenced += 1;
        }
    }
    Ok(Json(count))
}

async fn require_host(state: &AppState, id: &str) -> Result<(), ApiError> {
    let row = sqlx::query("SELECT status FROM agents WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db.pool)
        .await?
        .ok_or_else(|| ApiError::NotFound("host not found".into()))?;
    if row.try_get::<String, _>("status")? == "revoked" {
        return Err(ApiError::Conflict("The host is revoked.".into()));
    }
    Ok(())
}

#[utoipa::path(
    put,
    tag = "Alerts",
    operation_id = "silence_host",
    summary = "Silence a host",
    description = "A silenced host sends no notifications. Its alerts stay visible and the host \
        keeps its real state. A new silence replaces the previous one.",
    security(("session" = [], "csrf" = []), ("bearer" = ["alerts:write"])),
    path = "/api/v1/hosts/{id}/silence",
    params(("id" = String, Path, description = "Host id")),
    request_body = SilenceInput,
    responses(
        (status = 200, description = "The silence", body = SilenceDto),
        (status = 404, description = "Unknown host", body = ErrorBody),
        (status = 409, description = "The host is revoked", body = ErrorBody),
    )
)]
async fn silence_host(
    State(state): State<AppState>,
    auth: Scoped<AlertsWrite>,
    Path(id): Path<String>,
    Json(input): Json<SilenceInput>,
) -> Result<Json<SilenceDto>, ApiError> {
    require_host(&state, &id).await?;
    let now = now_ms();
    let silence = SilenceDto {
        host_id: id,
        until: input.duration.until(now),
        created_at: now,
        created_by: auth.actor(),
    };
    sqlx::query(
        "INSERT INTO alert_silences (host_id, until_ms, created_by, created_at) \
         VALUES ($1, $2, $3, $4) ON CONFLICT (host_id) DO UPDATE SET \
         until_ms = excluded.until_ms, created_by = excluded.created_by, \
         created_at = excluded.created_at",
    )
    .bind(&silence.host_id)
    .bind(silence.until)
    .bind(&silence.created_by)
    .bind(silence.created_at)
    .execute(&state.db.pool)
    .await?;
    Ok(Json(silence))
}

#[utoipa::path(
    delete,
    tag = "Alerts",
    operation_id = "resume_host_alerts",
    summary = "Lift the silence of a host",
    security(("session" = [], "csrf" = []), ("bearer" = ["alerts:write"])),
    path = "/api/v1/hosts/{id}/silence",
    params(("id" = String, Path, description = "Host id")),
    responses(
        (status = 204, description = "The host is no longer silenced"),
        (status = 404, description = "Unknown host", body = ErrorBody),
    )
)]
async fn resume_host_alerts(
    State(state): State<AppState>,
    _auth: Scoped<AlertsWrite>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    sqlx::query("SELECT id FROM agents WHERE id = $1")
        .bind(&id)
        .fetch_optional(&state.db.pool)
        .await?
        .ok_or_else(|| ApiError::NotFound("host not found".into()))?;
    sqlx::query("DELETE FROM alert_silences WHERE host_id = $1")
        .bind(&id)
        .execute(&state.db.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_alerts))
        .routes(routes!(count_alerts))
        .routes(routes!(silence_host))
        .routes(routes!(resume_host_alerts))
}
