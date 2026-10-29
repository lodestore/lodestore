use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use lodestore_policy::{Schedule, parse_size};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::db::{self, RunRow};
use super::{
    CheckBlocked, CheckLevel, CheckLevelSettings, CheckOrigin, CheckOutcome, CheckState,
    MAX_FILES_PER_RUN, blocked, start_check,
};
use crate::auth::tokens::{JobsRead, JobsRun, PoliciesWrite, Scoped};
use crate::db::now_ms;
use crate::error::{ApiError, ErrorBody};
use crate::maintenance::gc::api::FieldError;
use crate::policies::{ScheduleInfo, schedule_info_of};
use crate::repos::db::get_repository;
use crate::state::AppState;

const FINDINGS_SHOWN: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckPhase {
    Listing,
    Indexes,
    Snapshots,
    Packs,
    Restoring,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CheckProgress {
    pub phase: CheckPhase,
    pub done: u64,
    pub total: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StructureSummary {
    pub snapshots_checked: u64,
    pub trees_checked: u64,
    pub packs_listed: u64,
    pub missing_packs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PacksSummary {
    pub packs_read: u64,
    pub bytes_read: u64,
    pub blobs_verified: u64,
    pub damaged_blobs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RestoreSummary {
    pub snapshots_sampled: u64,
    pub files_restored: u64,
    pub bytes_restored: u64,
    /// Files that did not come back right or could not be restored.
    pub hash_mismatches: u64,
    /// Chosen files that no longer fitted in the size limit.
    pub skipped: u64,
    pub stopped_at_limit: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct CheckSummary {
    pub structure: Option<StructureSummary>,
    pub packs: Option<PacksSummary>,
    pub restore: Option<RestoreSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CheckFinding {
    /// For example `missing_pack`, `pack_mismatch` or `damaged_blob`.
    pub kind: String,
    /// The object key the finding is about.
    pub key: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CheckNote {
    pub kind: String,
    pub key: String,
    pub detail: String,
}

/// What a finished run keeps. `findings` and `notes` are capped; their
/// totals are not.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StoredReport {
    pub summary: CheckSummary,
    pub findings: Vec<CheckFinding>,
    pub findings_total: u64,
    pub notes: Vec<CheckNote>,
    #[serde(default)]
    pub notes_total: u64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CheckRunDto {
    pub id: String,
    pub level: CheckLevel,
    pub origin: CheckOrigin,
    pub state: CheckState,
    pub outcome: Option<CheckOutcome>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub duration_ms: Option<u64>,
    pub started_by: String,
    pub progress: Option<CheckProgress>,
    pub findings_total: u64,
    pub error: Option<String>,
    pub summary: CheckSummary,
    /// The first findings; the report lists them all.
    pub findings: Vec<CheckFinding>,
}

impl From<&RunRow> for CheckRunDto {
    fn from(run: &RunRow) -> Self {
        let report = run.report.as_ref();
        CheckRunDto {
            id: run.id.clone(),
            level: run.level,
            origin: run.origin,
            state: run.state,
            outcome: run.outcome,
            created_at: run.created_at,
            started_at: run.started_at,
            finished_at: run.finished_at,
            duration_ms: run
                .started_at
                .zip(run.finished_at)
                .map(|(start, end)| (end - start).max(0) as u64),
            started_by: run.started_by.clone(),
            progress: run.progress.clone().filter(|_| run.state.is_active()),
            findings_total: run.findings_total,
            error: run.error.clone(),
            summary: report.map(|r| r.summary.clone()).unwrap_or_default(),
            findings: report
                .map(|r| r.findings.iter().take(FINDINGS_SHOWN).cloned().collect())
                .unwrap_or_default(),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct CheckLevelStatus {
    pub level: CheckLevel,
    pub settings: CheckLevelSettings,
    pub schedule: Option<ScheduleInfo>,
    pub running: Option<CheckRunDto>,
    /// The newest run that reached a verdict.
    pub last: Option<CheckRunDto>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RepoUsageDto {
    pub physical_bytes: u64,
    pub logical_bytes: u64,
    /// Logical size divided by the size in the bucket.
    pub dedup_ratio: Option<f64>,
    pub packs: u64,
    pub counted_at: i64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PackCoverage {
    pub packs: u64,
    /// Whole percent of packs read in the last 30 days.
    pub checked_30d_pct: u64,
    pub never_checked: u64,
}

#[derive(Serialize, ToSchema)]
pub struct CheckStatus {
    pub repository_id: String,
    pub levels: Vec<CheckLevelStatus>,
    pub usage: Option<RepoUsageDto>,
    pub coverage: Option<PackCoverage>,
    pub blocked: Option<CheckBlocked>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CheckStarted {
    pub run_id: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CheckFullReport {
    pub run: CheckRunDto,
    pub findings: Vec<CheckFinding>,
    pub notes: Vec<CheckNote>,
    pub notes_total: u64,
}

async fn repository(state: &AppState, id: &str) -> Result<(), ApiError> {
    get_repository(state, id)
        .await?
        .map(drop)
        .ok_or_else(|| ApiError::NotFound("repository not found".into()))
}

async fn run_of(state: &AppState, id: &str, run_id: &str) -> Result<RunRow, ApiError> {
    db::load_run(state, run_id)
        .await?
        .filter(|run| run.repository_id == id)
        .ok_or_else(|| ApiError::NotFound("check not found".into()))
}

pub(crate) async fn build_status(
    state: &AppState,
    repository_id: &str,
) -> Result<CheckStatus, ApiError> {
    let now = now_ms();
    let active = db::active_run(state, repository_id).await?;
    let mut levels = Vec::new();
    for (level, settings) in db::load_settings(state, repository_id).await? {
        let schedule = schedule_info_of(
            &Schedule {
                cron: settings.cron.clone(),
                timezone: Some(settings.timezone.clone()),
                jitter: None,
                catch_up_window: None,
            },
            now,
        );
        levels.push(CheckLevelStatus {
            level,
            schedule,
            settings,
            running: active
                .as_ref()
                .filter(|run| run.level == level)
                .map(CheckRunDto::from),
            last: db::last_finished(state, repository_id, level)
                .await?
                .as_ref()
                .map(CheckRunDto::from),
        });
    }
    Ok(CheckStatus {
        repository_id: repository_id.to_string(),
        levels,
        usage: db::load_usage(state, repository_id).await?,
        coverage: db::coverage(state, repository_id, now).await?,
        blocked: blocked(state, repository_id).await?,
    })
}

#[utoipa::path(
    get,
    tag = "Checks",
    operation_id = "get_check_status",
    summary = "Get the check status of a repository",
    description = "Per level: the settings, the next scheduled runs, the running check and the \
        last one that finished. Also the size of the bucket and how much of it pack reading has \
        covered lately.",
    security(("session" = []), ("bearer" = ["jobs:read"])),
    path = "/api/v1/repositories/{id}/checks",
    params(("id" = String, Path, description = "Repository id")),
    responses(
        (status = 200, description = "The check status", body = CheckStatus),
        (status = 404, description = "Unknown repository", body = ErrorBody),
    )
)]
pub async fn get_status(
    State(state): State<AppState>,
    _auth: Scoped<JobsRead>,
    Path(id): Path<String>,
) -> Result<Json<CheckStatus>, ApiError> {
    repository(&state, &id).await?;
    Ok(Json(build_status(&state, &id).await?))
}

#[utoipa::path(
    post,
    tag = "Checks",
    operation_id = "start_check",
    summary = "Start a check now",
    description = "The server reads the bucket with its own key; no agent is needed. Levels: \
        `structure`, `packs` and `restore` (restores a sample of files into a scratch folder \
        and deletes it again). Follow the run in the check status or in the jobs list.",
    security(("session" = [], "csrf" = []), ("bearer" = ["jobs:run"])),
    path = "/api/v1/repositories/{id}/checks/{level}/run",
    params(
        ("id" = String, Path, description = "Repository id"),
        ("level" = String, Path, description = "`structure`, `packs` or `restore`"),
    ),
    responses(
        (status = 202, description = "The check started", body = CheckStarted),
        (status = 404, description = "Unknown repository", body = ErrorBody),
        (status = 409, description = "A cleanup cycle or another check is running", body = ErrorBody),
        (status = 422, description = "Unknown level", body = ErrorBody),
    )
)]
pub async fn start_run(
    State(state): State<AppState>,
    auth: Scoped<JobsRun>,
    Path((id, level)): Path<(String, String)>,
) -> Result<(StatusCode, Json<CheckStarted>), ApiError> {
    let level = CheckLevel::parse(&level)
        .ok_or_else(|| ApiError::Validation(format!("Unknown check level \"{level}\".")))?;
    let run_id = start_check(&state, &id, level, CheckOrigin::Manual, &auth.actor()).await?;
    Ok((StatusCode::ACCEPTED, Json(CheckStarted { run_id })))
}

#[utoipa::path(
    post,
    tag = "Checks",
    operation_id = "cancel_check",
    summary = "Stop a running check",
    description = "A queued check ends at once; a running one stops within a second or so.",
    security(("session" = [], "csrf" = []), ("bearer" = ["jobs:run"])),
    path = "/api/v1/repositories/{id}/checks/runs/{run_id}/cancel",
    params(
        ("id" = String, Path, description = "Repository id"),
        ("run_id" = String, Path, description = "Check run id"),
    ),
    responses(
        (status = 202, description = "The check is stopping", body = CheckStarted),
        (status = 404, description = "Unknown repository or check", body = ErrorBody),
        (status = 409, description = "The check has ended already", body = ErrorBody),
    )
)]
pub async fn cancel_run(
    State(state): State<AppState>,
    _auth: Scoped<JobsRun>,
    Path((id, run_id)): Path<(String, String)>,
) -> Result<(StatusCode, Json<CheckStarted>), ApiError> {
    let run = run_of(&state, &id, &run_id).await?;
    let stopped = match run.state {
        CheckState::Queued => db::cancel_queued(&state, &run.id).await?,
        CheckState::Running => db::request_cancel(&state, &run.id).await?,
        CheckState::Done | CheckState::Failed | CheckState::Aborted => false,
    };
    if !stopped {
        return Err(ApiError::Conflict("This check has ended already.".into()));
    }
    Ok((StatusCode::ACCEPTED, Json(CheckStarted { run_id })))
}

#[utoipa::path(
    get,
    tag = "Checks",
    operation_id = "get_check_run",
    summary = "Get a check run",
    security(("session" = []), ("bearer" = ["jobs:read"])),
    path = "/api/v1/repositories/{id}/checks/runs/{run_id}",
    params(
        ("id" = String, Path, description = "Repository id"),
        ("run_id" = String, Path, description = "Check run id"),
    ),
    responses(
        (status = 200, description = "The run", body = CheckRunDto),
        (status = 404, description = "Unknown repository or check", body = ErrorBody),
    )
)]
pub async fn get_run(
    State(state): State<AppState>,
    _auth: Scoped<JobsRead>,
    Path((id, run_id)): Path<(String, String)>,
) -> Result<Json<CheckRunDto>, ApiError> {
    Ok(Json(CheckRunDto::from(
        &run_of(&state, &id, &run_id).await?,
    )))
}

#[utoipa::path(
    get,
    tag = "Checks",
    operation_id = "get_check_report",
    summary = "Get the full report of a finished check",
    description = "Lists every finding and note, not only the first few.",
    security(("session" = []), ("bearer" = ["jobs:read"])),
    path = "/api/v1/repositories/{id}/checks/runs/{run_id}/report",
    params(
        ("id" = String, Path, description = "Repository id"),
        ("run_id" = String, Path, description = "Check run id"),
    ),
    responses(
        (status = 200, description = "The report", body = CheckFullReport),
        (status = 404, description = "Unknown repository or check, or a check without a report", body = ErrorBody),
    )
)]
pub async fn get_report(
    State(state): State<AppState>,
    _auth: Scoped<JobsRead>,
    Path((id, run_id)): Path<(String, String)>,
) -> Result<Json<CheckFullReport>, ApiError> {
    let run = run_of(&state, &id, &run_id).await?;
    let report = run
        .report
        .clone()
        .ok_or_else(|| ApiError::NotFound("report not found".into()))?;
    Ok(Json(CheckFullReport {
        run: CheckRunDto::from(&run),
        findings: report.findings,
        notes: report.notes,
        notes_total: report.notes_total,
    }))
}

const MIN_DATA_BYTES: u64 = 1 << 20;

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CheckLevelInput {
    pub enabled: bool,
    pub cron: String,
    pub timezone: String,
    /// `packs` only: a size such as `5GiB`. Left out, the saved value stays.
    pub read_per_run: Option<String>,
    /// `restore` only: a whole number from 1 to 1000. Left out, the saved value stays.
    pub files_per_run: Option<i64>,
    /// `restore` only: a size such as `1GiB`. Left out, the saved value stays.
    pub max_data_per_run: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CheckSettingsInput {
    pub structure: CheckLevelInput,
    pub packs: CheckLevelInput,
    pub restore: CheckLevelInput,
}

impl CheckSettingsInput {
    fn level(&self, level: CheckLevel) -> &CheckLevelInput {
        match level {
            CheckLevel::Structure => &self.structure,
            CheckLevel::Packs => &self.packs,
            CheckLevel::Restore => &self.restore,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct CheckSettingsErrors {
    pub errors: Vec<FieldError>,
}

#[derive(Serialize, ToSchema)]
pub struct LevelSchedule {
    pub level: CheckLevel,
    pub schedule: Option<ScheduleInfo>,
}

#[derive(Serialize, ToSchema)]
pub struct CheckSettingsCheck {
    pub schedules: Vec<LevelSchedule>,
    pub errors: Vec<FieldError>,
}

pub enum SettingsFailure {
    Api(ApiError),
    Fields(Vec<FieldError>),
}

impl<E: Into<ApiError>> From<E> for SettingsFailure {
    fn from(error: E) -> Self {
        SettingsFailure::Api(error.into())
    }
}

impl IntoResponse for SettingsFailure {
    fn into_response(self) -> Response {
        match self {
            SettingsFailure::Api(error) => error.into_response(),
            SettingsFailure::Fields(errors) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(CheckSettingsErrors { errors }),
            )
                .into_response(),
        }
    }
}

fn schedule_of(cron: &str, timezone: &str) -> Schedule {
    Schedule {
        cron: cron.trim().to_string(),
        timezone: Some(timezone.trim().to_string()),
        jitter: None,
        catch_up_window: None,
    }
}

fn size_error(path: &str) -> FieldError {
    FieldError {
        path: path.into(),
        message: "Use a size such as 5GiB or 500MiB.".into(),
    }
}

fn parse_limit(text: &str) -> Option<u64> {
    parse_size(text.trim()).filter(|bytes| *bytes >= MIN_DATA_BYTES)
}

/// Checks every field and reports all that are wrong, with the wording the
/// settings form shows next to them. Limits left out keep their saved value.
fn validate_settings(
    input: &CheckSettingsInput,
    saved: &[(CheckLevel, CheckLevelSettings)],
) -> Result<Vec<(CheckLevel, CheckLevelSettings)>, Vec<FieldError>> {
    let mut errors = Vec::new();
    let mut out = Vec::new();
    for (level, current) in saved {
        let name = level.as_str();
        let given = input.level(*level);
        let mut settings = current.clone();
        settings.enabled = given.enabled;
        settings.cron = given.cron.trim().to_string();
        settings.timezone = given.timezone.trim().to_string();

        let cron_check = Schedule {
            timezone: None,
            ..schedule_of(&settings.cron, "")
        };
        let zone_check = schedule_of(level.default_cron(), &settings.timezone);
        for (path, schedule) in [("cron", cron_check), ("timezone", zone_check)] {
            if let Err(problem) = schedule.compile() {
                errors.push(FieldError {
                    path: format!("{name}.schedule.{path}"),
                    message: problem.message,
                });
            }
        }

        match level {
            CheckLevel::Structure => {}
            CheckLevel::Packs => {
                if let Some(text) = &given.read_per_run {
                    match parse_limit(text) {
                        Some(bytes) => settings.read_per_run = bytes,
                        None => errors.push(size_error("packs.readPerRun")),
                    }
                }
            }
            CheckLevel::Restore => {
                if let Some(files) = given.files_per_run {
                    match u32::try_from(files)
                        .ok()
                        .filter(|files| (1..=MAX_FILES_PER_RUN).contains(files))
                    {
                        Some(files) => settings.files_per_run = files,
                        None => errors.push(FieldError {
                            path: "restore.filesPerRun".into(),
                            message: format!("Enter a whole number from 1 to {MAX_FILES_PER_RUN}."),
                        }),
                    }
                }
                if let Some(text) = &given.max_data_per_run {
                    match parse_limit(text) {
                        Some(bytes) => settings.max_data_per_run = bytes,
                        None => errors.push(size_error("restore.maxDataPerRun")),
                    }
                }
            }
        }
        out.push((*level, settings));
    }
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

#[utoipa::path(
    put,
    tag = "Checks",
    operation_id = "put_check_settings",
    summary = "Save the check settings of a repository",
    description = "Each level has its own schedule and limits. A limit left out keeps its saved \
        value. Saving moves the schedule on from the time of the change.",
    security(("session" = [], "csrf" = []), ("bearer" = ["policies:write"])),
    path = "/api/v1/repositories/{id}/checks/settings",
    params(("id" = String, Path, description = "Repository id")),
    request_body = CheckSettingsInput,
    responses(
        (status = 200, description = "Saved", body = CheckStatus),
        (status = 404, description = "Unknown repository", body = ErrorBody),
        (status = 422, description = "A field is not valid; nothing was saved", body = CheckSettingsErrors),
    )
)]
pub async fn put_settings(
    State(state): State<AppState>,
    _auth: Scoped<PoliciesWrite>,
    Path(id): Path<String>,
    Json(input): Json<CheckSettingsInput>,
) -> Result<Json<CheckStatus>, SettingsFailure> {
    repository(&state, &id).await?;
    let saved = db::load_settings(&state, &id).await?;
    let settings = validate_settings(&input, &saved).map_err(SettingsFailure::Fields)?;
    db::save_settings(&state, &id, &settings).await?;
    Ok(Json(build_status(&state, &id).await?))
}

#[utoipa::path(
    post,
    tag = "Checks",
    operation_id = "validate_check_settings",
    summary = "Check the settings without saving them",
    security(("session" = [], "csrf" = []), ("bearer" = ["jobs:read"])),
    path = "/api/v1/repositories/{id}/checks/settings/validate",
    params(("id" = String, Path, description = "Repository id")),
    request_body = CheckSettingsInput,
    responses(
        (status = 200, description = "The next runs of every level and the fields that are not valid", body = CheckSettingsCheck),
        (status = 404, description = "Unknown repository", body = ErrorBody),
    )
)]
pub async fn validate_settings_check(
    State(state): State<AppState>,
    _auth: Scoped<JobsRead>,
    Path(id): Path<String>,
    Json(input): Json<CheckSettingsInput>,
) -> Result<Json<CheckSettingsCheck>, ApiError> {
    repository(&state, &id).await?;
    let saved = db::load_settings(&state, &id).await?;
    let errors = validate_settings(&input, &saved).err().unwrap_or_default();
    let now = now_ms();
    let schedules = CheckLevel::ALL
        .into_iter()
        .map(|level| {
            let given = input.level(level);
            let broken = errors
                .iter()
                .any(|e| e.path.starts_with(&format!("{}.schedule", level.as_str())));
            LevelSchedule {
                level,
                schedule: (!broken)
                    .then(|| schedule_info_of(&schedule_of(&given.cron, &given.timezone), now))
                    .flatten(),
            }
        })
        .collect();
    Ok(Json(CheckSettingsCheck { schedules, errors }))
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(put_settings))
        .routes(routes!(validate_settings_check))
        .routes(routes!(get_status))
        .routes(routes!(start_run))
        .routes(routes!(cancel_run))
        .routes(routes!(get_run))
        .routes(routes!(get_report))
}
