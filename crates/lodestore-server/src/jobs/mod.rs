pub mod api;
pub mod restore;

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use lodestore_format::locks::DEFAULT_MAX_RUNTIME;
use lodestore_policy::{BackupPolicy, Hook, IoniceClass, Limits, parse_duration, parse_rate};
use lodestore_proto::agent::v1::{
    ConflictPolicy, Hook as ProtoHook, IoniceClass as ProtoIonice, JobSpec, Limits as ProtoLimits,
};
use prost::Message;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::any::AnyRow;
use utoipa::ToSchema;

use self::restore::RestoreRecord;
use crate::db::now_ms;
use crate::download::archive::ArchiveFormat;
use crate::error::ApiError;
use crate::maintenance::check::api::{CheckPhase, CheckProgress};
use crate::maintenance::check::db::{RUN_COLUMNS, run_from_row};
use crate::maintenance::check::{CheckLevel, CheckOrigin, CheckOutcome, CheckState};
use crate::maintenance::gc::api::{GcProgress, GcReport};
use crate::maintenance::gc::db::{CYCLE_COLUMNS, cycle_from_row};
use crate::maintenance::gc::{GcOrigin, GcState};
use crate::state::AppState;

pub const ACTIVE_STATES: &str = "'queued', 'assigned', 'running'";
pub const RUNNING_STATES: &str = "'assigned', 'running'";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Waiting,
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
            "waiting" => JobState::Waiting,
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
            JobState::Waiting => &["waiting"],
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
pub enum JobOrigin {
    Manual,
    Schedule,
    Catchup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RetentionState {
    Running,
    Done,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BlockedForget {
    pub id: String,
    pub reason: String,
    #[serde(default)]
    pub kept_in_catalog: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RetentionResultDto {
    pub state: RetentionState,
    #[serde(default)]
    pub forgot: u64,
    #[serde(default)]
    pub kept: u64,
    #[serde(default)]
    pub blocked: Vec<BlockedForget>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct LimitsDto {
    pub upload_rate_bps: Option<u64>,
    pub nice: Option<u32>,
    /// `best-effort` or `idle`.
    pub ionice_class: Option<String>,
    pub ionice_level: Option<u32>,
}

impl LimitsDto {
    fn from_spec(limits: &ProtoLimits) -> Option<LimitsDto> {
        let class = match ProtoIonice::try_from(limits.ionice_class) {
            Ok(ProtoIonice::BestEffort) => Some("best-effort"),
            Ok(ProtoIonice::Idle) => Some("idle"),
            _ => None,
        };
        let positive = |n: u32| (n > 0).then_some(n);
        let dto = LimitsDto {
            upload_rate_bps: (limits.upload_rate_bps > 0).then_some(limits.upload_rate_bps),
            nice: positive(limits.nice),
            ionice_level: (class == Some("best-effort")).then_some(limits.ionice_level),
            ionice_class: class.map(str::to_string),
        };
        (dto.upload_rate_bps.is_some() || dto.nice.is_some() || dto.ionice_class.is_some())
            .then_some(dto)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobPhaseName {
    PreHook,
    Backup,
    PostHook,
    Snapshot,
    RestorePlan,
    RestoreFiles,
    RestoreMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Backup,
    Restore,
    Download,
    Gc,
    Check,
    AgentUpdate,
}

impl JobKind {
    pub fn parse(text: &str) -> Option<JobKind> {
        Some(match text {
            "backup" => JobKind::Backup,
            "restore" => JobKind::Restore,
            "download" => JobKind::Download,
            "gc" => JobKind::Gc,
            "check" => JobKind::Check,
            "agent_update" => JobKind::AgentUpdate,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicyDto {
    OverwriteChanged,
    SkipExisting,
    StopIfNotEmpty,
}

impl ConflictPolicyDto {
    pub fn to_proto(self) -> ConflictPolicy {
        match self {
            ConflictPolicyDto::OverwriteChanged => ConflictPolicy::OverwriteChanged,
            ConflictPolicyDto::SkipExisting => ConflictPolicy::SkipExisting,
            ConflictPolicyDto::StopIfNotEmpty => ConflictPolicy::StopIfNotEmpty,
        }
    }

    fn from_name(name: &str) -> ConflictPolicyDto {
        match name {
            "skip_existing" => ConflictPolicyDto::SkipExisting,
            "stop_if_not_empty" => ConflictPolicyDto::StopIfNotEmpty,
            _ => ConflictPolicyDto::OverwriteChanged,
        }
    }
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
    pub bytes_downloaded: u64,
    pub scan_done: bool,
    pub speed_bps: u64,
    pub upload_speed_bps: u64,
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
pub struct RestoreInfo {
    pub snapshot_id: String,
    /// Agent id of the source host, or `agentless:{hostname}`.
    pub source_host_key: String,
    pub source_hostname: String,
    pub destination: String,
    pub in_place: bool,
    pub conflict: ConflictPolicyDto,
    pub restore_owners: bool,
    /// The snapshot is read from a repository other than the one the target host writes to.
    pub other_repository: bool,
    pub items: u64,
    pub started_by: String,
}

#[derive(Serialize, Deserialize, ToSchema, Default)]
#[serde(default)]
pub struct RestoreResultDto {
    pub created: u64,
    pub overwritten: u64,
    pub skipped: u64,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Serialize, ToSchema)]
pub struct DownloadInfo {
    pub snapshot_id: String,
    pub format: ArchiveFormat,
    pub items: u64,
    pub bytes_sent: u64,
    pub started_by: String,
}

#[derive(Serialize, ToSchema)]
pub struct GcJobInfo {
    pub repository_id: String,
    pub repository_name: String,
    pub dry_run: bool,
    pub origin: GcOrigin,
    pub phase: GcState,
    pub progress: Option<GcProgress>,
    pub delete_after: Option<i64>,
    pub bytes_freed: Option<u64>,
    pub locked_bytes: Option<u64>,
    pub locked_until: Option<i64>,
    /// Deletion is held back by a running backup or restore.
    pub postponed: bool,
    pub stopped_while_deleting: bool,
}

#[derive(Serialize, ToSchema)]
pub struct CheckJobInfo {
    pub repository_id: String,
    pub repository_name: String,
    pub level: CheckLevel,
    pub origin: CheckOrigin,
    pub outcome: Option<CheckOutcome>,
    pub phase: Option<CheckPhase>,
    pub progress: Option<CheckProgress>,
    pub findings_total: u64,
    pub error: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct AgentUpdateInfo {
    pub from_version: String,
    pub to_version: String,
    pub rollout_id: String,
    /// Why the update was rolled back or cancelled.
    pub reason: Option<String>,
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
    pub kind: JobKind,
    pub host_id: String,
    pub hostname: String,
    pub policy_name: Option<String>,
    pub policy_exists: bool,
    pub source: JobSource,
    pub origin: JobOrigin,
    pub missed_runs: Option<u64>,
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
    pub snapshot_suspicious: bool,
    pub stats: Option<JobStatsDto>,
    pub restore: Option<RestoreInfo>,
    pub restore_result: Option<RestoreResultDto>,
    pub download: Option<DownloadInfo>,
    pub gc: Option<GcJobInfo>,
    pub check: Option<CheckJobInfo>,
    pub agent_update: Option<AgentUpdateInfo>,
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
    pub retention: Option<RetentionResultDto>,
    pub limits: Option<LimitsDto>,
}

pub(crate) const DOWNLOAD_COLUMNS: &str = "id, repository_id, host_id, hostname, snapshot_id, \
    items_json, format, started_by, state, created_at, started_at, finished_at, bytes_sent, error_code";

pub(crate) const JOB_FROM: &str = "FROM jobs j JOIN agents a ON a.id = j.host_id \
    LEFT JOIN snapshots s ON s.id = j.snapshot_id";

pub(crate) const JOB_COLUMNS: &str = "j.id, j.host_id, a.hostname, a.repository_id, j.policy_id, \
    j.policy_name, j.stdin_name, j.source, j.state, j.phase, j.cancel_requested, j.queued_at, \
    j.started_at, j.finished_at, j.snapshot_id, j.parent_id, j.incomplete, j.errors_total, \
    j.warnings_total, j.stats_json, j.progress_json, j.error_code, j.error_message, \
    j.error_detail_json, j.log_seq, j.log_lines, j.logs_purged, j.caller_uid, j.caller_gid, \
    j.kind, j.restore_json, j.origin, j.missed_runs, j.retention_json, \
    COALESCE(s.suspicious, 0) AS snapshot_suspicious";

fn hook_spec(hook: &Hook) -> ProtoHook {
    ProtoHook {
        script: hook.script.clone().unwrap_or_default(),
        command: hook.command.clone().unwrap_or_default(),
        args: hook.args.clone(),
        timeout_s: hook.timeout_secs(),
    }
}

pub fn max_runtime_of(policy: &BackupPolicy) -> Duration {
    policy
        .spec
        .limits
        .as_ref()
        .and_then(|l| l.max_runtime.as_deref())
        .and_then(parse_duration)
        .unwrap_or(DEFAULT_MAX_RUNTIME)
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
        limits: spec
            .limits
            .as_ref()
            .filter(|l| {
                Limits {
                    max_runtime: None,
                    ..(*l).clone()
                } != Limits::default()
            })
            .map(limits_spec),
        max_runtime_s: max_runtime_of(policy).as_secs(),
        gc_locks_required: false,
        distrusted_shards: Vec::new(),
    }
}

fn limits_spec(limits: &Limits) -> ProtoLimits {
    ProtoLimits {
        upload_rate_bps: limits
            .upload_rate
            .as_deref()
            .and_then(parse_rate)
            .unwrap_or(0),
        nice: limits.nice.and_then(|n| u32::try_from(n).ok()).unwrap_or(0),
        ionice_class: match limits.ionice_class {
            None => ProtoIonice::Unspecified,
            Some(IoniceClass::BestEffort) => ProtoIonice::BestEffort,
            Some(IoniceClass::Idle) => ProtoIonice::Idle,
        } as i32,
        ionice_level: limits
            .ionice_level
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0),
    }
}

async fn enqueue_one(
    state: &AppState,
    policy_id: &str,
    policy_name: &str,
    (host_id, hostname): &(String, String),
    spec: &[u8],
    origin: &str,
) -> Result<RunJob, ApiError> {
    for _ in 0..3 {
        let id = uuid::Uuid::now_v7().to_string();
        let inserted = sqlx::query(
            "INSERT INTO jobs (id, host_id, policy_id, policy_name, source, state, spec, queued_at, origin) \
             VALUES ($1, $2, $3, $4, 'manual', 'queued', $5, $6, $7) ON CONFLICT DO NOTHING",
        )
        .bind(&id)
        .bind(host_id)
        .bind(policy_id)
        .bind(policy_name)
        .bind(spec.to_vec())
        .bind(now_ms())
        .bind(origin)
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

pub const RUN_NOW_PER_HOUR: i64 = 6;

const RUN_NOW_WINDOW_MS: i64 = 3_600_000;

pub async fn check_run_rate(
    state: &AppState,
    policy_id: &str,
    hosts: &[(String, String)],
) -> Result<(), ApiError> {
    let now = now_ms();
    for (host_id, hostname) in hosts {
        let row = sqlx::query(
            "SELECT COUNT(*) AS n, MIN(queued_at) AS oldest FROM jobs \
             WHERE policy_id = $1 AND host_id = $2 AND kind = 'backup' \
             AND source = 'manual' AND origin = 'manual' AND queued_at > $3",
        )
        .bind(policy_id)
        .bind(host_id)
        .bind(now - RUN_NOW_WINDOW_MS)
        .fetch_one(&state.db.pool)
        .await?;
        let runs: i64 = row.try_get("n")?;
        if runs < RUN_NOW_PER_HOUR {
            continue;
        }
        let oldest: i64 = row.try_get("oldest")?;
        let retry_after_s = u64::try_from((oldest + RUN_NOW_WINDOW_MS - now) / 1000 + 1)
            .unwrap_or(1)
            .max(1);
        let minutes = retry_after_s.div_ceil(60);
        return Err(ApiError::TooManyRuns {
            message: format!(
                "This token started this policy on {hostname} {RUN_NOW_PER_HOUR} times in the last hour. Try again in {minutes} minutes."
            ),
            retry_after_s,
        });
    }
    Ok(())
}

pub async fn enqueue(
    state: &AppState,
    policy_id: &str,
    policy_name: &str,
    hosts: &[(String, String)],
    spec: &JobSpec,
) -> Result<Vec<RunJob>, ApiError> {
    enqueue_with_origin(state, policy_id, policy_name, hosts, spec, "manual").await
}

pub async fn enqueue_with_origin(
    state: &AppState,
    policy_id: &str,
    policy_name: &str,
    hosts: &[(String, String)],
    spec: &JobSpec,
    origin: &str,
) -> Result<Vec<RunJob>, ApiError> {
    let spec = spec.encode_to_vec();
    let mut out = Vec::with_capacity(hosts.len());
    for host in hosts {
        out.push(enqueue_one(state, policy_id, policy_name, host, &spec, origin).await?);
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
    let origin: String = row.try_get("origin")?;
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
            "restore_plan" => Some(JobPhaseName::RestorePlan),
            "restore_files" => Some(JobPhaseName::RestoreFiles),
            "restore_metadata" => Some(JobPhaseName::RestoreMetadata),
            _ => None,
        });
    let db_kind: String = row.try_get("kind")?;
    let kind = match JobKind::parse(&db_kind) {
        Some(JobKind::Download | JobKind::Gc | JobKind::Check | JobKind::AgentUpdate) | None => {
            return Err(unknown("kind", &db_kind));
        }
        Some(kind) => kind,
    };
    let restore = match kind {
        JobKind::Restore => {
            let record: RestoreRecord = json_column(row, "restore_json")?
                .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("restore_json is missing")))?;
            let target_repository: String = row.try_get("repository_id")?;
            Some(restore_info(record, &target_repository))
        }
        _ => None,
    };
    let is_restore = kind == JobKind::Restore;
    Ok(JobSummary {
        kind,
        restore,
        restore_result: if is_restore {
            json_column(row, "stats_json")?
        } else {
            None
        },
        download: None,
        gc: None,
        check: None,
        agent_update: None,
        source: match source.as_str() {
            "manual" => JobSource::Manual,
            "cli" => JobSource::Cli,
            other => return Err(unknown("source", other)),
        },
        origin: match origin.as_str() {
            "manual" => JobOrigin::Manual,
            "schedule" => JobOrigin::Schedule,
            "catchup" => JobOrigin::Catchup,
            other => return Err(unknown("origin", other)),
        },
        missed_runs: row
            .try_get::<Option<i64>, _>("missed_runs")?
            .map(|n| n.max(0) as u64),
        snapshot_suspicious: row.try_get::<i64, _>("snapshot_suspicious")? != 0,
        state: JobState::parse(&db_state).ok_or_else(|| unknown("state", &db_state))?,
        phase,
        queue_reason: queue_reason.flatten(),
        host_online,
        progress: json_column(row, "progress_json")?,
        stats: if is_restore {
            None
        } else {
            json_column(row, "stats_json")?
        },
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

fn download_error_message(code: &str) -> &'static str {
    match code {
        "client_closed" => "The browser closed the connection before the download finished.",
        "server_restarted" => "The server restarted during the download.",
        "timed_out" => "The download took longer than this server allows.",
        _ => "The archive could not be read from storage.",
    }
}

pub(crate) fn download_summary(
    state: &AppState,
    row: &AnyRow,
) -> Result<(JobSummary, String), ApiError> {
    let hostname: String = row.try_get("hostname")?;
    let host_id = row
        .try_get::<Option<String>, _>("host_id")?
        .unwrap_or_else(|| format!("{}{hostname}", crate::browse::AGENTLESS));
    let db_state: String = row.try_get("state")?;
    let created_at: i64 = row.try_get("created_at")?;
    let job_state = match db_state.as_str() {
        "pending" if created_at > now_ms() - crate::download::api::TICKET_TTL_MS => {
            JobState::Queued
        }
        "pending" => JobState::NotStarted,
        "running" => JobState::Running,
        "succeeded" => JobState::Succeeded,
        "interrupted" => JobState::Interrupted,
        other => return Err(unknown("download state", other)),
    };
    let format = crate::download::api::parse_format(&row.try_get::<String, _>("format")?)
        .ok_or_else(|| unknown("download format", "format"))?;
    let items = serde_json::from_str::<Vec<String>>(&row.try_get::<String, _>("items_json")?)
        .map_or(0, |items| items.len() as u64);
    let error_code: Option<String> = row.try_get("error_code")?;
    let summary = JobSummary {
        id: row.try_get("id")?,
        kind: JobKind::Download,
        host_online: state.registry.is_online(&host_id),
        host_id,
        hostname,
        policy_name: None,
        policy_exists: false,
        source: JobSource::Manual,
        origin: JobOrigin::Manual,
        missed_runs: None,
        snapshot_suspicious: false,
        stdin_name: None,
        state: job_state,
        phase: None,
        queued_at: created_at,
        started_at: row.try_get("started_at")?,
        finished_at: row.try_get("finished_at")?,
        queue_reason: None,
        progress: None,
        snapshot_id: Some(row.try_get("snapshot_id")?),
        stats: None,
        restore: None,
        restore_result: None,
        download: Some(DownloadInfo {
            snapshot_id: row.try_get("snapshot_id")?,
            format,
            items,
            bytes_sent: count(row, "bytes_sent")?,
            started_by: row.try_get("started_by")?,
        }),
        gc: None,
        check: None,
        agent_update: None,
        incomplete: false,
        warnings_total: 0,
        errors_total: 0,
        error_message: error_code
            .as_deref()
            .map(|code| download_error_message(code).to_string()),
        error_code,
    };
    Ok((summary, row.try_get("repository_id")?))
}

const GC_EXTRA_COLUMNS: &str = "summary_json, \
    (SELECT name FROM repositories r WHERE r.id = gc_cycles.repository_id) AS repository_name";

pub(crate) fn gc_select() -> String {
    format!("SELECT {CYCLE_COLUMNS}, {GC_EXTRA_COLUMNS} FROM gc_cycles")
}

pub(crate) fn gc_summary(row: &AnyRow) -> Result<JobSummary, ApiError> {
    let cycle = cycle_from_row(row).map_err(ApiError::Internal)?;
    let report: Option<GcReport> = json_column(row, "summary_json")?;
    let state = match cycle.state {
        GcState::Marking | GcState::Repacking | GcState::Verifying | GcState::Sweeping => {
            JobState::Running
        }
        GcState::Waiting => JobState::Waiting,
        GcState::Done => JobState::Succeeded,
        GcState::Failed => JobState::Failed,
        GcState::Aborted => JobState::Cancelled,
    };
    let p = &cycle.progress;
    let info = GcJobInfo {
        repository_name: row.try_get("repository_name")?,
        repository_id: cycle.repository_id,
        dry_run: cycle.dry_run,
        origin: cycle.origin,
        phase: cycle.state,
        progress: p.shown(),
        delete_after: cycle.delete_after,
        bytes_freed: report.as_ref().map(|r| r.bytes_freed),
        locked_bytes: report.as_ref().map(|r| r.locked_bytes),
        locked_until: report.as_ref().and_then(|r| r.locked_until),
        postponed: !p.blocked_by_writers.is_empty() || p.waiting_for_restores,
        stopped_while_deleting: cycle.state == GcState::Failed
            && (cycle.sweep_started || cycle.error_after_delete),
    };
    Ok(JobSummary {
        id: cycle.id,
        kind: JobKind::Gc,
        host_id: String::new(),
        hostname: String::new(),
        policy_name: None,
        policy_exists: false,
        source: JobSource::Manual,
        origin: match cycle.origin {
            GcOrigin::Manual => JobOrigin::Manual,
            GcOrigin::Schedule => JobOrigin::Schedule,
        },
        missed_runs: None,
        stdin_name: None,
        state,
        phase: None,
        queued_at: cycle.created_at,
        started_at: cycle.started_at,
        finished_at: cycle.finished_at,
        queue_reason: None,
        host_online: false,
        progress: None,
        snapshot_id: None,
        snapshot_suspicious: false,
        stats: None,
        restore: None,
        restore_result: None,
        download: None,
        gc: Some(info),
        check: None,
        agent_update: None,
        incomplete: false,
        warnings_total: 0,
        errors_total: 0,
        error_code: None,
        error_message: cycle.error,
    })
}

const CHECK_EXTRA_COLUMNS: &str =
    "(SELECT name FROM repositories r WHERE r.id = check_runs.repository_id) AS repository_name";

pub(crate) fn check_select() -> String {
    format!("SELECT {RUN_COLUMNS}, {CHECK_EXTRA_COLUMNS} FROM check_runs")
}

pub(crate) fn check_summary(row: &AnyRow) -> Result<JobSummary, ApiError> {
    let run = run_from_row(row).map_err(ApiError::Internal)?;
    let state = match run.state {
        CheckState::Queued => JobState::Queued,
        CheckState::Running => JobState::Running,
        CheckState::Done => JobState::Succeeded,
        CheckState::Failed => JobState::Failed,
        CheckState::Aborted => JobState::Cancelled,
    };
    let progress = run.progress.filter(|_| run.state.is_active());
    let info = CheckJobInfo {
        repository_name: row.try_get("repository_name")?,
        repository_id: run.repository_id,
        level: run.level,
        origin: run.origin,
        outcome: run.outcome,
        phase: progress.as_ref().map(|p| p.phase),
        progress,
        findings_total: run.findings_total,
        error: run.error.clone(),
    };
    Ok(JobSummary {
        id: run.id,
        kind: JobKind::Check,
        host_id: String::new(),
        hostname: String::new(),
        policy_name: None,
        policy_exists: false,
        source: JobSource::Manual,
        origin: match run.origin {
            CheckOrigin::Manual => JobOrigin::Manual,
            CheckOrigin::Schedule => JobOrigin::Schedule,
        },
        missed_runs: None,
        stdin_name: None,
        state,
        phase: None,
        queued_at: run.created_at,
        started_at: run.started_at,
        finished_at: run.finished_at,
        queue_reason: None,
        host_online: false,
        progress: None,
        snapshot_id: None,
        snapshot_suspicious: false,
        stats: None,
        restore: None,
        restore_result: None,
        download: None,
        gc: None,
        check: Some(info),
        agent_update: None,
        incomplete: false,
        warnings_total: 0,
        errors_total: 0,
        error_code: None,
        error_message: run.error,
    })
}

pub(crate) const AGENT_UPDATE_COLUMNS: &str = "u.id, u.rollout_id, u.host_id, u.from_version, \
     u.target_version, u.state, u.reason, u.queued_at, u.assigned_at, u.finished_at, \
     a.hostname AS hostname";

pub(crate) const AGENT_UPDATE_FROM: &str = "FROM agent_updates u JOIN agents a ON a.id = u.host_id";

pub(crate) fn agent_update_summary(
    state: &AppState,
    row: &AnyRow,
    busy: &HashSet<String>,
) -> Result<JobSummary, ApiError> {
    let host_id: String = row.try_get("host_id")?;
    let db_state: String = row.try_get("state")?;
    let reason: Option<String> = row.try_get("reason")?;
    let host_online = state.registry.is_online(&host_id);
    let job_state = match db_state.as_str() {
        "queued" => JobState::Queued,
        "assigned" => JobState::Running,
        "succeeded" => JobState::Succeeded,
        "rolled_back" => JobState::Failed,
        "cancelled" => JobState::Cancelled,
        other => return Err(unknown("agent update state", other)),
    };
    let queue_reason = (db_state == "queued")
        .then(|| {
            if !host_online {
                Some(QueueReason::HostOffline)
            } else if busy.contains(&host_id) {
                Some(QueueReason::HostBusy)
            } else {
                None
            }
        })
        .flatten();
    let info = AgentUpdateInfo {
        from_version: row.try_get("from_version")?,
        to_version: row.try_get("target_version")?,
        rollout_id: row.try_get("rollout_id")?,
        reason: reason.clone(),
    };
    Ok(JobSummary {
        id: row.try_get("id")?,
        kind: JobKind::AgentUpdate,
        host_id,
        hostname: row.try_get("hostname")?,
        policy_name: None,
        policy_exists: false,
        source: JobSource::Manual,
        origin: JobOrigin::Manual,
        missed_runs: None,
        stdin_name: None,
        state: job_state,
        phase: None,
        queued_at: row.try_get("queued_at")?,
        started_at: row.try_get("assigned_at")?,
        finished_at: row.try_get("finished_at")?,
        queue_reason,
        host_online,
        progress: None,
        snapshot_id: None,
        snapshot_suspicious: false,
        stats: None,
        restore: None,
        restore_result: None,
        download: None,
        gc: None,
        check: None,
        agent_update: Some(info),
        incomplete: false,
        warnings_total: 0,
        errors_total: 0,
        error_code: (db_state == "rolled_back").then(|| "agent_update_rolled_back".to_string()),
        error_message: reason,
    })
}

pub async fn load_agent_update(state: &AppState, id: &str) -> Result<Option<JobDetail>, ApiError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {AGENT_UPDATE_COLUMNS} {AGENT_UPDATE_FROM} WHERE u.id = $1"
    )))
    .bind(id)
    .fetch_optional(&state.db.pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    Ok(Some(JobDetail {
        summary: agent_update_summary(state, &row, &busy_hosts(state).await?)?,
        repository_id: String::new(),
        parent_id: None,
        error_detail: BTreeMap::new(),
        caller_uid: None,
        caller_gid: None,
        cancel_requested: false,
        log_seq: 0,
        log_lines: 0,
        logs_purged: false,
        retention: None,
        limits: None,
    }))
}

pub async fn load_download(state: &AppState, id: &str) -> Result<Option<JobDetail>, ApiError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {DOWNLOAD_COLUMNS} FROM downloads WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(&state.db.pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let (summary, repository_id) = download_summary(state, &row)?;
    Ok(Some(JobDetail {
        summary,
        repository_id,
        parent_id: None,
        error_detail: BTreeMap::new(),
        caller_uid: None,
        caller_gid: None,
        cancel_requested: false,
        log_seq: 0,
        log_lines: 0,
        logs_purged: false,
        retention: None,
        limits: None,
    }))
}

fn restore_info(record: RestoreRecord, target_repository: &str) -> RestoreInfo {
    RestoreInfo {
        source_host_key: record
            .source_host_id
            .clone()
            .unwrap_or_else(|| format!("{}{}", crate::browse::AGENTLESS, record.source_hostname)),
        other_repository: record.source_repository_id != target_repository,
        conflict: ConflictPolicyDto::from_name(&record.conflict),
        items: record.items_b64.len() as u64,
        snapshot_id: record.snapshot_id,
        source_hostname: record.source_hostname,
        destination: record.destination,
        in_place: record.in_place,
        restore_owners: record.restore_owners,
        started_by: record.started_by,
    }
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
        retention: json_column(row, "retention_json")?,
        limits: limits_of(row)?,
    })
}

fn limits_of(row: &AnyRow) -> Result<Option<LimitsDto>, ApiError> {
    if row.try_get::<String, _>("kind")? != "backup" {
        return Ok(None);
    }
    let raw: Vec<u8> = row.try_get("spec")?;
    Ok(JobSpec::decode(raw.as_slice())
        .ok()
        .and_then(|spec| spec.limits)
        .and_then(|limits| LimitsDto::from_spec(&limits)))
}

async fn mark_catalog_presence(state: &AppState, job: &mut JobDetail) -> Result<(), ApiError> {
    let Some(retention) = job.retention.as_mut() else {
        return Ok(());
    };
    for blocked in &mut retention.blocked {
        blocked.kept_in_catalog = sqlx::query("SELECT 1 FROM snapshots WHERE id = $1")
            .bind(&blocked.id)
            .fetch_optional(&state.db.pool)
            .await?
            .is_some();
    }
    Ok(())
}

pub async fn load_job(state: &AppState, id: &str) -> Result<Option<JobDetail>, ApiError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {JOB_COLUMNS}, j.spec {JOB_FROM} WHERE j.id = $1"
    )))
    .bind(id)
    .fetch_optional(&state.db.pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let mut job = detail_from_row(state, &row, &busy_hosts(state).await?)?;
    mark_catalog_presence(state, &mut job).await?;
    Ok(Some(job))
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
