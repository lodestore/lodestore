//! Integrity checks of a repository, run by the server with its own key.

pub mod api;
pub mod db;
pub mod restore;
pub mod run;
pub mod schedule;

use std::sync::Arc;
use std::time::Duration;

use lodestore_policy::jiff::Timestamp;
use serde::{Deserialize, Serialize, Serializer};
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;
use utoipa::ToSchema;

use crate::db::now_ms;
use crate::error::ApiError;
use crate::maintenance::gc::GcState;
use crate::maintenance::gc::db::{active_cycle_of, stopped_while_deleting};
use crate::repos::db::get_repository;
use crate::state::AppState;

pub const DEFAULT_CRON_STRUCTURE: &str = "0 2 * * *";
pub const DEFAULT_CRON_PACKS: &str = "0 4 * * 3";
pub const DEFAULT_CRON_RESTORE: &str = "0 5 2 * *";
pub const DEFAULT_TIMEZONE: &str = "UTC";
pub const DEFAULT_READ_PER_RUN: u64 = 5 << 30;
pub const DEFAULT_FILES_PER_RUN: u32 = 20;
pub const MAX_FILES_PER_RUN: u32 = 1000;
pub const DEFAULT_MAX_DATA_PER_RUN: u64 = 1 << 30;

pub const CLEANUP_RUNNING: &str = "A cleanup cycle is running. Checks wait for it.";
pub const CLEANUP_STOPPED: &str =
    "A cleanup stopped while deleting. Checks wait until it finishes.";
pub const CHECK_RUNNING: &str = "Another check is running on this repository.";

pub const MAX_PARALLEL_CHECKS: usize = 2;

/// What keeps checks and cleanup out of each other's way in this process.
#[derive(Clone)]
pub struct CheckGate {
    /// Held while a check is admitted and while a cleanup decides to delete,
    /// so that one sees the other's row.
    pub admit: Arc<Mutex<()>>,
    /// A check holds one while it runs; the others stay queued.
    pub slots: Arc<Semaphore>,
}

impl Default for CheckGate {
    fn default() -> Self {
        CheckGate {
            admit: Arc::new(Mutex::new(())),
            slots: Arc::new(Semaphore::new(MAX_PARALLEL_CHECKS)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckLevel {
    Structure,
    Packs,
    Restore,
}

impl CheckLevel {
    pub const ALL: [CheckLevel; 3] = [
        CheckLevel::Structure,
        CheckLevel::Packs,
        CheckLevel::Restore,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            CheckLevel::Structure => "structure",
            CheckLevel::Packs => "packs",
            CheckLevel::Restore => "restore",
        }
    }

    pub fn parse(text: &str) -> Option<CheckLevel> {
        match text {
            "structure" => Some(CheckLevel::Structure),
            "packs" => Some(CheckLevel::Packs),
            "restore" => Some(CheckLevel::Restore),
            _ => None,
        }
    }

    pub fn default_cron(self) -> &'static str {
        match self {
            CheckLevel::Structure => DEFAULT_CRON_STRUCTURE,
            CheckLevel::Packs => DEFAULT_CRON_PACKS,
            CheckLevel::Restore => DEFAULT_CRON_RESTORE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Queued,
    Running,
    Done,
    Failed,
    Aborted,
}

impl CheckState {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckState::Queued => "queued",
            CheckState::Running => "running",
            CheckState::Done => "done",
            CheckState::Failed => "failed",
            CheckState::Aborted => "aborted",
        }
    }

    pub fn parse(text: &str) -> Option<CheckState> {
        Some(match text {
            "queued" => CheckState::Queued,
            "running" => CheckState::Running,
            "done" => CheckState::Done,
            "failed" => CheckState::Failed,
            "aborted" => CheckState::Aborted,
            _ => return None,
        })
    }

    pub fn is_active(self) -> bool {
        matches!(self, CheckState::Queued | CheckState::Running)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckOutcome {
    Passed,
    Problems,
    Stopped,
}

impl CheckOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckOutcome::Passed => "passed",
            CheckOutcome::Problems => "problems",
            CheckOutcome::Stopped => "stopped",
        }
    }

    pub fn parse(text: &str) -> Option<CheckOutcome> {
        match text {
            "passed" => Some(CheckOutcome::Passed),
            "problems" => Some(CheckOutcome::Problems),
            "stopped" => Some(CheckOutcome::Stopped),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckOrigin {
    Manual,
    Schedule,
}

impl CheckOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckOrigin::Manual => "manual",
            CheckOrigin::Schedule => "schedule",
        }
    }

    pub fn parse(text: &str) -> Option<CheckOrigin> {
        match text {
            "manual" => Some(CheckOrigin::Manual),
            "schedule" => Some(CheckOrigin::Schedule),
            _ => None,
        }
    }
}

fn size_text<S: Serializer>(bytes: &u64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&lodestore_policy::format_size(*bytes))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct CheckLevelSettings {
    pub enabled: bool,
    pub cron: String,
    pub timezone: String,
    /// A size such as `5GiB`: how much of the bucket a pack reading run downloads.
    #[serde(serialize_with = "size_text")]
    #[schema(value_type = String)]
    pub read_per_run: u64,
    /// How many files a test restore restores, from 1 to 1000.
    pub files_per_run: u32,
    /// A size such as `1GiB`: the most data a test restore writes to the server's disk.
    #[serde(serialize_with = "size_text")]
    #[schema(value_type = String)]
    pub max_data_per_run: u64,
}

impl CheckLevelSettings {
    pub fn defaults(level: CheckLevel) -> CheckLevelSettings {
        CheckLevelSettings {
            enabled: true,
            cron: level.default_cron().to_string(),
            timezone: DEFAULT_TIMEZONE.to_string(),
            read_per_run: DEFAULT_READ_PER_RUN,
            files_per_run: DEFAULT_FILES_PER_RUN,
            max_data_per_run: DEFAULT_MAX_DATA_PER_RUN,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChecksState {
    Passed,
    Failed,
    NeverRun,
    /// Every level is turned off.
    Off,
}

/// The worst result of the enabled levels, as the repository list shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub struct ChecksSummary {
    pub state: ChecksState,
    /// When the run that decided `state` finished.
    pub finished_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BlockReason {
    Cleanup,
    Check,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct CheckBlocked {
    pub reason: BlockReason,
    pub message: String,
}

/// Why a check can't start in the repository now. A cleanup that is
/// deleting or repacking may remove a pack the check is about to read; one
/// that only waits does not.
pub async fn blocked(
    state: &AppState,
    repository_id: &str,
) -> anyhow::Result<Option<CheckBlocked>> {
    if active_cycle_of(state, repository_id, false)
        .await?
        .is_some_and(|cycle| matches!(cycle.state, GcState::Sweeping | GcState::Repacking))
    {
        return Ok(Some(CheckBlocked {
            reason: BlockReason::Cleanup,
            message: CLEANUP_RUNNING.into(),
        }));
    }
    if stopped_while_deleting(state)
        .await?
        .iter()
        .any(|cycle| cycle.repository_id == repository_id)
    {
        return Ok(Some(CheckBlocked {
            reason: BlockReason::Cleanup,
            message: CLEANUP_STOPPED.into(),
        }));
    }
    if db::active_run(state, repository_id).await?.is_some() {
        return Ok(Some(CheckBlocked {
            reason: BlockReason::Check,
            message: CHECK_RUNNING.into(),
        }));
    }
    Ok(None)
}

/// Queues a check and starts it in the background.
pub async fn start_check(
    state: &AppState,
    repository_id: &str,
    level: CheckLevel,
    origin: CheckOrigin,
    actor: &str,
) -> Result<String, ApiError> {
    get_repository(state, repository_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("repository not found".into()))?;
    let _admit = state.checks.admit.lock().await;
    if let Some(block) = blocked(state, repository_id).await? {
        return Err(ApiError::Conflict(block.message));
    }
    let id = uuid::Uuid::now_v7().to_string();
    db::insert_run(state, &id, repository_id, level, origin, actor).await?;
    tokio::spawn({
        let state = state.clone();
        let id = id.clone();
        async move { run::run_check(&state, &id).await }
    });
    Ok(id)
}

/// Starts the checks whose cron slot has come.
pub async fn tick_once(state: &AppState, now_ms: i64) -> anyhow::Result<()> {
    let now = Timestamp::from_millisecond(now_ms)?;
    schedule::tick(state, now).await?;
    Ok(())
}

pub async fn run(state: AppState, stop: CancellationToken) -> anyhow::Result<()> {
    let period = Duration::from_secs(state.config.schedule_tick_secs.max(1));
    let mut interval = tokio::time::interval(period);
    loop {
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            _ = interval.tick() => {}
        }
        if let Err(error) = tick_once(&state, now_ms()).await {
            tracing::warn!(%error, "check tick failed");
        }
    }
}

pub const RESTART_MESSAGE: &str = "The server restarted during the check.";

/// A check lives in this process only, so one left unfinished at start-up
/// was cut short by the restart; so was the test restore it may have left on
/// the disk.
pub async fn interrupt_running(state: &AppState) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE check_runs SET state = 'failed', outcome = 'stopped', error = $1, \
         finished_at = $2 WHERE state IN ('queued', 'running')",
    )
    .bind(RESTART_MESSAGE)
    .bind(now_ms())
    .execute(&state.db.pool)
    .await?;
    if let Err(error) = std::fs::remove_dir_all(state.config.data_dir.join("check-restore"))
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%error, "removing the leftover test restore failed");
    }
    Ok(())
}
