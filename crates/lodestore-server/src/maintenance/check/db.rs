use std::collections::{HashMap, HashSet};

use anyhow::Context;
use serde::Deserialize;
use sqlx::Row;
use sqlx::any::AnyRow;

use super::api::{CheckProgress, PackCoverage, RepoUsageDto, StoredReport};
use super::{
    CHECK_RUNNING, CheckLevel, CheckLevelSettings, CheckOrigin, CheckOutcome, CheckState,
    ChecksState, ChecksSummary,
};
use crate::db::now_ms;
use crate::error::ApiError;
use crate::state::AppState;

const ACTIVE: &str = "('queued', 'running')";
const DAY_MS: i64 = 86_400_000;
pub const COVERAGE_DAYS: i64 = 30;

fn clamp(n: i64) -> u64 {
    n.max(0) as u64
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct Limits {
    read_per_run: Option<u64>,
    files_per_run: Option<u32>,
    max_data_per_run: Option<u64>,
}

/// The settings of every level; a level without a row runs on its defaults.
pub async fn load_settings(
    state: &AppState,
    repository_id: &str,
) -> anyhow::Result<Vec<(CheckLevel, CheckLevelSettings)>> {
    let rows = sqlx::query(
        "SELECT level, enabled, cron, timezone, limits_json FROM check_settings \
         WHERE repository_id = $1",
    )
    .bind(repository_id)
    .fetch_all(&state.db.pool)
    .await?;
    let mut all: Vec<_> = CheckLevel::ALL
        .into_iter()
        .map(|level| (level, CheckLevelSettings::defaults(level)))
        .collect();
    for row in rows {
        let name: String = row.try_get("level")?;
        let Some((_, settings)) = all.iter_mut().find(|(level, _)| level.as_str() == name) else {
            continue;
        };
        let limits: Limits = row
            .try_get::<Option<String>, _>("limits_json")?
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        settings.enabled = row.try_get::<i64, _>("enabled")? != 0;
        settings.cron = row.try_get("cron")?;
        settings.timezone = row.try_get("timezone")?;
        if let Some(bytes) = limits.read_per_run {
            settings.read_per_run = bytes;
        }
        if let Some(files) = limits.files_per_run {
            settings.files_per_run = files;
        }
        if let Some(bytes) = limits.max_data_per_run {
            settings.max_data_per_run = bytes;
        }
    }
    Ok(all)
}

/// Stores the settings of every level given; the schedule cursor stays.
pub async fn save_settings(
    state: &AppState,
    repository_id: &str,
    levels: &[(CheckLevel, CheckLevelSettings)],
) -> anyhow::Result<()> {
    let now = now_ms();
    let mut tx = state.db.pool.begin().await?;
    for (level, settings) in levels {
        let limits = match level {
            CheckLevel::Structure => None,
            CheckLevel::Packs => Some(serde_json::json!({ "read_per_run": settings.read_per_run })),
            CheckLevel::Restore => Some(serde_json::json!({
                "files_per_run": settings.files_per_run,
                "max_data_per_run": settings.max_data_per_run,
            })),
        };
        sqlx::query(
            "INSERT INTO check_settings (repository_id, level, enabled, cron, timezone, \
             limits_json, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (repository_id, level) DO UPDATE SET enabled = excluded.enabled, \
             cron = excluded.cron, timezone = excluded.timezone, \
             limits_json = excluded.limits_json, updated_at = excluded.updated_at",
        )
        .bind(repository_id)
        .bind(level.as_str())
        .bind(i64::from(settings.enabled))
        .bind(&settings.cron)
        .bind(&settings.timezone)
        .bind(limits.map(|json| json.to_string()))
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The worst result of the enabled levels: a failed one, else one that never
/// ran, else passed. Reads the catalog only, never the bucket.
pub async fn repo_checks_summary(
    state: &AppState,
    repository_id: &str,
) -> anyhow::Result<ChecksSummary> {
    let mut failed: Option<i64> = None;
    let mut passed: Option<i64> = None;
    let mut never_run = false;
    let mut enabled = false;
    for (level, settings) in load_settings(state, repository_id).await? {
        if !settings.enabled {
            continue;
        }
        enabled = true;
        match last_finished(state, repository_id, level).await? {
            None => never_run = true,
            Some(run) if run.state == CheckState::Failed => {
                failed = failed.max(run.finished_at);
            }
            Some(run) => passed = passed.max(run.finished_at),
        }
    }
    let (state, finished_at) = if !enabled {
        (ChecksState::Off, None)
    } else if let Some(at) = failed {
        (ChecksState::Failed, Some(at))
    } else if never_run {
        (ChecksState::NeverRun, None)
    } else {
        (ChecksState::Passed, passed)
    };
    Ok(ChecksSummary { state, finished_at })
}

pub async fn load_level_settings(
    state: &AppState,
    repository_id: &str,
    level: CheckLevel,
) -> anyhow::Result<CheckLevelSettings> {
    load_settings(state, repository_id)
        .await?
        .into_iter()
        .find(|(candidate, _)| *candidate == level)
        .map(|(_, settings)| settings)
        .context("the level has no settings")
}

#[derive(Debug, Clone)]
pub struct RunRow {
    pub id: String,
    pub repository_id: String,
    pub level: CheckLevel,
    pub origin: CheckOrigin,
    pub started_by: String,
    pub state: CheckState,
    pub outcome: Option<CheckOutcome>,
    pub cancel_requested: bool,
    pub progress: Option<CheckProgress>,
    pub report: Option<StoredReport>,
    pub findings_total: u64,
    pub error: Option<String>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

pub(crate) const RUN_COLUMNS: &str = "id, repository_id, level, origin, started_by, state, \
    outcome, cancel_requested, progress_json, report_json, findings_total, error, created_at, \
    started_at, finished_at";

fn unknown(what: &str, value: &str) -> anyhow::Error {
    anyhow::anyhow!("unknown check run {what} \"{value}\"")
}

pub(crate) fn run_from_row(row: &AnyRow) -> anyhow::Result<RunRow> {
    let level: String = row.try_get("level")?;
    let origin: String = row.try_get("origin")?;
    let state: String = row.try_get("state")?;
    let outcome: Option<String> = row.try_get("outcome")?;
    Ok(RunRow {
        id: row.try_get("id")?,
        repository_id: row.try_get("repository_id")?,
        level: CheckLevel::parse(&level).ok_or_else(|| unknown("level", &level))?,
        origin: CheckOrigin::parse(&origin).ok_or_else(|| unknown("origin", &origin))?,
        started_by: row.try_get("started_by")?,
        state: CheckState::parse(&state).ok_or_else(|| unknown("state", &state))?,
        outcome: outcome
            .map(|text| CheckOutcome::parse(&text).ok_or_else(|| unknown("outcome", &text)))
            .transpose()?,
        cancel_requested: row.try_get::<i64, _>("cancel_requested")? != 0,
        progress: row
            .try_get::<Option<String>, _>("progress_json")?
            .and_then(|json| serde_json::from_str(&json).ok()),
        report: row
            .try_get::<Option<String>, _>("report_json")?
            .and_then(|json| serde_json::from_str(&json).ok()),
        findings_total: clamp(row.try_get("findings_total")?),
        error: row.try_get("error")?,
        created_at: row.try_get("created_at")?,
        started_at: row.try_get("started_at")?,
        finished_at: row.try_get("finished_at")?,
    })
}

pub async fn insert_run(
    state: &AppState,
    id: &str,
    repository_id: &str,
    level: CheckLevel,
    origin: CheckOrigin,
    actor: &str,
) -> Result<(), ApiError> {
    let inserted = sqlx::query(
        "INSERT INTO check_runs (id, repository_id, level, origin, started_by, state, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(repository_id)
    .bind(level.as_str())
    .bind(origin.as_str())
    .bind(actor)
    .bind(CheckState::Queued.as_str())
    .bind(now_ms())
    .execute(&state.db.pool)
    .await;
    match inserted {
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            Err(ApiError::Conflict(CHECK_RUNNING.into()))
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn load_run(state: &AppState, id: &str) -> anyhow::Result<Option<RunRow>> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {RUN_COLUMNS} FROM check_runs WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(&state.db.pool)
    .await?
    .as_ref()
    .map(run_from_row)
    .transpose()
}

pub async fn active_run(state: &AppState, repository_id: &str) -> anyhow::Result<Option<RunRow>> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {RUN_COLUMNS} FROM check_runs WHERE repository_id = $1 AND state IN {ACTIVE}"
    )))
    .bind(repository_id)
    .fetch_optional(&state.db.pool)
    .await?
    .as_ref()
    .map(run_from_row)
    .transpose()
}

/// The newest run of the level that reached a verdict. An aborted one did not,
/// nor did one cut short by a restart or an error, which has no report; a run
/// that stopped at a limit did report.
pub async fn last_finished(
    state: &AppState,
    repository_id: &str,
    level: CheckLevel,
) -> anyhow::Result<Option<RunRow>> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {RUN_COLUMNS} FROM check_runs \
         WHERE repository_id = $1 AND level = $2 AND state IN ('done', 'failed') \
         AND (outcome IS NULL OR outcome <> 'stopped' OR report_json IS NOT NULL) \
         ORDER BY created_at DESC, id DESC LIMIT 1"
    )))
    .bind(repository_id)
    .bind(level.as_str())
    .fetch_optional(&state.db.pool)
    .await?
    .as_ref()
    .map(run_from_row)
    .transpose()
}

/// Whether the level has ever been run, whatever came of it.
pub async fn has_run(
    state: &AppState,
    repository_id: &str,
    level: CheckLevel,
) -> anyhow::Result<bool> {
    Ok(
        sqlx::query("SELECT 1 FROM check_runs WHERE repository_id = $1 AND level = $2 LIMIT 1")
            .bind(repository_id)
            .bind(level.as_str())
            .fetch_optional(&state.db.pool)
            .await?
            .is_some(),
    )
}

/// Moves a queued run to `running`; `false` when it was cancelled first.
pub async fn start(state: &AppState, id: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query(
        "UPDATE check_runs SET state = 'running', started_at = $1 \
         WHERE id = $2 AND state = 'queued'",
    )
    .bind(now_ms())
    .bind(id)
    .execute(&state.db.pool)
    .await?
    .rows_affected()
        == 1)
}

pub async fn set_progress(
    state: &AppState,
    id: &str,
    progress: &CheckProgress,
) -> anyhow::Result<()> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE check_runs SET progress_json = $1 WHERE id = $2 AND state IN {ACTIVE}"
    )))
    .bind(serde_json::to_string(progress)?)
    .bind(id)
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

pub async fn finish_run(
    state: &AppState,
    id: &str,
    outcome: CheckOutcome,
    report: &StoredReport,
) -> anyhow::Result<()> {
    let end = if outcome == CheckOutcome::Passed {
        CheckState::Done
    } else {
        CheckState::Failed
    };
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE check_runs SET state = $1, outcome = $2, report_json = $3, findings_total = $4, \
         finished_at = $5 WHERE id = $6 AND state IN {ACTIVE}"
    )))
    .bind(end.as_str())
    .bind(outcome.as_str())
    .bind(serde_json::to_string(report)?)
    .bind(i64::try_from(report.findings_total).unwrap_or(i64::MAX))
    .bind(now_ms())
    .bind(id)
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

/// A run that stopped before it reached a verdict, with the report of what it
/// did.
pub async fn stop_run(
    state: &AppState,
    id: &str,
    error: &str,
    report: &StoredReport,
) -> anyhow::Result<()> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE check_runs SET state = 'failed', outcome = 'stopped', error = $1, \
         report_json = $2, findings_total = $3, finished_at = $4 \
         WHERE id = $5 AND state IN {ACTIVE}"
    )))
    .bind(error)
    .bind(serde_json::to_string(report)?)
    .bind(i64::try_from(report.findings_total).unwrap_or(i64::MAX))
    .bind(now_ms())
    .bind(id)
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

/// The check could not finish: this is not a finding in the repository.
pub async fn fail_run(state: &AppState, id: &str, error: &str) -> anyhow::Result<()> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE check_runs SET state = 'failed', outcome = 'stopped', error = $1, \
         finished_at = $2 WHERE id = $3 AND state IN {ACTIVE}"
    )))
    .bind(error)
    .bind(now_ms())
    .bind(id)
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

/// `false` when the run had ended already.
pub async fn abort_run(state: &AppState, id: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE check_runs SET state = 'aborted', finished_at = $1 \
         WHERE id = $2 AND state IN {ACTIVE}"
    )))
    .bind(now_ms())
    .bind(id)
    .execute(&state.db.pool)
    .await?
    .rows_affected()
        == 1)
}

/// Cancels a check the caller saw queued; one that started meanwhile is
/// asked to stop instead.
pub async fn cancel_queued(state: &AppState, id: &str) -> anyhow::Result<bool> {
    let aborted = sqlx::query(
        "UPDATE check_runs SET state = 'aborted', finished_at = $1 \
         WHERE id = $2 AND state = 'queued'",
    )
    .bind(now_ms())
    .bind(id)
    .execute(&state.db.pool)
    .await?
    .rows_affected()
        == 1;
    Ok(aborted || request_cancel(state, id).await?)
}

/// Asks the task driving a running check to stop at its next tick.
pub async fn request_cancel(state: &AppState, id: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query(
        "UPDATE check_runs SET cancel_requested = 1 WHERE id = $1 AND state = 'running'",
    )
    .bind(id)
    .execute(&state.db.pool)
    .await?
    .rows_affected()
        == 1)
}

pub async fn cancel_requested(state: &AppState, id: &str) -> anyhow::Result<bool> {
    let row = sqlx::query("SELECT cancel_requested FROM check_runs WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db.pool)
        .await?;
    Ok(match row {
        Some(row) => row.try_get::<i64, _>("cancel_requested")? != 0,
        None => true,
    })
}

#[derive(Debug, Clone, Copy)]
pub struct PackRow {
    pub first_seen_at: i64,
    pub checked_at: Option<i64>,
    pub bad: bool,
}

/// Makes the rotation memory match the packs the bucket lists: new packs
/// get a row, packs that are gone lose theirs. Returns the rows as they are
/// after that.
pub async fn sync_packs(
    state: &AppState,
    repository_id: &str,
    listed: &HashSet<String>,
    now: i64,
) -> anyhow::Result<HashMap<String, PackRow>> {
    let rows = sqlx::query(
        "SELECT pack_id, first_seen_at, checked_at, bad FROM pack_checks WHERE repository_id = $1",
    )
    .bind(repository_id)
    .fetch_all(&state.db.pool)
    .await?;
    let mut known = HashMap::with_capacity(rows.len().max(listed.len()));
    for row in rows {
        known.insert(
            row.try_get::<String, _>("pack_id")?,
            PackRow {
                first_seen_at: row.try_get("first_seen_at")?,
                checked_at: row.try_get("checked_at")?,
                bad: row.try_get::<i64, _>("bad")? != 0,
            },
        );
    }
    let gone: Vec<String> = known
        .keys()
        .filter(|id| !listed.contains(*id))
        .cloned()
        .collect();
    let added: Vec<&String> = listed
        .iter()
        .filter(|id| !known.contains_key(*id))
        .collect();
    if gone.is_empty() && added.is_empty() {
        return Ok(known);
    }
    let mut tx = state.db.pool.begin().await?;
    for id in &gone {
        sqlx::query("DELETE FROM pack_checks WHERE repository_id = $1 AND pack_id = $2")
            .bind(repository_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        known.remove(id);
    }
    for id in added {
        sqlx::query(
            "INSERT INTO pack_checks (repository_id, pack_id, first_seen_at, checked_at, bad) \
             VALUES ($1, $2, $3, NULL, 0) ON CONFLICT (repository_id, pack_id) DO NOTHING",
        )
        .bind(repository_id)
        .bind(id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        known.insert(
            id.clone(),
            PackRow {
                first_seen_at: now,
                checked_at: None,
                bad: false,
            },
        );
    }
    tx.commit().await?;
    Ok(known)
}

/// What a pack reading run found out about the packs it read: `Some(bad)`
/// for a pack that was read, `None` for one that was not in the bucket.
pub async fn record_packs(
    state: &AppState,
    repository_id: &str,
    results: &[(String, Option<bool>)],
    now: i64,
) -> anyhow::Result<()> {
    let mut tx = state.db.pool.begin().await?;
    for (id, result) in results {
        match result {
            Some(bad) => {
                sqlx::query(
                    "UPDATE pack_checks SET checked_at = $1, bad = $2 \
                     WHERE repository_id = $3 AND pack_id = $4",
                )
                .bind(now)
                .bind(i64::from(*bad))
                .bind(repository_id)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            }
            None => {
                sqlx::query("DELETE FROM pack_checks WHERE repository_id = $1 AND pack_id = $2")
                    .bind(repository_id)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
    }
    tx.commit().await?;
    Ok(())
}

/// How much of the repository pack reading has covered lately. `None` until
/// a run has listed the packs.
pub async fn coverage(
    state: &AppState,
    repository_id: &str,
    now: i64,
) -> anyhow::Result<Option<PackCoverage>> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS packs, \
         COUNT(CASE WHEN checked_at IS NULL THEN 1 END) AS never, \
         COUNT(CASE WHEN checked_at >= $2 THEN 1 END) AS recent \
         FROM pack_checks WHERE repository_id = $1",
    )
    .bind(repository_id)
    .bind(now - COVERAGE_DAYS * DAY_MS)
    .fetch_one(&state.db.pool)
    .await?;
    let packs = clamp(row.try_get("packs")?);
    if packs == 0 {
        return Ok(None);
    }
    Ok(Some(PackCoverage {
        packs,
        checked_30d_pct: clamp(row.try_get("recent")?) * 100 / packs,
        never_checked: clamp(row.try_get("never")?),
    }))
}

pub async fn load_usage(
    state: &AppState,
    repository_id: &str,
) -> anyhow::Result<Option<RepoUsageDto>> {
    let row = sqlx::query(
        "SELECT physical_bytes, logical_bytes, packs, counted_at FROM repo_usage \
         WHERE repository_id = $1",
    )
    .bind(repository_id)
    .fetch_optional(&state.db.pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let physical = clamp(row.try_get("physical_bytes")?);
    let logical = clamp(row.try_get("logical_bytes")?);
    Ok(Some(RepoUsageDto {
        physical_bytes: physical,
        logical_bytes: logical,
        dedup_ratio: (physical > 0).then(|| logical as f64 / physical as f64),
        packs: clamp(row.try_get("packs")?),
        counted_at: row.try_get("counted_at")?,
    }))
}

pub async fn save_usage(
    state: &AppState,
    repository_id: &str,
    physical_bytes: u64,
    logical_bytes: u64,
    packs: u64,
    objects: u64,
) -> anyhow::Result<()> {
    let n = |value: u64| i64::try_from(value).unwrap_or(i64::MAX);
    sqlx::query(
        "INSERT INTO repo_usage (repository_id, physical_bytes, logical_bytes, packs, objects, \
         counted_at) VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (repository_id) DO UPDATE SET physical_bytes = excluded.physical_bytes, \
         logical_bytes = excluded.logical_bytes, packs = excluded.packs, \
         objects = excluded.objects, counted_at = excluded.counted_at",
    )
    .bind(repository_id)
    .bind(n(physical_bytes))
    .bind(n(logical_bytes))
    .bind(n(packs))
    .bind(n(objects))
    .bind(now_ms())
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

/// What the snapshots in the catalog add up to before deduplication.
pub async fn logical_bytes(state: &AppState, repository_id: &str) -> anyhow::Result<u64> {
    #[derive(Deserialize)]
    struct Total {
        bytes_total: u64,
    }
    let rows = sqlx::query("SELECT stats_json FROM snapshots WHERE repository_id = $1")
        .bind(repository_id)
        .fetch_all(&state.db.pool)
        .await?;
    let mut sum = 0u64;
    for row in rows {
        if let Ok(total) = serde_json::from_str::<Total>(&row.try_get::<String, _>("stats_json")?) {
            sum = sum.saturating_add(total.bytes_total);
        }
    }
    Ok(sum)
}
