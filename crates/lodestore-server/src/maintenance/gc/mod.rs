//! Freeing space in a repository: settings, cycles and the dry run.

pub mod api;
pub mod cycle;
pub mod db;
pub mod preflight;
pub mod schedule;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use lodestore_format::gc::plan::{MAX_CANDIDATES, PackStat, PlanOptions};
use lodestore_format::gc::{Survey, SurveyProgress, survey};
use lodestore_format::index::hex32;
use lodestore_format::locks::{DEFAULT_MAX_RUNTIME, RUNTIME_MARGIN};
use lodestore_policy::jiff::Timestamp;
use lodestore_policy::{
    BackupPolicy, MAX_RUNTIME as POLICY_MAX_RUNTIME, Schedule, format_duration, parse_duration,
    parse_size,
};
use lodestore_proto::agent::v1::JobSpec;
use prost::Message;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use utoipa::ToSchema;

use self::api::{
    FieldError, GcFailure, GcFullReport, GcObjectNote, GcPackItem, GcProgress, GcReport,
    GcSettingsInput, WriterMarkerDto,
};
use self::db::CycleRow;
use crate::catalog::open_repository;
use crate::db::now_ms;
use crate::error::ApiError;
use crate::jobs::max_runtime_of;
use crate::repos::db::{get_repository, server_access};
use crate::repos::object_lock::default_retention;
use crate::state::AppState;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_CRON: &str = "0 3 * * 0";
pub const DEFAULT_TIMEZONE: &str = "UTC";
pub const MIN_PAUSE: Duration = Duration::from_hours(24);
pub const DEFAULT_REPACK_BELOW_PCT: u8 = 50;
pub const DEFAULT_MAX_REPACK_BYTES: u64 = 10 << 30;
pub const MIN_MAX_REPACK_BYTES: u64 = 1 << 20;

const PROGRESS_PERIOD: Duration = Duration::from_secs(1);
const RESTART_MESSAGE: &str = "The server restarted during the dry run.";
const RESUME_FIRST_BACKOFF: Duration = Duration::from_mins(5);
const RESUME_MAX_BACKOFF: Duration = Duration::from_hours(24);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GcState {
    Marking,
    Repacking,
    Waiting,
    Verifying,
    Sweeping,
    Done,
    Failed,
    Aborted,
}

impl GcState {
    pub fn as_str(self) -> &'static str {
        match self {
            GcState::Marking => "marking",
            GcState::Repacking => "repacking",
            GcState::Waiting => "waiting",
            GcState::Verifying => "verifying",
            GcState::Sweeping => "sweeping",
            GcState::Done => "done",
            GcState::Failed => "failed",
            GcState::Aborted => "aborted",
        }
    }

    pub fn parse(text: &str) -> Option<GcState> {
        Some(match text {
            "marking" => GcState::Marking,
            "repacking" => GcState::Repacking,
            "waiting" => GcState::Waiting,
            "verifying" => GcState::Verifying,
            "sweeping" => GcState::Sweeping,
            "done" => GcState::Done,
            "failed" => GcState::Failed,
            "aborted" => GcState::Aborted,
            _ => return None,
        })
    }

    pub fn is_active(self) -> bool {
        !matches!(self, GcState::Done | GcState::Failed | GcState::Aborted)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GcOrigin {
    Manual,
    Schedule,
}

impl GcOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            GcOrigin::Manual => "manual",
            GcOrigin::Schedule => "schedule",
        }
    }

    pub fn parse(text: &str) -> Option<GcOrigin> {
        match text {
            "manual" => Some(GcOrigin::Manual),
            "schedule" => Some(GcOrigin::Schedule),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcSettings {
    pub enabled: bool,
    pub cron: String,
    pub timezone: String,
    pub pause: Duration,
    pub repack_below_pct: u8,
    pub max_repack_bytes: u64,
}

impl Default for GcSettings {
    fn default() -> Self {
        GcSettings {
            enabled: false,
            cron: DEFAULT_CRON.to_string(),
            timezone: DEFAULT_TIMEZONE.to_string(),
            pause: MIN_PAUSE,
            repack_below_pct: DEFAULT_REPACK_BELOW_PCT,
            max_repack_bytes: DEFAULT_MAX_REPACK_BYTES,
        }
    }
}

/// The settings a cycle runs with, frozen when it starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveSettings {
    pub pause_ms: i64,
    pub repack_below_pct: u8,
    pub max_repack_bytes: u64,
}

impl EffectiveSettings {
    pub fn freeze(settings: &GcSettings, floor: Duration) -> EffectiveSettings {
        EffectiveSettings {
            pause_ms: i64::try_from(settings.pause.max(floor).as_millis()).unwrap_or(i64::MAX),
            repack_below_pct: settings.repack_below_pct,
            max_repack_bytes: settings.max_repack_bytes,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StoredProgress {
    pub done: u64,
    pub total: u64,
    pub bytes: u64,
    pub marked_packs: u64,
    pub marked_bytes: u64,
    pub blocked_by_writers: Vec<WriterMarkerDto>,
    pub waiting_for_restores: bool,
    pub repack_read: u64,
    pub repack_written: u64,
}

impl StoredProgress {
    pub fn shown(&self) -> Option<GcProgress> {
        (self.total > 0 || self.done > 0).then_some(GcProgress {
            done: self.done,
            total: self.total,
            bytes: self.bytes,
        })
    }
}

/// A pause shorter than the longest backup plus a margin would let a running
/// backup outlive the wait, so it is never allowed. A policy that can't be
/// read counts with the longest time limit a policy may have.
pub async fn pause_floor(state: &AppState, repository_id: &str) -> anyhow::Result<Duration> {
    let rows = sqlx::query("SELECT name, spec_json FROM policies WHERE repository_id = $1")
        .bind(repository_id)
        .fetch_all(&state.db.pool)
        .await?;
    let mut floor = MIN_PAUSE;
    for row in rows {
        let name: String = row.try_get("name")?;
        let runtime = match serde_json::from_str::<BackupPolicy>(
            &row.try_get::<String, _>("spec_json")?,
        ) {
            Ok(policy) => max_runtime_of(&policy),
            Err(error) => {
                tracing::warn!(%error, policy = %name, "an unreadable policy counts with the longest time limit");
                POLICY_MAX_RUNTIME
            }
        };
        floor = floor.max(runtime + RUNTIME_MARGIN);
    }
    Ok(floor)
}

fn ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// When every backup of the repository that began before `before_ms` has
/// used up the time limit in its own job, plus the margin. The policy may
/// have been shortened or deleted since the job was queued, and the agent
/// follows the job. An interrupted job counts too: the agent may still be
/// running it. `None` when there is no such backup.
pub async fn backups_end(
    state: &AppState,
    repository_id: &str,
    before_ms: i64,
) -> anyhow::Result<Option<i64>> {
    let rows = sqlx::query(
        "SELECT j.spec, COALESCE(j.started_at, j.assigned_at, j.queued_at) AS began \
         FROM jobs j JOIN agents a ON a.id = j.host_id \
         WHERE a.repository_id = $1 AND j.kind <> 'restore' \
         AND j.state IN ('queued', 'assigned', 'running', 'interrupted') \
         AND COALESCE(j.started_at, j.assigned_at, j.queued_at) < $2",
    )
    .bind(repository_id)
    .bind(before_ms)
    .fetch_all(&state.db.pool)
    .await?;
    let mut end = None;
    for row in rows {
        let raw: Vec<u8> = row.try_get("spec")?;
        let limit = match JobSpec::decode(raw.as_slice()) {
            Ok(spec) if spec.max_runtime_s > 0 => Duration::from_secs(spec.max_runtime_s),
            Ok(_) => DEFAULT_MAX_RUNTIME,
            Err(_) => POLICY_MAX_RUNTIME,
        };
        let job_end = row
            .try_get::<i64, _>("began")?
            .saturating_add(ms(limit + RUNTIME_MARGIN));
        end = end.max(Some(job_end));
    }
    Ok(end)
}

/// Inserts a cycle in `marking`; the repository allows one unfinished cycle.
pub async fn start_cycle(
    state: &AppState,
    repository_id: &str,
    dry_run: bool,
    origin: GcOrigin,
    actor: &str,
) -> Result<String, ApiError> {
    get_repository(state, repository_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("repository not found".into()))?;
    let settings = db::load_settings(state, repository_id).await?;
    let floor = pause_floor(state, repository_id).await?;
    let id = uuid::Uuid::now_v7().to_string();
    db::insert_cycle(
        state,
        &id,
        repository_id,
        dry_run,
        origin,
        actor,
        &EffectiveSettings::freeze(&settings, floor),
    )
    .await?;
    Ok(id)
}

/// Runs the survey of a dry-run cycle and stores the report. A failure ends
/// the cycle as `failed`; nothing is written to the repository either way.
pub async fn run_dry_run(state: &AppState, cycle_id: &str) {
    run_dry_run_at(state, cycle_id, now_ms()).await;
}

/// [`run_dry_run`] planning as of `plan_now_ms`.
pub async fn run_dry_run_at(state: &AppState, cycle_id: &str, plan_now_ms: i64) {
    if let Err(error) = dry_run(state, cycle_id, plan_now_ms).await {
        let message = format!("{error:#}");
        tracing::warn!(cycle_id, error = %message, "dry run failed");
        if let Err(error) = db::fail_cycle(state, cycle_id, &message).await {
            tracing::warn!(cycle_id, %error, "recording a failed dry run failed");
        }
    }
}

async fn dry_run(state: &AppState, cycle_id: &str, plan_now_ms: i64) -> anyhow::Result<()> {
    let cycle = db::load_cycle(state, cycle_id)
        .await?
        .context("the cycle is missing")?;
    let started = now_ms();
    db::append_event(state, cycle_id, "info", "started", "Dry run started.").await?;
    let source = open_repository(state, &cycle.repository_id)
        .await
        .context("opening the repository")?;
    let opts = PlanOptions {
        now_ms: plan_now_ms,
        min_age_ms: cycle.settings.pause_ms,
        repack_below_pct: cycle.settings.repack_below_pct,
        max_repack_bytes: cycle.settings.max_repack_bytes,
        max_candidates: MAX_CANDIDATES,
    };

    let seen = Arc::new(Mutex::new(StoredProgress::default()));
    let record = {
        let seen = Arc::clone(&seen);
        move |event: SurveyProgress| {
            let SurveyProgress::Snapshots { done, total } = event;
            let mut seen = seen.lock().unwrap_or_else(|p| p.into_inner());
            seen.done = done;
            seen.total = total;
        }
    };
    let surveying = survey(&source.store, &source.keys, &source.config, &opts, &record);
    tokio::pin!(surveying);
    let mut tick = tokio::time::interval(PROGRESS_PERIOD);
    let surveyed = loop {
        tokio::select! {
            result = &mut surveying => break result,
            _ = tick.tick() => {
                let progress = seen.lock().unwrap_or_else(|p| p.into_inner()).clone();
                db::set_progress(state, cycle_id, &progress).await?;
            }
        }
    };
    let surveyed = surveyed.context("surveying the repository")?;

    let finished = now_ms();
    let retention = bucket_retention(state, &cycle.repository_id).await;
    let report = build_report(&cycle, &surveyed, finished, finished - started, retention);
    db::finish_cycle(state, cycle_id, &report).await?;
    db::append_event(
        state,
        cycle_id,
        "info",
        "finished",
        &format!(
            "Dry run finished: {} bytes can be freed.",
            report.summary.bytes_freed
        ),
    )
    .await?;
    Ok(())
}

/// The default retention of an Object Lock bucket, asked once; `None` for any
/// other storage or when the bucket sets none.
pub(crate) async fn bucket_retention(state: &AppState, repository_id: &str) -> Option<Duration> {
    let access = server_access(state, repository_id).await.ok()?;
    if access.location.backend != "s3" {
        return None;
    }
    default_retention(&access.location, access.server_creds.as_ref()?).await
}

/// When the storage lets go of an object written at `modified_ms`; `None`
/// when that is unknown or already past, which the storage then should not
/// have refused.
pub(crate) fn held_until(
    modified_ms: Option<i64>,
    retention: Option<Duration>,
    now_ms: i64,
) -> Option<i64> {
    modified_ms
        .zip(retention)
        .map(|(modified, retention)| modified.saturating_add(ms(retention)))
        .filter(|until| *until > now_ms)
}

pub(crate) fn build_report(
    cycle: &CycleRow,
    surveyed: &Survey,
    finished_at: i64,
    duration_ms: i64,
    retention: Option<Duration>,
) -> GcFullReport {
    let item = |p: &PackStat| GcPackItem {
        id: hex32(&p.pack_id),
        size: p.size,
        live_bytes: p.live_bytes,
    };
    let plan = &surveyed.plan;
    let dead = |p: &PackStat| p.size.saturating_sub(p.live_bytes);
    let mut locked_bytes = 0;
    let mut locked_until: Option<i64> = None;
    if retention.is_some() {
        let held = plan
            .delete
            .iter()
            .map(|p| (p, p.size))
            .chain(plan.repack.iter().map(|p| (p, dead(p))));
        for (pack, freed) in held {
            if let Some(until) = held_until(pack.modified_ms, retention, finished_at) {
                locked_bytes += freed;
                locked_until = Some(locked_until.map_or(until, |least| least.min(until)));
            }
        }
    }
    GcFullReport {
        summary: GcReport {
            cycle_id: cycle.id.clone(),
            dry_run: cycle.dry_run,
            finished_at,
            duration_ms: duration_ms.max(0) as u64,
            bytes_freed: plan.bytes_freed().saturating_sub(locked_bytes),
            packs_deleted: plan.delete.len() as u64,
            packs_repacked: plan.repack.len() as u64,
            repack_transfer_bytes: plan.repack_transfer_bytes(),
            locked_bytes,
            locked_until,
            locked_until_unknown: false,
        },
        delete: plan.delete.iter().map(item).collect(),
        repack: plan.repack.iter().map(item).collect(),
        ignored: surveyed
            .ignored
            .iter()
            .map(|(key, reason)| GcObjectNote {
                key: key.clone(),
                reason: reason.clone(),
            })
            .collect(),
        unindexed: plan
            .unindexed
            .iter()
            .map(|(key, size)| GcObjectNote {
                key: key.clone(),
                reason: format!("No index shard names this pack ({size} bytes)."),
            })
            .collect(),
        locked: Vec::new(),
    }
}

/// A dry run lives in this process only, so one left unfinished at start-up
/// was cut short by the restart.
pub async fn interrupt_dry_runs(state: &AppState) -> anyhow::Result<()> {
    let rows = sqlx::query(
        "SELECT id FROM gc_cycles WHERE dry_run = 1 AND state NOT IN ('done', 'failed', 'aborted')",
    )
    .fetch_all(&state.db.pool)
    .await?;
    for row in rows {
        db::fail_cycle(state, &row.try_get::<String, _>("id")?, RESTART_MESSAGE).await?;
    }
    Ok(())
}

fn schedule_errors(cron: &str, timezone: &str) -> Vec<FieldError> {
    let mut errors = Vec::new();
    let candidate = |cron: &str, timezone: Option<&str>| Schedule {
        cron: cron.to_string(),
        timezone: timezone.map(str::to_string),
        jitter: None,
        catch_up_window: None,
    };
    for (path, schedule) in [
        ("spec.schedule.cron", candidate(cron, None)),
        (
            "spec.schedule.timezone",
            candidate(DEFAULT_CRON, Some(timezone)),
        ),
    ] {
        if let Err(problem) = schedule.compile() {
            errors.push(FieldError {
                path: path.into(),
                message: problem.message,
            });
        }
    }
    errors
}

/// Checks every field and reports all that are wrong, with the wording the
/// settings form shows next to them.
pub fn validate_settings(
    input: &GcSettingsInput,
    floor: Duration,
) -> Result<GcSettings, Vec<FieldError>> {
    let mut errors = schedule_errors(input.cron.trim(), input.timezone.trim());
    let pause = parse_duration(input.pause.trim()).filter(|pause| *pause >= floor);
    if pause.is_none() {
        errors.push(FieldError {
            path: "spec.pause".into(),
            message: format!(
                "Use a duration of {} or more, such as 24h or 2d.",
                format_duration(floor)
            ),
        });
    }
    let repack_below_pct = u8::try_from(input.repack_below_pct)
        .ok()
        .filter(|pct| (1..=99).contains(pct));
    if repack_below_pct.is_none() {
        errors.push(FieldError {
            path: "spec.repackBelow".into(),
            message: "Enter a whole number from 1 to 99.".into(),
        });
    }
    let max_repack_bytes =
        parse_size(input.max_repack.trim()).filter(|size| *size >= MIN_MAX_REPACK_BYTES);
    if max_repack_bytes.is_none() {
        errors.push(FieldError {
            path: "spec.maxRepack".into(),
            message: "Use a size such as 10GiB or 500MiB.".into(),
        });
    }
    match (pause, repack_below_pct, max_repack_bytes) {
        (Some(pause), Some(repack_below_pct), Some(max_repack_bytes)) if errors.is_empty() => {
            Ok(GcSettings {
                enabled: input.enabled,
                cron: input.cron.trim().to_string(),
                timezone: input.timezone.trim().to_string(),
                pause,
                repack_below_pct,
                max_repack_bytes,
            })
        }
        _ => Err(errors),
    }
}

/// The cycles this process is driving right now.
#[derive(Clone, Default)]
pub struct GcRuns(Arc<Mutex<HashSet<String>>>);

pub struct RunGuard {
    runs: GcRuns,
    id: String,
}

impl GcRuns {
    /// `None` when the cycle is being driven already.
    pub fn claim(&self, id: &str) -> Option<RunGuard> {
        let mut running = self.0.lock().unwrap_or_else(|p| p.into_inner());
        running.insert(id.to_string()).then(|| RunGuard {
            runs: self.clone(),
            id: id.to_string(),
        })
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.runs
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.id);
    }
}

fn spawn_drive(state: &AppState, cycle_id: &str) {
    tokio::spawn(cycle::drive(state.clone(), cycle_id.to_string()));
}

/// Starts a cleanup cycle, or continues the one that stopped while deleting.
pub async fn start_real_cycle(
    state: &AppState,
    repository_id: &str,
    origin: GcOrigin,
    actor: &str,
) -> Result<String, GcFailure> {
    get_repository(state, repository_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("repository not found".into()))?;
    if let Some(last) = db::last_real_cycle(state, repository_id).await?
        && last.state == GcState::Failed
        && last.error_after_delete
        && cycle::marker_in_bucket(state, &last).await?
    {
        resume(state, &last, &format!("started by {actor}")).await?;
        return Ok(last.id);
    }
    if !db::load_settings(state, repository_id).await?.enabled {
        return Err(ApiError::Conflict("Turn on cleanup for this repository first.".into()).into());
    }
    let preflight = preflight::run_preflight(state, repository_id, true).await?;
    if !preflight.problems.is_empty() {
        return Err(GcFailure::Preflight(preflight.problems));
    }
    let id = start_cycle(state, repository_id, false, origin, actor).await?;
    spawn_drive(state, &id);
    Ok(id)
}

/// Puts a cycle that stopped while deleting back to work. Finishing a
/// deletion that has started is safer than leaving it half done, so this
/// does not need cleanup to be turned on.
async fn resume(state: &AppState, failed: &CycleRow, by: &str) -> Result<(), GcFailure> {
    if let Some(problem) = preflight::old_agents_problem(state, &failed.repository_id).await? {
        return Err(GcFailure::Preflight(vec![problem]));
    }
    let admit = state.checks.admit.lock().await;
    if cycle::restores_active(state, &failed.repository_id).await? {
        return Err(ApiError::Conflict(
            "Restores, downloads or checks are reading this repository. \
             The cleanup continues when they finish."
                .into(),
        )
        .into());
    }
    if !db::resume_failed(state, &failed.id).await? {
        return Err(ApiError::Conflict(
            "A cleanup cycle is already running in this repository.".into(),
        )
        .into());
    }
    drop(admit);
    let message = format!("Cleanup continues where it stopped ({by}).");
    cycle::event(state, &failed.id, "info", "resumed", &message).await;
    spawn_drive(state, &failed.id);
    Ok(())
}

/// How long a cycle that stopped while deleting rests before the server
/// tries it again by itself: doubling with every failure, up to a day.
fn resume_backoff(failures: u32) -> Duration {
    RESUME_FIRST_BACKOFF
        .saturating_mul(1 << failures.saturating_sub(1).min(16))
        .min(RESUME_MAX_BACKOFF)
}

/// Retries the cycles that stopped while deleting, whether or not cleanup
/// is still turned on: their marker keeps every writer away from the marked
/// packs and old agents from backing up until they finish.
async fn resume_stopped(state: &AppState, now_ms: i64) -> anyhow::Result<()> {
    for failed in db::stopped_while_deleting(state).await? {
        let failures = db::count_events(state, &failed.id, "failed").await?;
        let due = failed
            .finished_at
            .is_none_or(|at| now_ms >= at.saturating_add(ms(resume_backoff(failures))));
        let busy = db::active_cycle_of(state, &failed.repository_id, false)
            .await?
            .is_some()
            || cycle::restores_active(state, &failed.repository_id).await?;
        if !due || busy {
            continue;
        }
        match resume(state, &failed, "retried by the server").await {
            Ok(()) => {}
            Err(GcFailure::Preflight(_)) => {
                tracing::debug!(cycle_id = %failed.id, "an old agent holds a stopped cleanup back");
            }
            Err(_) => tracing::warn!(cycle_id = %failed.id, "retrying a stopped cleanup failed"),
        }
    }
    Ok(())
}

/// Picks up unfinished cycles nobody in this process is driving, such as
/// those left by a restart or by a wait that is over.
pub async fn tick_once(state: &AppState, now_ms: i64) -> anyhow::Result<()> {
    match Timestamp::from_millisecond(now_ms) {
        Ok(now) => {
            if let Err(error) = schedule::tick(state, now).await {
                tracing::warn!(%error, "scheduled cleanup tick failed");
            }
        }
        Err(error) => tracing::warn!(%error, "the clock is out of range"),
    }
    for cycle in db::unfinished_real_cycles(state).await? {
        let waiting = cycle.state == GcState::Waiting
            && cycle.delete_after.is_some_and(|until| until > now_ms);
        if !waiting {
            spawn_drive(state, &cycle.id);
        }
    }
    if let Err(error) = resume_stopped(state, now_ms).await {
        tracing::warn!(%error, "retrying stopped cleanups failed");
    }
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
            tracing::warn!(%error, "cleanup tick failed");
        }
    }
}
