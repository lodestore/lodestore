use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use std::time::Duration;

use lodestore_format::index::parse_hex32;
use lodestore_policy::{Schedule, format_duration, format_size, parse_duration};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::cycle::{event, take_down_marker, unlock_writers};
use super::db::{self, CycleRow, Excluded};
use super::preflight::{Preflight, PreflightProblem, run_preflight};
use super::{
    GcOrigin, GcSettings, GcState, pause_floor, run_dry_run, start_cycle, start_real_cycle,
    validate_settings,
};
use crate::auth::tokens::{JobsRead, JobsRun, Scoped, SnapshotsDelete};
use crate::db::now_ms;
use crate::error::{ApiError, ErrorBody};
use crate::policies::{ScheduleInfo, schedule_info_of};
use crate::repos::CheckDto;
use crate::repos::db::{RepoFull, find_repository};
use crate::state::AppState;

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct GcSettingsDto {
    pub cron: String,
    pub timezone: String,
    /// A duration such as `24h` or `2d`.
    pub pause: String,
    pub repack_below_pct: u8,
    /// A size such as `10GiB`.
    pub max_repack: String,
}

impl From<&GcSettings> for GcSettingsDto {
    fn from(settings: &GcSettings) -> Self {
        GcSettingsDto {
            cron: settings.cron.clone(),
            timezone: settings.timezone.clone(),
            pause: format_duration(settings.pause),
            repack_below_pct: settings.repack_below_pct,
            max_repack: format_size(settings.max_repack_bytes),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GcProgress {
    pub done: u64,
    pub total: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct WriterMarkerDto {
    pub key: String,
    /// `None` when the marker can't be read.
    pub host: Option<String>,
    /// When the bucket stored the marker; `None` when it doesn't say.
    pub started_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct GcUnlocked {
    pub unlocked: u64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct GcCycleDto {
    pub id: String,
    pub state: GcState,
    pub dry_run: bool,
    pub origin: GcOrigin,
    pub started_at: Option<i64>,
    pub delete_after: Option<i64>,
    pub progress: Option<GcProgress>,
    pub marked_packs: u64,
    pub marked_bytes: u64,
    pub blocked_by_writers: Vec<WriterMarkerDto>,
    pub waiting_for_restores: bool,
    pub error: Option<String>,
    pub error_after_delete: bool,
}

impl From<&CycleRow> for GcCycleDto {
    fn from(cycle: &CycleRow) -> Self {
        let p = &cycle.progress;
        GcCycleDto {
            id: cycle.id.clone(),
            state: cycle.state,
            dry_run: cycle.dry_run,
            origin: cycle.origin,
            started_at: cycle.started_at,
            delete_after: cycle.delete_after,
            progress: p.shown(),
            marked_packs: p.marked_packs,
            marked_bytes: p.marked_bytes,
            blocked_by_writers: p.blocked_by_writers.clone(),
            waiting_for_restores: p.waiting_for_restores,
            error: cycle.error.clone(),
            error_after_delete: cycle.error_after_delete,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GcReport {
    pub cycle_id: String,
    pub dry_run: bool,
    pub finished_at: i64,
    pub duration_ms: u64,
    pub bytes_freed: u64,
    pub packs_deleted: u64,
    pub packs_repacked: u64,
    pub repack_transfer_bytes: u64,
    pub locked_bytes: u64,
    pub locked_until: Option<i64>,
    pub locked_until_unknown: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GcPackItem {
    pub id: String,
    pub size: u64,
    pub live_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GcObjectNote {
    pub key: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GcLockedItem {
    pub key: String,
    pub size: u64,
    pub until: Option<i64>,
    #[serde(default)]
    pub modified_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GcFullReport {
    #[serde(flatten)]
    pub summary: GcReport,
    pub delete: Vec<GcPackItem>,
    pub repack: Vec<GcPackItem>,
    pub ignored: Vec<GcObjectNote>,
    pub unindexed: Vec<GcObjectNote>,
    pub locked: Vec<GcLockedItem>,
    /// Index shards the cleanup could not use and left alone.
    #[serde(default)]
    pub skipped_shards: Vec<GcObjectNote>,
    /// Snapshots the cleanup could not read.
    #[serde(default)]
    pub quarantined: Vec<GcObjectNote>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct GcQuarantineDto {
    /// The snapshot id, hex.
    pub snapshot_id: String,
    /// The snapshot object in the repository.
    pub key: String,
    /// Why the cleanup cannot read it.
    pub reason: String,
    /// False for a snapshot a backup job points at; it cannot be excluded.
    pub confirmable: bool,
    pub detected_at: i64,
    /// When an administrator excluded it from cleanup.
    pub confirmed_at: Option<i64>,
    pub confirmed_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct GcEventDto {
    pub seq: i64,
    pub at: i64,
    pub level: String,
    pub kind: String,
    pub message: String,
}

#[derive(Serialize, ToSchema)]
pub struct GcStatus {
    pub repository_id: String,
    pub enabled: bool,
    pub settings: GcSettingsDto,
    pub pause_floor: String,
    /// The pause the next cycle uses: the setting, raised to `pause_floor`.
    pub pause_effective: String,
    pub schedule: Option<ScheduleInfo>,
    pub preflight: Preflight,
    pub has_object_lock: bool,
    /// The unfinished cycle: the real one when there is one, otherwise a dry
    /// run. `events` belong to it.
    pub cycle: Option<GcCycleDto>,
    /// A dry run in progress, also while a real cycle runs or waits.
    pub active_dry_run: Option<GcCycleDto>,
    pub last_cleanup: Option<GcReport>,
    pub last_dry_run: Option<GcReport>,
    pub last_aborted_at: Option<i64>,
    /// The newest cleanup that was not a dry run, when it failed. `None`
    /// once a later one is running or has finished.
    pub last_failure: Option<GcFailureDto>,
    pub events: Vec<GcEventDto>,
    pub events_total: u64,
    /// Snapshots the cleanup cannot read; it stops until each confirmable one is excluded.
    pub quarantine: Vec<GcQuarantineDto>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct GcFailureDto {
    pub cycle_id: String,
    pub origin: GcOrigin,
    pub error: String,
    /// Deleting had started: some marked packs may be gone already. The
    /// cycle keeps its marker and continues where it stopped, on the next
    /// run or when the server retries it.
    pub after_delete: bool,
    pub failed_at: Option<i64>,
}

/// Where cleanup of a repository stands, read from the catalog only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CleanupState {
    Off,
    Idle,
    Running,
    Waiting,
    Postponed,
    Stopped,
    /// The catalog could not be read; the list still loads.
    Unknown,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CleanupSummary {
    pub state: CleanupState,
    /// `waiting`: when deletion may start. `idle`: the next scheduled run.
    pub at: Option<i64>,
}

impl CleanupSummary {
    pub const OFF: CleanupSummary = CleanupSummary {
        state: CleanupState::Off,
        at: None,
    };
}

/// [`cleanup_summary`] for lists: a broken cleanup row must not take the
/// repository pages down, since those are where it gets fixed.
pub async fn cleanup_summary_or_unknown(state: &AppState, repository_id: &str) -> CleanupSummary {
    cleanup_summary(state, repository_id)
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(repository_id, %error, "reading the cleanup state failed");
            CleanupSummary {
                state: CleanupState::Unknown,
                at: None,
            }
        })
}

/// The cleanup state for repository lists. Unlike [`build_status`] it never
/// touches the storage.
pub async fn cleanup_summary(
    state: &AppState,
    repository_id: &str,
) -> anyhow::Result<CleanupSummary> {
    if let Some(cycle) = db::active_cycle_of(state, repository_id, false).await? {
        let (shown, at) = match cycle.state {
            GcState::Waiting if !cycle.progress.blocked_by_writers.is_empty() => {
                (CleanupState::Postponed, None)
            }
            GcState::Waiting => (CleanupState::Waiting, cycle.delete_after),
            _ => (CleanupState::Running, None),
        };
        return Ok(CleanupSummary { state: shown, at });
    }
    if db::last_real_cycle(state, repository_id)
        .await?
        .is_some_and(|c| c.state == GcState::Failed)
    {
        return Ok(CleanupSummary {
            state: CleanupState::Stopped,
            at: None,
        });
    }
    let settings = db::load_settings(state, repository_id).await?;
    if !settings.enabled {
        return Ok(CleanupSummary::OFF);
    }
    let next = schedule_info_of(
        &Schedule {
            cron: settings.cron.clone(),
            timezone: Some(settings.timezone.clone()),
            jitter: None,
            catch_up_window: None,
        },
        now_ms(),
    )
    .filter(|_| !settings.cron.trim().is_empty())
    .and_then(|info| info.next_runs.first().copied());
    Ok(CleanupSummary {
        state: CleanupState::Idle,
        at: next,
    })
}

impl GcFailureDto {
    fn of(cycle: &CycleRow) -> Option<GcFailureDto> {
        (cycle.state == GcState::Failed).then(|| GcFailureDto {
            cycle_id: cycle.id.clone(),
            origin: cycle.origin,
            error: cycle.error.clone().unwrap_or_default(),
            after_delete: cycle.error_after_delete || cycle.sweep_started,
            failed_at: cycle.finished_at,
        })
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct GcStarted {
    pub cycle_id: String,
}

fn has_object_lock(repo: &RepoFull) -> bool {
    repo.checks_json
        .as_deref()
        .and_then(|json| serde_json::from_str::<Vec<CheckDto>>(json).ok())
        .is_some_and(|checks| {
            checks
                .iter()
                .any(|c| c.id == "object_lock" && c.status == "pass")
        })
}

pub(crate) async fn repository(state: &AppState, id: &str) -> Result<RepoFull, ApiError> {
    find_repository(state, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("repository not found".into()))
}

pub(crate) async fn build_status(state: &AppState, repo: &RepoFull) -> Result<GcStatus, ApiError> {
    let settings = db::load_settings(state, &repo.id).await?;
    let floor = pause_floor(state, &repo.id).await?;
    let schedule = schedule_info_of(
        &Schedule {
            cron: settings.cron.clone(),
            timezone: Some(settings.timezone.clone()),
            jitter: None,
            catch_up_window: None,
        },
        now_ms(),
    );
    let cycle = db::active_cycle(state, &repo.id).await?;
    let (events, events_total) =
        db::latest_events(state, &repo.id, cycle.as_ref().map(|c| c.id.as_str())).await?;
    Ok(GcStatus {
        repository_id: repo.id.clone(),
        enabled: settings.enabled,
        settings: GcSettingsDto::from(&settings),
        pause_floor: format_duration(floor),
        pause_effective: format_duration(settings.pause.max(floor)),
        schedule,
        preflight: run_preflight(state, &repo.id, false).await?,
        has_object_lock: has_object_lock(repo),
        cycle: cycle.as_ref().map(GcCycleDto::from),
        active_dry_run: db::active_cycle_of(state, &repo.id, true)
            .await?
            .as_ref()
            .map(GcCycleDto::from),
        last_cleanup: db::last_report(state, &repo.id, false).await?,
        last_dry_run: db::last_report(state, &repo.id, true).await?,
        last_aborted_at: db::last_aborted_at(state, &repo.id).await?,
        last_failure: db::last_real_cycle(state, &repo.id)
            .await?
            .as_ref()
            .and_then(GcFailureDto::of),
        events,
        events_total,
        quarantine: db::list_quarantine(state, &repo.id).await?,
    })
}

#[utoipa::path(
    get,
    tag = "Cleanup",
    summary = "Get the cleanup status of a repository",
    security(("session" = []), ("bearer" = ["jobs:read"])),
    path = "/api/v1/repositories/{id}/gc",
    params(("id" = String, Path, description = "Repository id")),
    responses(
        (status = 200, description = "Settings, safety checks, the running cycle and the last reports", body = GcStatus),
        (status = 404, description = "Unknown repository", body = ErrorBody),
    )
)]
pub async fn get_status(
    State(state): State<AppState>,
    _auth: Scoped<JobsRead>,
    Path(id): Path<String>,
) -> Result<Json<GcStatus>, ApiError> {
    let repo = repository(&state, &id).await?;
    Ok(Json(build_status(&state, &repo).await?))
}

#[utoipa::path(
    post,
    tag = "Cleanup",
    summary = "Start a dry run",
    description = "Reports how much space a cleanup would free without changing the repository. \
        Works while cleanup is turned off.",
    security(("session" = [], "csrf" = []), ("bearer" = ["jobs:run"])),
    path = "/api/v1/repositories/{id}/gc/dry-run",
    params(("id" = String, Path, description = "Repository id")),
    responses(
        (status = 202, description = "The dry run started; follow it in the cleanup status", body = GcStarted),
        (status = 404, description = "Unknown repository", body = ErrorBody),
        (status = 409, description = "A cleanup cycle is already running in this repository", body = ErrorBody),
    )
)]
pub async fn start_dry_run(
    State(state): State<AppState>,
    auth: Scoped<JobsRun>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<GcStarted>), ApiError> {
    let cycle_id = start_cycle(&state, &id, true, GcOrigin::Manual, &auth.actor()).await?;
    tokio::spawn({
        let state = state.clone();
        let cycle_id = cycle_id.clone();
        async move { run_dry_run(&state, &cycle_id).await }
    });
    Ok((StatusCode::ACCEPTED, Json(GcStarted { cycle_id })))
}

#[utoipa::path(
    post,
    tag = "Cleanup",
    summary = "Start a cleanup now",
    description = "Marks packs nothing needs, waits out the pause and deletes them. A cleanup that \
        stopped while deleting is continued instead of starting a new one.",
    security(("session" = [], "csrf" = []), ("bearer" = ["snapshots:delete"])),
    path = "/api/v1/repositories/{id}/gc/run",
    params(("id" = String, Path, description = "Repository id")),
    responses(
        (status = 202, description = "The cleanup started; follow it in the cleanup status", body = GcStarted),
        (status = 404, description = "Unknown repository", body = ErrorBody),
        (status = 409, description = "Cleanup is off, a cycle is running, or a safety check failed", body = GcPreflightError),
    )
)]
pub async fn start_run(
    State(state): State<AppState>,
    auth: Scoped<SnapshotsDelete>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<GcStarted>), GcFailure> {
    let cycle_id = start_real_cycle(&state, &id, GcOrigin::Manual, &auth.actor()).await?;
    Ok((StatusCode::ACCEPTED, Json(GcStarted { cycle_id })))
}

#[utoipa::path(
    post,
    tag = "Cleanup",
    summary = "Abort the running cleanup",
    description = "Works until the first object is deleted. The cleanup marker is removed and \
        nothing is lost.",
    security(("session" = [], "csrf" = []), ("bearer" = ["jobs:run"])),
    path = "/api/v1/repositories/{id}/gc/abort",
    params(("id" = String, Path, description = "Repository id")),
    responses(
        (status = 200, description = "Aborted", body = GcStatus),
        (status = 404, description = "Unknown repository", body = ErrorBody),
        (status = 409, description = "Nothing is running, or deleting has started", body = ErrorBody),
    )
)]
pub async fn abort(
    State(state): State<AppState>,
    auth: Scoped<JobsRun>,
    Path(id): Path<String>,
) -> Result<Json<GcStatus>, ApiError> {
    let repo = repository(&state, &id).await?;
    let cycle = db::active_cycle(&state, &id)
        .await?
        .ok_or_else(|| ApiError::Conflict("There is no cleanup to abort.".into()))?;
    if !db::abort_cycle(&state, &cycle.id).await? {
        return Err(ApiError::Conflict("This step can't be aborted.".into()));
    }
    let message = format!("Aborted by {}.", auth.actor());
    event(&state, &cycle.id, "warning", "aborted", &message).await;
    if let Some(_driving) = state.gc.claim(&cycle.id) {
        take_down_marker(&state, &cycle.id).await;
    }
    Ok(Json(build_status(&state, &repo).await?))
}

#[utoipa::path(
    post,
    tag = "Cleanup",
    summary = "Close the backup markers that hold deletion back",
    description = "A backup that started before the cleanup marked its packs and never finished \
        keeps deletion waiting. This closes those markers so the cleanup can go on. Do it only \
        when the backup is known to be dead.",
    security(("session" = [], "csrf" = []), ("bearer" = ["snapshots:delete"])),
    path = "/api/v1/repositories/{id}/gc/unlock",
    params(("id" = String, Path, description = "Repository id")),
    responses(
        (status = 200, description = "The markers were closed", body = GcUnlocked),
        (status = 404, description = "Unknown repository", body = ErrorBody),
        (status = 409, description = "No cleanup is waiting on markers", body = ErrorBody),
    )
)]
pub async fn unlock(
    State(state): State<AppState>,
    auth: Scoped<SnapshotsDelete>,
    Path(id): Path<String>,
) -> Result<Json<GcUnlocked>, ApiError> {
    repository(&state, &id).await?;
    let none = || ApiError::Conflict("There are no markers to unlock.".into());
    let cycle = db::active_cycle_of(&state, &id, false)
        .await?
        .ok_or_else(none)?;
    if cycle.state != GcState::Waiting {
        return Err(none());
    }
    let unlocked = unlock_writers(&state, &cycle).await?;
    if unlocked == 0 {
        return Err(none());
    }
    let message = format!(
        "Unlocked {unlocked} {} by {}.",
        if unlocked == 1 { "marker" } else { "markers" },
        auth.actor()
    );
    event(&state, &cycle.id, "warning", "unlocked", &message).await;
    tokio::spawn(super::cycle::drive(state.clone(), cycle.id.clone()));
    Ok(Json(GcUnlocked {
        unlocked: unlocked as u64,
    }))
}

#[utoipa::path(
    post,
    tag = "Cleanup",
    summary = "Exclude a quarantined snapshot from cleanup",
    description = "The cleanup stops while a snapshot it cannot read is in quarantine. Excluding \
        it lets the next cleanup go on and may delete the data only that snapshot used. The \
        snapshot object stays in storage; delete it separately.",
    security(("session" = [], "csrf" = []), ("bearer" = ["snapshots:delete"])),
    path = "/api/v1/repositories/{id}/gc/quarantine/{snapshot_id}/exclude",
    params(
        ("id" = String, Path, description = "Repository id"),
        ("snapshot_id" = String, Path, description = "Snapshot id, hex"),
    ),
    responses(
        (status = 200, description = "The snapshot is excluded", body = GcStatus),
        (status = 404, description = "Unknown repository, or no such snapshot in quarantine", body = ErrorBody),
        (status = 409, description = "The snapshot belongs to a backup job, or is already excluded", body = ErrorBody),
    )
)]
pub async fn exclude(
    State(state): State<AppState>,
    auth: Scoped<SnapshotsDelete>,
    Path((id, snapshot_id)): Path<(String, String)>,
) -> Result<Json<GcStatus>, ApiError> {
    let repo = repository(&state, &id).await?;
    let missing = || ApiError::NotFound("There is no such snapshot in quarantine.".into());
    let snapshot = parse_hex32(&snapshot_id).ok_or_else(missing)?;
    let actor = auth.actor();
    match db::exclude_from_gc(&state, &id, &snapshot, &actor, now_ms()).await? {
        Excluded::Done => {}
        Excluded::Missing => return Err(missing()),
        Excluded::Bound => {
            return Err(ApiError::Conflict(
                "This snapshot belongs to a backup job; repair the repository or delete the snapshot."
                    .into(),
            ));
        }
        Excluded::Already => {
            return Err(ApiError::Conflict(
                "This snapshot is already excluded.".into(),
            ));
        }
    }
    tracing::info!(repository = %id, snapshot = %snapshot_id, %actor, "snapshot excluded from cleanup");
    Ok(Json(build_status(&state, &repo).await?))
}

#[utoipa::path(
    get,
    tag = "Cleanup",
    summary = "Get the full report of a finished cycle",
    description = "Lists every pack the cycle deletes or repacks.",
    security(("session" = []), ("bearer" = ["jobs:read"])),
    path = "/api/v1/repositories/{id}/gc/cycles/{cycle_id}/report",
    params(
        ("id" = String, Path, description = "Repository id"),
        ("cycle_id" = String, Path, description = "Cycle id"),
    ),
    responses(
        (status = 200, description = "The report", body = GcFullReport),
        (status = 404, description = "Unknown cycle, or one without a report", body = ErrorBody),
    )
)]
pub async fn get_report(
    State(state): State<AppState>,
    _auth: Scoped<JobsRead>,
    Path((id, cycle_id)): Path<(String, String)>,
) -> Result<Json<GcFullReport>, ApiError> {
    let row = sqlx::query(
        "SELECT report_json FROM gc_cycles WHERE id = $1 AND repository_id = $2 \
         AND report_json IS NOT NULL",
    )
    .bind(&cycle_id)
    .bind(&id)
    .fetch_optional(&state.db.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("report not found".into()))?;
    let report = serde_json::from_str(&row.try_get::<String, _>("report_json")?)
        .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(Json(report))
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FieldError {
    /// The policy-style path of the field, such as `spec.pause`.
    pub path: String,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct GcSettingsInput {
    pub enabled: bool,
    pub cron: String,
    pub timezone: String,
    /// A duration such as `24h` or `2d`, not below the pause floor.
    pub pause: String,
    /// A whole number from 1 to 99.
    pub repack_below_pct: i64,
    /// A size such as `10GiB`.
    pub max_repack: String,
}

#[derive(Serialize, ToSchema)]
pub struct GcSettingsErrors {
    pub errors: Vec<FieldError>,
}

#[derive(Serialize, ToSchema)]
pub struct GcPreflightError {
    pub message: String,
    pub problems: Vec<PreflightProblem>,
}

#[derive(Serialize, ToSchema)]
pub struct GcSettingsCheck {
    pub schedule: Option<ScheduleInfo>,
    pub problems: Vec<FieldError>,
    pub pause_floor: String,
}

pub enum GcFailure {
    Api(ApiError),
    Fields(Vec<FieldError>),
    Preflight(Vec<PreflightProblem>),
}

impl<E: Into<ApiError>> From<E> for GcFailure {
    fn from(error: E) -> Self {
        GcFailure::Api(error.into())
    }
}

impl IntoResponse for GcFailure {
    fn into_response(self) -> Response {
        match self {
            GcFailure::Api(error) => error.into_response(),
            GcFailure::Fields(errors) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(GcSettingsErrors { errors }),
            )
                .into_response(),
            GcFailure::Preflight(problems) => (
                StatusCode::CONFLICT,
                Json(GcPreflightError {
                    message: "Cleanup can't run yet.".into(),
                    problems,
                }),
            )
                .into_response(),
        }
    }
}

#[utoipa::path(
    put,
    tag = "Cleanup",
    summary = "Save the cleanup settings of a repository",
    description = "Turning cleanup on is refused while the safety checks report a problem. \
        Turning it off with the pause unchanged always works, even when the pause floor has \
        risen above the saved pause since.",
    security(("session" = [], "csrf" = []), ("bearer" = ["snapshots:delete"])),
    path = "/api/v1/repositories/{id}/gc/settings",
    params(("id" = String, Path, description = "Repository id")),
    request_body = GcSettingsInput,
    responses(
        (status = 200, description = "Saved", body = GcStatus),
        (status = 404, description = "Unknown repository", body = ErrorBody),
        (status = 409, description = "Cleanup can't be turned on yet; nothing was saved", body = GcPreflightError),
        (status = 422, description = "A field is not valid", body = GcSettingsErrors),
    )
)]
pub async fn put_settings(
    State(state): State<AppState>,
    _auth: Scoped<SnapshotsDelete>,
    Path(id): Path<String>,
    Json(input): Json<GcSettingsInput>,
) -> Result<Json<GcStatus>, GcFailure> {
    let repo = repository(&state, &id).await?;
    let saved = db::load_settings(&state, &id).await?;
    let floor = if !input.enabled && parse_duration(input.pause.trim()) == Some(saved.pause) {
        Duration::ZERO
    } else {
        pause_floor(&state, &id).await?
    };
    let settings = validate_settings(&input, floor).map_err(GcFailure::Fields)?;
    if settings.enabled {
        let preflight = run_preflight(&state, &id, true).await?;
        if !preflight.problems.is_empty() {
            return Err(GcFailure::Preflight(preflight.problems));
        }
    }
    db::save_settings(&state, &id, &settings).await?;
    Ok(Json(build_status(&state, &repo).await?))
}

#[utoipa::path(
    post,
    tag = "Cleanup",
    summary = "Check cleanup settings without saving them",
    security(("session" = [], "csrf" = []), ("bearer" = ["jobs:read"])),
    path = "/api/v1/repositories/{id}/gc/settings/validate",
    params(("id" = String, Path, description = "Repository id")),
    request_body = GcSettingsInput,
    responses(
        (status = 200, description = "The next runs of the schedule and the fields that are not valid", body = GcSettingsCheck),
        (status = 404, description = "Unknown repository", body = ErrorBody),
    )
)]
pub async fn validate(
    State(state): State<AppState>,
    _auth: Scoped<JobsRead>,
    Path(id): Path<String>,
    Json(input): Json<GcSettingsInput>,
) -> Result<Json<GcSettingsCheck>, ApiError> {
    repository(&state, &id).await?;
    let floor = pause_floor(&state, &id).await?;
    let problems = validate_settings(&input, floor).err().unwrap_or_default();
    let schedule_ok = problems
        .iter()
        .all(|p| !p.path.starts_with("spec.schedule"));
    let schedule = schedule_ok
        .then(|| {
            schedule_info_of(
                &Schedule {
                    cron: input.cron.trim().to_string(),
                    timezone: Some(input.timezone.trim().to_string()),
                    jitter: None,
                    catch_up_window: None,
                },
                now_ms(),
            )
        })
        .flatten();
    Ok(Json(GcSettingsCheck {
        schedule,
        problems,
        pause_floor: format_duration(floor),
    }))
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_status))
        .routes(routes!(put_settings))
        .routes(routes!(validate))
        .routes(routes!(start_dry_run))
        .routes(routes!(start_run))
        .routes(routes!(abort))
        .routes(routes!(unlock))
        .routes(routes!(exclude))
        .routes(routes!(get_report))
}
