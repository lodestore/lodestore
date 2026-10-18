use axum::Json;
use axum::extract::{Path, State};
use lodestore_format::index::{hex32, parse_hex32};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use lodestore_policy::retention::Rule;
use lodestore_policy::{BackupPolicy, Retention, Schedule, validate};

use super::retention::plan_policy;
use super::{SNAPSHOT_COLUMNS, SnapshotItem, item_from_row};
use crate::auth::tokens::{PoliciesRead, Scoped, SnapshotsDelete};
use crate::db::now_ms;
use crate::error::{ApiError, ErrorBody};
use crate::state::AppState;

const AUTOMATIC_MARK: &str = "anomaly";
const REASON_MAX_CHARS: usize = 200;
const PREVIEW_LIMIT: usize = 200;
const ZONE_ONLY_CRON: &str = "0 0 * * *";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AnomalyKind {
    FilesDrop,
    BytesDrop,
    ChangedSpike,
    AddedSpike,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AnomalyReasonDto {
    pub kind: AnomalyKind,
    pub pct: u32,
    pub from: u64,
    pub to: u64,
    pub factor: f64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SuspicionDto {
    pub automatic: bool,
    pub reasons: Vec<AnomalyReasonDto>,
    pub note: Option<String>,
    pub marked_by: String,
    pub marked_at: i64,
}

pub(crate) fn suspicion_from_row(
    row: &sqlx::any::AnyRow,
) -> Result<Option<SuspicionDto>, ApiError> {
    if row.try_get::<i64, _>("suspicious")? == 0 {
        return Ok(None);
    }
    let marked_by = row
        .try_get::<Option<String>, _>("suspicious_by")?
        .unwrap_or_default();
    let reasons = row
        .try_get::<Option<String>, _>("anomaly_json")?
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();
    Ok(Some(SuspicionDto {
        automatic: marked_by == AUTOMATIC_MARK,
        reasons,
        note: row.try_get("suspicious_reason")?,
        marked_by,
        marked_at: row.try_get::<Option<i64>, _>("suspicious_at")?.unwrap_or(0),
    }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SetSuspiciousRequest {
    pub suspicious: bool,
    pub reason: String,
}

#[utoipa::path(
    put,
    tag = "Snapshots",
    summary = "Mark a snapshot as suspicious or clear the mark",
    security(("session" = [], "csrf" = []), ("bearer" = ["snapshots:delete"])),
    path = "/api/v1/snapshots/{id}/suspicious",
    params(("id" = String, Path, description = "Snapshot id")),
    request_body = SetSuspiciousRequest,
    responses(
        (status = 200, description = "The snapshot with its new mark", body = SnapshotItem),
        (status = 403, description = "The token needs the snapshots:delete scope", body = ErrorBody),
        (status = 404, description = "Unknown snapshot", body = ErrorBody),
        (status = 422, description = "The reason is empty or too long", body = ErrorBody),
    )
)]
pub async fn set_suspicious(
    State(state): State<AppState>,
    auth: Scoped<SnapshotsDelete>,
    Path(id): Path<String>,
    Json(req): Json<SetSuspiciousRequest>,
) -> Result<Json<SnapshotItem>, ApiError> {
    let unknown = || ApiError::NotFound("snapshot not found".into());
    let reason = req.reason.trim();
    if reason.is_empty() || reason.chars().count() > REASON_MAX_CHARS {
        return Err(ApiError::Validation("Enter a reason.".into()));
    }
    let id = hex32(&parse_hex32(&id).ok_or_else(unknown)?);
    let sql = if req.suspicious {
        "UPDATE snapshots SET suspicious = 1, suspicious_by = $1, suspicious_reason = $2, \
         suspicious_at = $3 WHERE id = $4"
    } else {
        "UPDATE snapshots SET suspicious = 0, suspicious_cleared_by = $1, \
         suspicious_cleared_reason = $2, suspicious_cleared_at = $3 \
         WHERE id = $4 AND suspicious = 1"
    };
    sqlx::query(sql)
        .bind(auth.actor())
        .bind(reason)
        .bind(now_ms())
        .bind(&id)
        .execute(&state.db.pool)
        .await?;
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {SNAPSHOT_COLUMNS} FROM snapshots WHERE id = $1"
    )))
    .bind(&id)
    .fetch_optional(&state.db.pool)
    .await?
    .ok_or_else(unknown)?;
    Ok(Json(item_from_row(&row)?))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct RetentionPreviewRequest {
    pub retention: Retention,
    pub timezone: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PreviewSnapshot {
    pub id: String,
    pub time: i64,
    pub received_at: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct HostRetentionPreview {
    pub host_id: String,
    pub hostname: String,
    pub total: u64,
    pub forget: u64,
    pub snapshots: Vec<PreviewSnapshot>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RetentionPreview {
    pub total: u64,
    pub forget: u64,
    /// Snapshots no rule keeps that stay only because the two newest complete
    /// snapshots of each host are never forgotten.
    pub floor: u64,
    pub suspicious: u64,
    pub truncated: u64,
    pub hosts: Vec<HostRetentionPreview>,
}

#[utoipa::path(
    post,
    tag = "Policies",
    summary = "Preview what retention rules would forget",
    security(("session" = [], "csrf" = []), ("bearer" = ["policies:read"])),
    path = "/api/v1/policies/{name}/retention-preview",
    params(("name" = String, Path, description = "Policy name")),
    request_body = RetentionPreviewRequest,
    responses(
        (status = 200, description = "Snapshots the rules would forget, per host", body = RetentionPreview),
        (status = 404, description = "Unknown policy", body = ErrorBody),
        (status = 422, description = "The rules are not valid", body = ErrorBody),
    )
)]
pub async fn retention_preview(
    State(state): State<AppState>,
    _auth: Scoped<PoliciesRead>,
    Path(name): Path<String>,
    Json(req): Json<RetentionPreviewRequest>,
) -> Result<Json<RetentionPreview>, ApiError> {
    let row = sqlx::query("SELECT spec_json FROM policies WHERE name = $1")
        .bind(&name)
        .fetch_optional(&state.db.pool)
        .await?
        .ok_or_else(|| ApiError::NotFound("policy not found".into()))?;
    let mut policy: BackupPolicy = serde_json::from_str(&row.try_get::<String, _>("spec_json")?)
        .map_err(|e| ApiError::Internal(e.into()))?;
    if let Some(timezone) = req.timezone {
        policy
            .spec
            .schedule
            .get_or_insert_with(|| Schedule {
                cron: ZONE_ONLY_CRON.into(),
                timezone: None,
                jitter: None,
                catch_up_window: None,
            })
            .timezone = Some(timezone);
    }
    policy.spec.retention = Some(req.retention);
    if let Some(problem) = validate(&policy).into_iter().next() {
        return Err(ApiError::Validation(problem.message));
    }
    let rules = policy.spec.retention.clone().unwrap_or_default();
    let plan = plan_policy(&state, &policy, &rules).await?;

    let mut preview = RetentionPreview {
        total: 0,
        forget: 0,
        floor: 0,
        suspicious: 0,
        truncated: 0,
        hosts: Vec::new(),
    };
    let mut room = PREVIEW_LIMIT;
    for host in plan.hosts {
        let mut doomed: Vec<_> = host.items.iter().filter(|i| !i.keep).collect();
        doomed.sort_by(|a, b| {
            b.received_at_ms
                .cmp(&a.received_at_ms)
                .then_with(|| a.id.cmp(&b.id))
        });
        let shown = doomed.len().min(room);
        room -= shown;
        preview.total += host.items.len() as u64;
        preview.forget += doomed.len() as u64;
        preview.truncated += (doomed.len() - shown) as u64;
        preview.floor += host
            .items
            .iter()
            .filter(|i| i.reasons == [Rule::Floor])
            .count() as u64;
        preview.suspicious += host.items.iter().filter(|i| i.suspicious).count() as u64;
        preview.hosts.push(HostRetentionPreview {
            host_id: host.host_id,
            hostname: host.hostname,
            total: host.items.len() as u64,
            forget: doomed.len() as u64,
            snapshots: doomed
                .into_iter()
                .take(shown)
                .map(|i| PreviewSnapshot {
                    id: i.id.clone(),
                    time: i.time_ms,
                    received_at: i.received_at_ms,
                })
                .collect(),
        });
    }
    Ok(Json(preview))
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(set_suspicious))
        .routes(routes!(retention_preview))
}
