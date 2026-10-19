use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::any::AnyRow;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::restore::{RestoreRequest, enqueue_restore};
use super::{
    ACTIVE_STATES, ConflictPolicyDto, DOWNLOAD_COLUMNS, JOB_COLUMNS, JOB_FROM, JobDetail, JobKind,
    JobProgressDto, JobSource, JobState, JobSummary, RunJob, busy_hosts, download_summary, enqueue,
    gc_select, gc_summary, job_spec, load_download, load_job, summary_from_row,
};
use crate::auth::tokens::{JobsRead, JobsRun, RestoreScope, Scoped};
use crate::browse::AGENTLESS;
use crate::browse::tree::decode_b64;
use crate::db::now_ms;
use crate::download::api::TICKET_TTL_MS;
use crate::error::{ApiError, ErrorBody};
use crate::policies::policy_for_rerun;
use crate::state::AppState;

const PAGE_SIZE: i64 = 50;
const EVENTS_LIMIT: i64 = 1000;

fn overlay_live(state: &AppState, job: &mut JobSummary) {
    if let Some(live) = state.registry.live.get(&job.id) {
        job.progress = Some(JobProgressDto {
            files_done: live.files_done,
            files_total: live.files_total,
            bytes_done: live.bytes_done,
            bytes_total: live.bytes_total,
            bytes_uploaded: live.bytes_uploaded,
            bytes_downloaded: live.bytes_downloaded,
            scan_done: live.scan_done,
            speed_bps: live.speed_bps,
            upload_speed_bps: live.upload_speed_bps,
            eta_s: live.eta_s.map(|s| s as i64),
            updated_at: live.updated_at,
        });
    }
}

#[derive(Serialize, ToSchema)]
pub struct JobPage {
    pub items: Vec<JobSummary>,
    pub total: u64,
    pub page: u64,
    pub page_size: u64,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct JobsQuery {
    /// One of queued, running, waiting, succeeded, warnings, failed, cancelled, interrupted,
    /// not_started. `running` also lists cleanup cycles that are waiting.
    pub status: Option<String>,
    /// Exact policy name.
    pub policy: Option<String>,
    /// `manual` or `cli`.
    pub source: Option<String>,
    /// Case-insensitive part of the hostname.
    pub host: Option<String>,
    /// Exact host id.
    pub host_id: Option<String>,
    /// `backup`, `restore`, `download` or `gc`.
    pub kind: Option<String>,
    /// One-based page number.
    pub page: Option<u64>,
}

fn like_pattern(part: &str) -> String {
    let mut out = String::from("%");
    for c in part.to_lowercase().chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

#[utoipa::path(
    get,
    tag = "Jobs",
    summary = "List jobs",
    security(("session" = []), ("bearer" = ["jobs:read"])),
    path = "/api/v1/jobs",
    params(JobsQuery),
    responses(
        (status = 200, description = "Job history, active jobs first", body = JobPage),
        (status = 422, description = "Unknown filter value", body = ErrorBody),
    )
)]
pub async fn list_jobs(
    State(state): State<AppState>,
    _auth: Scoped<JobsRead>,
    Query(query): Query<JobsQuery>,
) -> Result<Json<JobPage>, ApiError> {
    let kind = match &query.kind {
        Some(kind) => Some(
            JobKind::parse(kind)
                .ok_or_else(|| ApiError::Validation(format!("Unknown kind \"{kind}\".")))?,
        ),
        None => None,
    };
    let status = match &query.status {
        Some(status) => Some(
            JobState::parse(status)
                .ok_or_else(|| ApiError::Validation(format!("Unknown status \"{status}\".")))?,
        ),
        None => None,
    };
    if let Some(source) = &query.source
        && !matches!(source.as_str(), "manual" | "cli")
    {
        return Err(ApiError::Validation(format!(
            "Unknown source \"{source}\"."
        )));
    }
    let want_jobs = !matches!(kind, Some(JobKind::Download | JobKind::Gc));
    let want_downloads = kind.is_none() || kind == Some(JobKind::Download);
    let want_gc = kind.is_none() || kind == Some(JobKind::Gc);

    let page = query.page.unwrap_or(1).max(1);
    let offset = i64::try_from(page - 1)
        .unwrap_or(i64::MAX)
        .saturating_mul(PAGE_SIZE);
    let merged = [want_jobs, want_downloads, want_gc]
        .iter()
        .filter(|wanted| **wanted)
        .count()
        > 1;
    let (sql_offset, sql_limit) = if merged {
        (0, offset.saturating_add(PAGE_SIZE))
    } else {
        (offset, PAGE_SIZE)
    };

    let mut total = 0i64;
    let mut entries: Vec<(SortKey, Entry)> = Vec::new();
    if want_jobs {
        let (count, rows) = fetch_jobs(&state, &query, kind, status, sql_limit, sql_offset).await?;
        total += count;
        for row in rows {
            let key = SortKey::new(
                matches!(
                    row.try_get::<String, _>("state")?.as_str(),
                    "queued" | "assigned" | "running"
                ),
                row.try_get("queued_at")?,
                row.try_get("id")?,
            );
            entries.push((key, Entry::Job(row)));
        }
    }
    if want_downloads {
        let (count, rows) = fetch_downloads(&state, &query, status, sql_limit, sql_offset).await?;
        total += count;
        for row in rows {
            let state_name: String = row.try_get("state")?;
            let created_at: i64 = row.try_get("created_at")?;
            let fresh = created_at > now_ms() - TICKET_TTL_MS;
            let key = SortKey::new(
                state_name == "running" || (state_name == "pending" && fresh),
                created_at,
                row.try_get("id")?,
            );
            entries.push((key, Entry::Download(row)));
        }
    }
    if want_gc {
        let (count, rows) = fetch_gc(&state, &query, status, sql_limit, sql_offset).await?;
        total += count;
        for row in rows {
            let key = SortKey::new(
                matches!(
                    row.try_get::<String, _>("state")?.as_str(),
                    "marking" | "repacking" | "waiting" | "verifying" | "sweeping"
                ),
                row.try_get("created_at")?,
                row.try_get("id")?,
            );
            entries.push((key, Entry::Gc(row)));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let skip = if merged { offset as usize } else { 0 };

    let busy = busy_hosts(&state).await?;
    let mut items = Vec::new();
    for (_, entry) in entries.iter().skip(skip).take(PAGE_SIZE as usize) {
        items.push(match entry {
            Entry::Job(row) => summary_from_row(&state, row, &busy)?,
            Entry::Download(row) => download_summary(&state, row)?.0,
            Entry::Gc(row) => gc_summary(row)?,
        });
    }
    for item in &mut items {
        overlay_live(&state, item);
    }
    Ok(Json(JobPage {
        items,
        total: total.max(0) as u64,
        page,
        page_size: PAGE_SIZE as u64,
    }))
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct SortKey(u8, std::cmp::Reverse<i64>, std::cmp::Reverse<String>);

impl SortKey {
    fn new(active: bool, queued_at: i64, id: String) -> SortKey {
        SortKey(
            u8::from(!active),
            std::cmp::Reverse(queued_at),
            std::cmp::Reverse(id),
        )
    }
}

enum Entry {
    Job(AnyRow),
    Download(AnyRow),
    Gc(AnyRow),
}

fn place(binds: &mut Vec<String>, value: String) -> String {
    binds.push(value);
    format!("${}", binds.len())
}

async fn fetch_jobs(
    state: &AppState,
    query: &JobsQuery,
    kind: Option<JobKind>,
    status: Option<JobState>,
    limit: i64,
    offset: i64,
) -> Result<(i64, Vec<AnyRow>), ApiError> {
    let mut clauses: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();
    if let Some(status) = status {
        let marks: Vec<String> = status
            .db_names()
            .iter()
            .map(|name| place(&mut binds, (*name).to_string()))
            .collect();
        clauses.push(format!("j.state IN ({})", marks.join(", ")));
    }
    if let Some(kind) = kind {
        let name = if kind == JobKind::Restore {
            "restore"
        } else {
            "backup"
        };
        clauses.push(format!("j.kind = {}", place(&mut binds, name.to_string())));
    }
    if let Some(policy) = &query.policy {
        clauses.push(format!(
            "j.policy_name = {}",
            place(&mut binds, policy.clone())
        ));
    }
    if let Some(source) = &query.source {
        clauses.push(format!("j.source = {}", place(&mut binds, source.clone())));
    }
    if let Some(host) = query.host.as_deref().filter(|h| !h.is_empty()) {
        clauses.push(format!(
            "LOWER(a.hostname) LIKE {} ESCAPE '\\'",
            place(&mut binds, like_pattern(host))
        ));
    }
    if let Some(host_id) = query.host_id.as_deref().filter(|h| !h.is_empty()) {
        clauses.push(format!(
            "j.host_id = {}",
            place(&mut binds, host_id.to_string())
        ));
    }
    let filter = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };

    let mut count = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) AS n {JOB_FROM}{filter}"
    )));
    for value in &binds {
        count = count.bind(value.as_str());
    }
    let total: i64 = count.fetch_one(&state.db.pool).await?.try_get("n")?;

    let mut select = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {JOB_COLUMNS} {JOB_FROM}{filter} \
         ORDER BY CASE WHEN j.state IN ({ACTIVE_STATES}) THEN 0 ELSE 1 END, \
         j.queued_at DESC, j.id DESC LIMIT ${} OFFSET ${}",
        binds.len() + 1,
        binds.len() + 2
    )));
    for value in &binds {
        select = select.bind(value.as_str());
    }
    let rows = select
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db.pool)
        .await?;
    Ok((total, rows))
}

async fn fetch_downloads(
    state: &AppState,
    query: &JobsQuery,
    status: Option<JobState>,
    limit: i64,
    offset: i64,
) -> Result<(i64, Vec<AnyRow>), ApiError> {
    let cutoff = now_ms() - TICKET_TTL_MS;
    let mut clauses: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();
    if let Some(status) = status {
        clauses.push(match status {
            JobState::Queued => format!("(state = 'pending' AND created_at > {cutoff})"),
            JobState::NotStarted => format!("(state = 'pending' AND created_at <= {cutoff})"),
            JobState::Running => "state = 'running'".to_string(),
            JobState::Succeeded => "state = 'succeeded'".to_string(),
            JobState::Interrupted => "state = 'interrupted'".to_string(),
            JobState::Waiting | JobState::Warnings | JobState::Failed | JobState::Cancelled => {
                "1 = 0".to_string()
            }
        });
    }
    if query.policy.is_some() || query.source.as_deref() == Some("cli") {
        clauses.push("1 = 0".to_string());
    }
    if let Some(host) = query.host.as_deref().filter(|h| !h.is_empty()) {
        clauses.push(format!(
            "LOWER(hostname) LIKE {} ESCAPE '\\'",
            place(&mut binds, like_pattern(host))
        ));
    }
    if let Some(host_id) = query.host_id.as_deref().filter(|h| !h.is_empty()) {
        clauses.push(match host_id.strip_prefix(AGENTLESS) {
            Some(hostname) => format!(
                "(host_id IS NULL AND hostname = {})",
                place(&mut binds, hostname.to_string())
            ),
            None => format!("host_id = {}", place(&mut binds, host_id.to_string())),
        });
    }
    let filter = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };

    let mut count = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) AS n FROM downloads{filter}"
    )));
    for value in &binds {
        count = count.bind(value.as_str());
    }
    let total: i64 = count.fetch_one(&state.db.pool).await?.try_get("n")?;

    let mut select = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {DOWNLOAD_COLUMNS} FROM downloads{filter} \
         ORDER BY CASE WHEN state = 'running' OR (state = 'pending' AND created_at > {cutoff}) \
         THEN 0 ELSE 1 END, created_at DESC, id DESC LIMIT ${} OFFSET ${}",
        binds.len() + 1,
        binds.len() + 2
    )));
    for value in &binds {
        select = select.bind(value.as_str());
    }
    let rows = select
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db.pool)
        .await?;
    Ok((total, rows))
}

async fn fetch_gc(
    state: &AppState,
    query: &JobsQuery,
    status: Option<JobState>,
    limit: i64,
    offset: i64,
) -> Result<(i64, Vec<AnyRow>), ApiError> {
    let mut clauses: Vec<&str> = Vec::new();
    if let Some(status) = status {
        clauses.push(match status {
            JobState::Running => {
                "state IN ('marking', 'repacking', 'waiting', 'verifying', 'sweeping')"
            }
            JobState::Waiting => "state = 'waiting'",
            JobState::Succeeded => "state = 'done'",
            JobState::Failed => "state = 'failed'",
            JobState::Cancelled => "state = 'aborted'",
            JobState::Queued
            | JobState::Warnings
            | JobState::Interrupted
            | JobState::NotStarted => "1 = 0",
        });
    }
    let hostless = query.policy.is_some()
        || query.source.as_deref() == Some("cli")
        || query.host.as_deref().is_some_and(|h| !h.is_empty())
        || query.host_id.as_deref().is_some_and(|h| !h.is_empty());
    if hostless {
        clauses.push("1 = 0");
    }
    let filter = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };

    let total: i64 = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) AS n FROM gc_cycles{filter}"
    )))
    .fetch_one(&state.db.pool)
    .await?
    .try_get("n")?;
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "{}{filter} ORDER BY CASE WHEN state IN ('done', 'failed', 'aborted') THEN 1 ELSE 0 END, \
         created_at DESC, id DESC LIMIT $1 OFFSET $2",
        gc_select()
    )))
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db.pool)
    .await?;
    Ok((total, rows))
}

#[utoipa::path(
    get,
    tag = "Jobs",
    summary = "Get a job",
    security(("session" = []), ("bearer" = ["jobs:read"])),
    path = "/api/v1/jobs/{id}",
    params(("id" = String, Path, description = "Job id")),
    responses(
        (status = 200, description = "The job", body = JobDetail),
        (status = 404, description = "Unknown job", body = ErrorBody),
    )
)]
pub async fn get_job(
    State(state): State<AppState>,
    _auth: Scoped<JobsRead>,
    Path(id): Path<String>,
) -> Result<Json<JobDetail>, ApiError> {
    job_detail(&state, &id).await
}

async fn job_detail(state: &AppState, id: &str) -> Result<Json<JobDetail>, ApiError> {
    let mut job = match load_job(state, id).await? {
        Some(job) => job,
        None => load_download(state, id)
            .await?
            .ok_or_else(|| ApiError::NotFound("job not found".into()))?,
    };
    overlay_live(state, &mut job.summary);
    Ok(Json(job))
}

#[derive(Serialize, ToSchema)]
pub struct JobEventDto {
    pub seq: i64,
    pub at: i64,
    pub level: String,
    pub kind: String,
    pub path: Option<String>,
    pub script: Option<String>,
    pub message: String,
}

#[derive(Serialize, ToSchema)]
pub struct JobEventPage {
    pub events: Vec<JobEventDto>,
    pub last_seq: i64,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct EventsQuery {
    /// Return events with a greater sequence number only.
    pub after: Option<i64>,
    /// `warning` or `error`: return events of this level only.
    pub level: Option<String>,
}

#[utoipa::path(
    get,
    tag = "Jobs",
    summary = "List job log events",
    security(("session" = []), ("bearer" = ["jobs:read"])),
    path = "/api/v1/jobs/{id}/events",
    params(("id" = String, Path, description = "Job id"), EventsQuery),
    responses(
        (status = 200, description = "Up to 1000 log events in ascending order", body = JobEventPage),
        (status = 404, description = "Unknown job", body = ErrorBody),
        (status = 422, description = "Unknown level", body = ErrorBody),
    )
)]
pub async fn list_job_events(
    State(state): State<AppState>,
    _auth: Scoped<JobsRead>,
    Path(id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> Result<Json<JobEventPage>, ApiError> {
    if let Some(level) = &query.level
        && !matches!(level.as_str(), "warning" | "error")
    {
        return Err(ApiError::Validation(format!("Unknown level \"{level}\".")));
    }
    sqlx::query("SELECT 1 FROM jobs WHERE id = $1")
        .bind(&id)
        .fetch_optional(&state.db.pool)
        .await?
        .ok_or_else(|| ApiError::NotFound("job not found".into()))?;
    let after = query.after.unwrap_or(i64::MIN);
    let level_clause = if query.level.is_some() {
        " AND level = $3"
    } else {
        ""
    };
    let select = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT seq, at, level, kind, path, script, message FROM job_events \
         WHERE job_id = $1 AND seq > $2{level_clause} ORDER BY seq LIMIT {EVENTS_LIMIT}"
    )))
    .bind(&id)
    .bind(after);
    let select = match &query.level {
        Some(level) => select.bind(level.as_str()),
        None => select,
    };
    let events = select
        .fetch_all(&state.db.pool)
        .await?
        .iter()
        .map(|row| {
            Ok(JobEventDto {
                seq: row.try_get("seq")?,
                at: row.try_get("at")?,
                level: row.try_get("level")?,
                kind: row.try_get("kind")?,
                path: row.try_get("path")?,
                script: row.try_get("script")?,
                message: row.try_get("message")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
    Ok(Json(JobEventPage {
        last_seq: events.last().map_or(query.after.unwrap_or(0), |e| e.seq),
        events,
    }))
}

#[utoipa::path(
    post,
    tag = "Jobs",
    summary = "Cancel a job",
    security(("session" = [], "csrf" = []), ("bearer" = ["jobs:run"])),
    path = "/api/v1/jobs/{id}/cancel",
    params(("id" = String, Path, description = "Job id")),
    responses(
        (status = 200, description = "A queued job is cancelled at once; a running job gets a cancel request", body = JobDetail),
        (status = 404, description = "Unknown job", body = ErrorBody),
        (status = 409, description = "The job has finished", body = ErrorBody),
    )
)]
pub async fn cancel_job(
    State(state): State<AppState>,
    _auth: Scoped<JobsRun>,
    Path(id): Path<String>,
) -> Result<Json<JobDetail>, ApiError> {
    let cancelled = sqlx::query(
        "UPDATE jobs SET state = 'cancelled', finished_at = $1 WHERE id = $2 AND state = 'queued'",
    )
    .bind(now_ms())
    .bind(&id)
    .execute(&state.db.pool)
    .await?
    .rows_affected()
        == 1;
    if !cancelled {
        let requested = sqlx::query(
            "UPDATE jobs SET cancel_requested = 1 \
             WHERE id = $1 AND state IN ('assigned', 'running')",
        )
        .bind(&id)
        .execute(&state.db.pool)
        .await?
        .rows_affected()
            == 1;
        if !requested {
            return Err(match load_job(&state, &id).await? {
                Some(_) => ApiError::Conflict("The job has already finished.".into()),
                None if load_download(&state, &id).await?.is_some() => downloads_refused(),
                None => ApiError::NotFound("job not found".into()),
            });
        }
    }
    job_detail(&state, &id).await
}

#[utoipa::path(
    post,
    tag = "Jobs",
    summary = "Run a job again",
    security(("session" = [], "csrf" = []), ("bearer" = ["jobs:run"])),
    path = "/api/v1/jobs/{id}/rerun",
    params(("id" = String, Path, description = "Job id")),
    responses(
        (status = 200, description = "The queued job of the same host and policy", body = RunJob),
        (status = 404, description = "Unknown job", body = ErrorBody),
        (status = 409, description = "The job cannot be repeated", body = ErrorBody),
    )
)]
pub async fn rerun_job(
    State(state): State<AppState>,
    _auth: Scoped<JobsRun>,
    Path(id): Path<String>,
) -> Result<Json<RunJob>, ApiError> {
    let Some(job) = load_job(&state, &id).await? else {
        return Err(if load_download(&state, &id).await?.is_some() {
            downloads_refused()
        } else {
            ApiError::NotFound("job not found".into())
        });
    };
    if job.summary.source == JobSource::Cli {
        return Err(ApiError::Conflict(
            "Jobs started with the CLI cannot be repeated.".into(),
        ));
    }
    if job.summary.kind != JobKind::Backup {
        return Err(ApiError::Conflict(
            "Start a new restore from Restore.".into(),
        ));
    }
    let policy_id: Option<String> = sqlx::query("SELECT policy_id FROM jobs WHERE id = $1")
        .bind(&id)
        .fetch_one(&state.db.pool)
        .await?
        .try_get("policy_id")?;
    let policy = match &policy_id {
        Some(policy_id) => policy_for_rerun(&state, policy_id, &job.summary.host_id).await?,
        None => None,
    };
    let (Some(policy_id), Some(policy)) = (policy_id, policy) else {
        return Err(ApiError::Conflict(
            "The policy of this job no longer exists.".into(),
        ));
    };
    let host = [(job.summary.host_id.clone(), job.summary.hostname.clone())];
    let run = enqueue(
        &state,
        &policy_id,
        &policy.metadata.name,
        &host,
        &job_spec(&policy),
    )
    .await?
    .pop()
    .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("no job was queued")))?;
    Ok(Json(run))
}

fn downloads_refused() -> ApiError {
    ApiError::Conflict("Downloads cannot be cancelled or run again.".into())
}

#[derive(Deserialize, ToSchema)]
pub struct RestoreBody {
    pub snapshot_id: String,
    /// Selected paths as base64url bytes without padding.
    pub items: Vec<String>,
    pub target_host_id: String,
    /// Absolute path of the folder to restore into; `/` with `in_place`.
    pub destination: String,
    pub in_place: bool,
    pub conflict: ConflictPolicyDto,
    pub restore_owners: bool,
}

#[derive(Serialize, ToSchema)]
pub struct RestoreCreated {
    pub job_id: String,
}

#[utoipa::path(
    post,
    tag = "Restore",
    summary = "Restore selected paths of a snapshot to a host",
    security(("session" = [], "csrf" = []), ("bearer" = ["restore"])),
    path = "/api/v1/restores",
    request_body = RestoreBody,
    responses(
        (status = 201, description = "The restore job is queued", body = RestoreCreated),
        (status = 400, description = "`restore_destination_root`: a restore into a new folder names `/`; `restore_in_place_not_empty`: a restore in place with `stop_if_not_empty`", body = ErrorBody),
        (status = 404, description = "Unknown snapshot or host, or a selected path is missing", body = ErrorBody),
        (status = 422, description = "The selection, destination or host is not valid", body = ErrorBody),
        (status = 502, description = "The repository storage is unavailable", body = ErrorBody),
    )
)]
pub async fn create_restore(
    State(state): State<AppState>,
    auth: Scoped<RestoreScope>,
    Json(body): Json<RestoreBody>,
) -> Result<(StatusCode, Json<RestoreCreated>), ApiError> {
    let items = body
        .items
        .iter()
        .map(|item| decode_b64(item))
        .collect::<Result<Vec<_>, _>>()?;
    let job_id = enqueue_restore(
        &state,
        RestoreRequest {
            snapshot_id: body.snapshot_id,
            items,
            target_host_id: body.target_host_id,
            destination: body.destination,
            in_place: body.in_place,
            conflict: body.conflict.to_proto(),
            restore_owners: body.restore_owners,
        },
        &auth.actor(),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(RestoreCreated { job_id })))
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_restore))
        .routes(routes!(list_jobs))
        .routes(routes!(get_job))
        .routes(routes!(list_job_events))
        .routes(routes!(cancel_job))
        .routes(routes!(rerun_job))
}
