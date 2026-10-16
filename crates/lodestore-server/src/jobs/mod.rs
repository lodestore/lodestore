pub mod api;

use std::collections::{BTreeMap, HashSet};

use lodestore_policy::{BackupPolicy, Hook};
use lodestore_proto::agent::v1::{Hook as ProtoHook, JobSpec};
use prost::Message;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::any::AnyRow;
use utoipa::ToSchema;

use crate::db::now_ms;
use crate::error::ApiError;
use crate::state::AppState;

pub const ACTIVE_STATES: &str = "'queued', 'assigned', 'running'";
pub const RUNNING_STATES: &str = "'assigned', 'running'";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Succeeded,
    Warnings,
    Failed,
    Cancelled,
    Interrupted,
    NotStarted,
}

impl JobState {
    pub fn parse(text: &str) -> Option<JobState> {
        Some(match text {
            "queued" | "assigned" => JobState::Queued,
            "running" => JobState::Running,
            "succeeded" => JobState::Succeeded,
            "warnings" => JobState::Warnings,
            "failed" => JobState::Failed,
            "cancelled" => JobState::Cancelled,
            "interrupted" => JobState::Interrupted,
            "not_started" => JobState::NotStarted,
            _ => return None,
        })
    }

    pub fn db_names(self) -> &'static [&'static str] {
        match self {
            JobState::Queued => &["queued", "assigned"],
            JobState::Running => &["running"],
            JobState::Succeeded => &["succeeded"],
            JobState::Warnings => &["warnings"],
            JobState::Failed => &["failed"],
            JobState::Cancelled => &["cancelled"],
            JobState::Interrupted => &["interrupted"],
            JobState::NotStarted => &["not_started"],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobSource {
    Manual,
    Cli,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobPhaseName {
    PreHook,
    Backup,
    PostHook,
    Snapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum QueueReason {
    HostOffline,
    HostBusy,
}

#[derive(Serialize, Deserialize, ToSchema, Default)]
#[serde(default)]
pub struct JobProgressDto {
    pub files_done: u64,
    pub files_total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub bytes_uploaded: u64,
    pub scan_done: bool,
    pub speed_bps: u64,
    pub eta_s: Option<i64>,
    pub updated_at: i64,
}

#[derive(Serialize, Deserialize, ToSchema, Default)]
#[serde(default)]
pub struct JobStatsDto {
    pub files_new: u64,
    pub files_changed: u64,
    pub files_unchanged: u64,
    pub dirs: u64,
    pub bytes_total: u64,
    pub bytes_read: u64,
    pub bytes_added: u64,
}

#[derive(Serialize, ToSchema)]
pub struct JobBrief {
    pub id: String,
    pub state: JobState,
    pub queued_at: i64,
    pub finished_at: Option<i64>,
}

#[derive(Serialize, ToSchema)]
pub struct RunJob {
    pub id: String,
    pub host_id: String,
    pub hostname: String,
    pub created: bool,
}

#[derive(Serialize, ToSchema)]
pub struct RunResult {
    pub jobs: Vec<RunJob>,
}

#[derive(Serialize, ToSchema)]
pub struct JobSummary {
    pub id: String,
    pub host_id: String,
    pub hostname: String,
    pub policy_name: Option<String>,
    pub policy_exists: bool,
    pub source: JobSource,
    pub stdin_name: Option<String>,
    pub state: JobState,
    pub phase: Option<JobPhaseName>,
    pub queued_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub queue_reason: Option<QueueReason>,
    pub host_online: bool,
    pub progress: Option<JobProgressDto>,
    pub snapshot_id: Option<String>,
    pub stats: Option<JobStatsDto>,
    pub incomplete: bool,
    pub warnings_total: u64,
    pub errors_total: u64,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct JobDetail {
    #[serde(flatten)]
    pub summary: JobSummary,
    pub repository_id: String,
    pub parent_id: Option<String>,
    pub error_detail: BTreeMap<String, String>,
    pub caller_uid: Option<i64>,
    pub caller_gid: Option<i64>,
    pub cancel_requested: bool,
    pub log_seq: u64,
    pub log_lines: u64,
    pub logs_purged: bool,
}

pub(crate) const JOB_FROM: &str = "FROM jobs j JOIN agents a ON a.id = j.host_id";

pub(crate) const JOB_COLUMNS: &str = "j.id, j.host_id, a.hostname, a.repository_id, j.policy_id, \
    j.policy_name, j.stdin_name, j.source, j.state, j.phase, j.cancel_requested, j.queued_at, \
    j.started_at, j.finished_at, j.snapshot_id, j.parent_id, j.incomplete, j.errors_total, \
    j.warnings_total, j.stats_json, j.progress_json, j.error_code, j.error_message, \
    j.error_detail_json, j.log_seq, j.log_lines, j.logs_purged, j.caller_uid, j.caller_gid";

fn hook_spec(hook: &Hook) -> ProtoHook {
    ProtoHook {
        script: hook.script.clone().unwrap_or_default(),
        command: hook.command.clone().unwrap_or_default(),
        args: hook.args.clone(),
        timeout_s: hook.timeout_secs(),
    }
}

pub fn job_spec(policy: &BackupPolicy) -> JobSpec {
    let spec = &policy.spec;
    JobSpec {
        policy_name: policy.metadata.name.clone(),
        paths: spec.paths.clone(),
        exclude: spec.exclude.clone(),
        one_file_system: spec.one_file_system,
        exclude_caches: spec.exclude_caches,
        before: spec.hooks.before.iter().map(hook_spec).collect(),
        after: spec.hooks.after.iter().map(hook_spec).collect(),
        tags: vec![format!("policy={}", policy.metadata.name)],
        stdin_name: String::new(),
    }
}

async fn enqueue_one(
    state: &AppState,
    policy_id: &str,
    policy_name: &str,
    (host_id, hostname): &(String, String),
    spec: &[u8],
) -> Result<RunJob, ApiError> {
    for _ in 0..3 {
        let id = uuid::Uuid::now_v7().to_string();
        let inserted = sqlx::query(
            "INSERT INTO jobs (id, host_id, policy_id, policy_name, source, state, spec, queued_at) \
             VALUES ($1, $2, $3, $4, 'manual', 'queued', $5, $6) ON CONFLICT DO NOTHING",
        )
        .bind(&id)
        .bind(host_id)
        .bind(policy_id)
        .bind(policy_name)
        .bind(spec.to_vec())
        .bind(now_ms())
        .execute(&state.db.pool)
        .await?
        .rows_affected()
            == 1;
        let (id, created) = if inserted {
            (id, true)
        } else {
            let existing = sqlx::query(sqlx::AssertSqlSafe(format!(
                "SELECT id FROM jobs WHERE host_id = $1 AND policy_id = $2 \
                 AND state IN ({ACTIVE_STATES})"
            )))
            .bind(host_id)
            .bind(policy_id)
            .fetch_optional(&state.db.pool)
            .await?;
            let Some(row) = existing else { continue };
            (row.try_get("id")?, false)
        };
        return Ok(RunJob {
            id,
            host_id: host_id.clone(),
            hostname: hostname.clone(),
            created,
        });
    }
    Err(ApiError::Conflict(
        "The job could not be queued. Try again.".into(),
    ))
}

pub async fn enqueue(
    state: &AppState,
    policy_id: &str,
    policy_name: &str,
    hosts: &[(String, String)],
    spec: &JobSpec,
) -> Result<Vec<RunJob>, ApiError> {
    let spec = spec.encode_to_vec();
    let mut out = Vec::with_capacity(hosts.len());
    for host in hosts {
        out.push(enqueue_one(state, policy_id, policy_name, host, &spec).await?);
    }
    Ok(out)
}

pub async fn busy_hosts(state: &AppState) -> Result<HashSet<String>, ApiError> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT DISTINCT host_id FROM jobs WHERE state IN ({RUNNING_STATES})"
    )))
    .fetch_all(&state.db.pool)
    .await?;
    rows.iter()
        .map(|r| r.try_get("host_id").map_err(ApiError::from))
        .collect()
}

fn count(row: &AnyRow, column: &str) -> Result<u64, ApiError> {
    Ok(row.try_get::<i64, _>(column)?.max(0) as u64)
}

fn json_column<T: serde::de::DeserializeOwned>(
    row: &AnyRow,
    column: &str,
) -> Result<Option<T>, ApiError> {
    Ok(row
        .try_get::<Option<String>, _>(column)?
        .and_then(|text| serde_json::from_str(&text).ok()))
}

fn unknown(what: &str, value: &str) -> ApiError {
    ApiError::Internal(anyhow::anyhow!("unknown job {what} \"{value}\""))
}

pub(crate) fn summary_from_row(
    state: &AppState,
    row: &AnyRow,
    busy: &HashSet<String>,
) -> Result<JobSummary, ApiError> {
    let id: String = row.try_get("id")?;
    let host_id: String = row.try_get("host_id")?;
    let db_state: String = row.try_get("state")?;
    let source: String = row.try_get("source")?;
    let host_online = state.registry.is_online(&host_id);
    let queue_reason = (db_state == "queued").then(|| {
        if !host_online {
            Some(QueueReason::HostOffline)
        } else if busy.contains(&host_id) {
            Some(QueueReason::HostBusy)
        } else {
            None
        }
    });
    let phase = row
        .try_get::<Option<String>, _>("phase")?
        .and_then(|p| match p.as_str() {
            "pre_hook" => Some(JobPhaseName::PreHook),
            "backup" => Some(JobPhaseName::Backup),
            "post_hook" => Some(JobPhaseName::PostHook),
            "snapshot" => Some(JobPhaseName::Snapshot),
            _ => None,
        });
    Ok(JobSummary {
        source: match source.as_str() {
            "manual" => JobSource::Manual,
            "cli" => JobSource::Cli,
            other => return Err(unknown("source", other)),
        },
        state: JobState::parse(&db_state).ok_or_else(|| unknown("state", &db_state))?,
        phase,
        queue_reason: queue_reason.flatten(),
        host_online,
        progress: json_column(row, "progress_json")?,
        stats: json_column(row, "stats_json")?,
        hostname: row.try_get("hostname")?,
        policy_name: row.try_get("policy_name")?,
        policy_exists: row.try_get::<Option<String>, _>("policy_id")?.is_some(),
        stdin_name: row.try_get("stdin_name")?,
        queued_at: row.try_get("queued_at")?,
        started_at: row.try_get("started_at")?,
        finished_at: row.try_get("finished_at")?,
        snapshot_id: row.try_get("snapshot_id")?,
        incomplete: row.try_get::<i64, _>("incomplete")? != 0,
        warnings_total: count(row, "warnings_total")?,
        errors_total: count(row, "errors_total")?,
        error_code: row.try_get("error_code")?,
        error_message: row.try_get("error_message")?,
        id,
        host_id,
    })
}

fn detail_from_row(
    state: &AppState,
    row: &AnyRow,
    busy: &HashSet<String>,
) -> Result<JobDetail, ApiError> {
    Ok(JobDetail {
        summary: summary_from_row(state, row, busy)?,
        repository_id: row.try_get("repository_id")?,
        parent_id: row.try_get("parent_id")?,
        error_detail: json_column(row, "error_detail_json")?.unwrap_or_default(),
        caller_uid: row.try_get("caller_uid")?,
        caller_gid: row.try_get("caller_gid")?,
        cancel_requested: row.try_get::<i64, _>("cancel_requested")? != 0,
        log_seq: count(row, "log_seq")?,
        log_lines: count(row, "log_lines")?,
        logs_purged: row.try_get::<i64, _>("logs_purged")? != 0,
    })
}

pub async fn load_job(state: &AppState, id: &str) -> Result<Option<JobDetail>, ApiError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {JOB_COLUMNS} {JOB_FROM} WHERE j.id = $1"
    )))
    .bind(id)
    .fetch_optional(&state.db.pool)
    .await?;
    match row {
        Some(row) => Ok(Some(detail_from_row(
            state,
            &row,
            &busy_hosts(state).await?,
        )?)),
        None => Ok(None),
    }
}

pub async fn last_job(
    state: &AppState,
    policy_id: &str,
    host_id: Option<&str>,
) -> Result<Option<JobBrief>, ApiError> {
    let host_clause = if host_id.is_some() {
        " AND host_id = $2"
    } else {
        ""
    };
    let query = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT id, state, queued_at, finished_at FROM jobs WHERE policy_id = $1{host_clause} \
         ORDER BY queued_at DESC, id DESC LIMIT 1"
    )))
    .bind(policy_id);
    let query = match host_id {
        Some(host) => query.bind(host),
        None => query,
    };
    let Some(row) = query.fetch_optional(&state.db.pool).await? else {
        return Ok(None);
    };
    let db_state: String = row.try_get("state")?;
    Ok(Some(JobBrief {
        id: row.try_get("id")?,
        state: JobState::parse(&db_state).ok_or_else(|| unknown("state", &db_state))?,
        queued_at: row.try_get("queued_at")?,
        finished_at: row.try_get("finished_at")?,
    }))
}
