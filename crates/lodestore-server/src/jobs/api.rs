use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::{
    ACTIVE_STATES, JOB_COLUMNS, JOB_FROM, JobDetail, JobProgressDto, JobSource, JobState,
    JobSummary, RunJob, busy_hosts, enqueue, job_spec, load_job, summary_from_row,
};
use crate::auth::AuthUser;
use crate::db::now_ms;
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
            scan_done: live.scan_done,
            speed_bps: live.speed_bps,
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
    /// One of queued, running, succeeded, warnings, failed, cancelled, interrupted, not_started.
    pub status: Option<String>,
    /// Exact policy name.
    pub policy: Option<String>,
    /// `manual` or `cli`.
    pub source: Option<String>,
    /// Case-insensitive part of the hostname.
    pub host: Option<String>,
    /// Exact host id.
    pub host_id: Option<String>,
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
    path = "/api/v1/jobs",
    params(JobsQuery),
    responses(
        (status = 200, description = "Job history, active jobs first", body = JobPage),
        (status = 422, description = "Unknown filter value", body = ErrorBody),
    )
)]
pub async fn list_jobs(
    State(state): State<AppState>,
    _user: AuthUser,
    Query(query): Query<JobsQuery>,
) -> Result<Json<JobPage>, ApiError> {
    let mut clauses: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();
    fn place(binds: &mut Vec<String>, value: String) -> String {
        binds.push(value);
        format!("${}", binds.len())
    }
    if let Some(status) = &query.status {
        let status = JobState::parse(status)
            .ok_or_else(|| ApiError::Validation(format!("Unknown status \"{status}\".")))?;
        let marks: Vec<String> = status
            .db_names()
            .iter()
            .map(|name| place(&mut binds, (*name).to_string()))
            .collect();
        clauses.push(format!("j.state IN ({})", marks.join(", ")));
    }
    if let Some(policy) = &query.policy {
        clauses.push(format!(
            "j.policy_name = {}",
            place(&mut binds, policy.clone())
        ));
    }
    if let Some(source) = &query.source {
        if !matches!(source.as_str(), "manual" | "cli") {
            return Err(ApiError::Validation(format!(
                "Unknown source \"{source}\"."
            )));
        }
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

    let page = query.page.unwrap_or(1).max(1);
    let offset = i64::try_from(page - 1)
        .unwrap_or(i64::MAX)
        .saturating_mul(PAGE_SIZE);
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
        .bind(PAGE_SIZE)
        .bind(offset)
        .fetch_all(&state.db.pool)
        .await?;
    let busy = busy_hosts(&state).await?;
    let mut items = rows
        .iter()
        .map(|row| summary_from_row(&state, row, &busy))
        .collect::<Result<Vec<_>, _>>()?;
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

#[utoipa::path(
    get,
    tag = "Jobs",
    summary = "Get a job",
    path = "/api/v1/jobs/{id}",
    params(("id" = String, Path, description = "Job id")),
    responses(
        (status = 200, description = "The job", body = JobDetail),
        (status = 404, description = "Unknown job", body = ErrorBody),
    )
)]
pub async fn get_job(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<JobDetail>, ApiError> {
    let mut job = load_job(&state, &id)
        .await?
        .ok_or_else(|| ApiError::NotFound("job not found".into()))?;
    overlay_live(&state, &mut job.summary);
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
    _user: AuthUser,
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
    let after = query.after.unwrap_or(0);
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
        last_seq: events.last().map_or(after, |e| e.seq),
        events,
    }))
}

#[utoipa::path(
    post,
    tag = "Jobs",
    summary = "Cancel a job",
    security(("session" = [], "csrf" = [])),
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
    _user: AuthUser,
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
                None => ApiError::NotFound("job not found".into()),
            });
        }
    }
    get_job(State(state), _user, Path(id)).await
}

#[utoipa::path(
    post,
    tag = "Jobs",
    summary = "Run a job again",
    security(("session" = [], "csrf" = [])),
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
    _user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<RunJob>, ApiError> {
    let job = load_job(&state, &id)
        .await?
        .ok_or_else(|| ApiError::NotFound("job not found".into()))?;
    if job.summary.source == JobSource::Cli {
        return Err(ApiError::Conflict(
            "Jobs started with the CLI cannot be repeated.".into(),
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

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_jobs))
        .routes(routes!(get_job))
        .routes(routes!(list_job_events))
        .routes(routes!(cancel_job))
        .routes(routes!(rerun_job))
}
