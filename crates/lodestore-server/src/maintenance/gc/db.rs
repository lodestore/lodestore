use std::collections::HashSet;
use std::time::Duration;

use anyhow::Context;
use sqlx::Row;
use sqlx::any::AnyRow;

use lodestore_format::attest::read_attestations;
use lodestore_format::crypto::BlobId;
use lodestore_format::gc::mark::{MarkPolicy, Quarantined};
use lodestore_format::index::{hex32, parse_hex32};
use lodestore_format::provenance::{ProvenanceSet, Signer};
use lodestore_format::store::BlobStore;

use super::api::{GcEventDto, GcFullReport, GcQuarantineDto, GcReport};
use super::{EffectiveSettings, GcOrigin, GcSettings, GcState, StoredProgress};
use crate::catalog::OpenRepository;
use crate::db::now_ms;
use crate::error::ApiError;
use crate::grpc::dispatch::messages::clip;
use crate::state::AppState;

const FINISHED: &str = "('done', 'failed', 'aborted')";
const EVENTS_SHOWN: i64 = 20;

fn clamp(n: i64) -> u64 {
    n.max(0) as u64
}

pub async fn load_settings(state: &AppState, repository_id: &str) -> anyhow::Result<GcSettings> {
    let row = sqlx::query(
        "SELECT enabled, cron, timezone, pause_ms, repack_below_pct, max_repack_bytes \
         FROM gc_settings WHERE repository_id = $1",
    )
    .bind(repository_id)
    .fetch_optional(&state.db.pool)
    .await?;
    let Some(row) = row else {
        return Ok(GcSettings::default());
    };
    Ok(GcSettings {
        enabled: row.try_get::<i64, _>("enabled")? != 0,
        cron: row.try_get("cron")?,
        timezone: row.try_get("timezone")?,
        pause: Duration::from_millis(clamp(row.try_get("pause_ms")?)),
        repack_below_pct: u8::try_from(row.try_get::<i64, _>("repack_below_pct")?)
            .unwrap_or(u8::MAX),
        max_repack_bytes: clamp(row.try_get("max_repack_bytes")?),
    })
}

pub async fn save_settings(
    state: &AppState,
    repository_id: &str,
    settings: &GcSettings,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO gc_settings (repository_id, enabled, cron, timezone, pause_ms, \
         repack_below_pct, max_repack_bytes, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         ON CONFLICT (repository_id) DO UPDATE SET enabled = excluded.enabled, \
         cron = excluded.cron, timezone = excluded.timezone, pause_ms = excluded.pause_ms, \
         repack_below_pct = excluded.repack_below_pct, \
         max_repack_bytes = excluded.max_repack_bytes, updated_at = excluded.updated_at",
    )
    .bind(repository_id)
    .bind(i64::from(settings.enabled))
    .bind(&settings.cron)
    .bind(&settings.timezone)
    .bind(i64::try_from(settings.pause.as_millis()).unwrap_or(i64::MAX))
    .bind(i64::from(settings.repack_below_pct))
    .bind(i64::try_from(settings.max_repack_bytes).unwrap_or(i64::MAX))
    .bind(now_ms())
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

pub async fn save_probe(
    state: &AppState,
    repository_id: &str,
    probe_json: &str,
) -> anyhow::Result<()> {
    let defaults = GcSettings::default();
    sqlx::query(
        "INSERT INTO gc_settings (repository_id, enabled, cron, timezone, pause_ms, \
         repack_below_pct, max_repack_bytes, probe_json, probe_at, updated_at) \
         VALUES ($1, 0, $2, $3, $4, $5, $6, $7, $8, $8) \
         ON CONFLICT (repository_id) DO UPDATE SET probe_json = excluded.probe_json, \
         probe_at = excluded.probe_at",
    )
    .bind(repository_id)
    .bind(&defaults.cron)
    .bind(&defaults.timezone)
    .bind(i64::try_from(defaults.pause.as_millis()).unwrap_or(i64::MAX))
    .bind(i64::from(defaults.repack_below_pct))
    .bind(i64::try_from(defaults.max_repack_bytes).unwrap_or(i64::MAX))
    .bind(probe_json)
    .bind(now_ms())
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

pub async fn load_probe(
    state: &AppState,
    repository_id: &str,
) -> anyhow::Result<Option<(String, i64)>> {
    let row = sqlx::query("SELECT probe_json, probe_at FROM gc_settings WHERE repository_id = $1")
        .bind(repository_id)
        .fetch_optional(&state.db.pool)
        .await?;
    let Some(row) = row else { return Ok(None) };
    let json: Option<String> = row.try_get("probe_json")?;
    let at: Option<i64> = row.try_get("probe_at")?;
    Ok(json.zip(at))
}

#[derive(Debug, Clone)]
pub struct CycleRow {
    pub id: String,
    pub repository_id: String,
    pub state: GcState,
    pub dry_run: bool,
    pub origin: GcOrigin,
    pub started_by: String,
    pub settings: EffectiveSettings,
    pub gc_id: Option<String>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub published_at: Option<i64>,
    pub delete_after: Option<i64>,
    pub sweep_started: bool,
    pub finished_at: Option<i64>,
    pub progress: StoredProgress,
    pub error: Option<String>,
    pub error_after_delete: bool,
    pub resurrected: Vec<String>,
    /// The index shards the cycle's compaction replaced. Its sweep deletes
    /// these and no other shard.
    pub rewrite: Vec<String>,
}

pub(crate) const CYCLE_COLUMNS: &str = "id, repository_id, state, dry_run, origin, started_by, \
    settings_json, gc_id, created_at, started_at, published_at, delete_after, sweep_started, \
    finished_at, progress_json, error, error_after_delete, resurrected_json, rewrite_json";

fn json_list(row: &AnyRow, column: &str) -> anyhow::Result<Vec<String>> {
    Ok(row
        .try_get::<Option<String>, _>(column)?
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default())
}

fn unknown(what: &str, value: &str) -> anyhow::Error {
    anyhow::anyhow!("unknown cleanup cycle {what} \"{value}\"")
}

pub(crate) fn cycle_from_row(row: &AnyRow) -> anyhow::Result<CycleRow> {
    let state: String = row.try_get("state")?;
    let origin: String = row.try_get("origin")?;
    Ok(CycleRow {
        id: row.try_get("id")?,
        repository_id: row.try_get("repository_id")?,
        state: GcState::parse(&state).ok_or_else(|| unknown("state", &state))?,
        dry_run: row.try_get::<i64, _>("dry_run")? != 0,
        origin: GcOrigin::parse(&origin).ok_or_else(|| unknown("origin", &origin))?,
        started_by: row.try_get("started_by")?,
        settings: serde_json::from_str(&row.try_get::<String, _>("settings_json")?)
            .context("reading the frozen cleanup settings")?,
        gc_id: row.try_get("gc_id")?,
        created_at: row.try_get("created_at")?,
        started_at: row.try_get("started_at")?,
        published_at: row.try_get("published_at")?,
        delete_after: row.try_get("delete_after")?,
        sweep_started: row.try_get::<i64, _>("sweep_started")? != 0,
        finished_at: row.try_get("finished_at")?,
        progress: row
            .try_get::<Option<String>, _>("progress_json")?
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default(),
        error: row.try_get("error")?,
        error_after_delete: row.try_get::<i64, _>("error_after_delete")? != 0,
        resurrected: json_list(row, "resurrected_json")?,
        rewrite: json_list(row, "rewrite_json")?,
    })
}

pub async fn insert_cycle(
    state: &AppState,
    id: &str,
    repository_id: &str,
    dry_run: bool,
    origin: GcOrigin,
    actor: &str,
    settings: &EffectiveSettings,
) -> Result<(), ApiError> {
    let now = now_ms();
    let inserted = sqlx::query(
        "INSERT INTO gc_cycles (id, repository_id, state, dry_run, origin, started_by, \
         settings_json, created_at, started_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8)",
    )
    .bind(id)
    .bind(repository_id)
    .bind(GcState::Marking.as_str())
    .bind(i64::from(dry_run))
    .bind(origin.as_str())
    .bind(actor)
    .bind(serde_json::to_string(settings).map_err(|e| ApiError::Internal(e.into()))?)
    .bind(now)
    .execute(&state.db.pool)
    .await;
    match inserted {
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            Err(ApiError::Conflict(if dry_run {
                "A dry run is already running in this repository.".into()
            } else {
                "A cleanup cycle is already running in this repository.".into()
            }))
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn load_cycle(state: &AppState, id: &str) -> anyhow::Result<Option<CycleRow>> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {CYCLE_COLUMNS} FROM gc_cycles WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(&state.db.pool)
    .await?
    .as_ref()
    .map(cycle_from_row)
    .transpose()
}

/// The unfinished cycle of the repository: the real one when there is one,
/// otherwise a dry run. A dry run has its own slot, so both can be running.
pub async fn active_cycle(
    state: &AppState,
    repository_id: &str,
) -> anyhow::Result<Option<CycleRow>> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {CYCLE_COLUMNS} FROM gc_cycles \
         WHERE repository_id = $1 AND state NOT IN {FINISHED} ORDER BY dry_run LIMIT 1"
    )))
    .bind(repository_id)
    .fetch_optional(&state.db.pool)
    .await?
    .as_ref()
    .map(cycle_from_row)
    .transpose()
}

/// The unfinished cycle of one kind.
pub async fn active_cycle_of(
    state: &AppState,
    repository_id: &str,
    dry_run: bool,
) -> anyhow::Result<Option<CycleRow>> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {CYCLE_COLUMNS} FROM gc_cycles \
         WHERE repository_id = $1 AND dry_run = $2 AND state NOT IN {FINISHED}"
    )))
    .bind(repository_id)
    .bind(i64::from(dry_run))
    .fetch_optional(&state.db.pool)
    .await?
    .as_ref()
    .map(cycle_from_row)
    .transpose()
}

pub async fn set_state(state: &AppState, id: &str, to: GcState) -> anyhow::Result<()> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE gc_cycles SET state = $1 WHERE id = $2 AND state NOT IN {FINISHED}"
    )))
    .bind(to.as_str())
    .bind(id)
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

/// Moves the cycle from one state to another; `false` when something else
/// changed it first, such as an abort.
pub async fn transition(
    state: &AppState,
    id: &str,
    from: GcState,
    to: GcState,
) -> anyhow::Result<bool> {
    Ok(
        sqlx::query("UPDATE gc_cycles SET state = $1 WHERE id = $2 AND state = $3")
            .bind(to.as_str())
            .bind(id)
            .bind(from.as_str())
            .execute(&state.db.pool)
            .await?
            .rows_affected()
            == 1,
    )
}

/// Saved before the marker is written, so that a restart knows to look for it.
pub async fn set_gc_id(state: &AppState, id: &str, gc_id: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE gc_cycles SET gc_id = $1 WHERE id = $2")
        .bind(gc_id)
        .bind(id)
        .execute(&state.db.pool)
        .await?;
    Ok(())
}

/// Records the published marker and starts the wait; `false` when the cycle
/// was aborted while it marked.
pub async fn publish_cycle(
    state: &AppState,
    id: &str,
    gc_id: &str,
    published_at: i64,
    delete_after: i64,
    progress: &StoredProgress,
    plan: &GcFullReport,
) -> anyhow::Result<bool> {
    Ok(sqlx::query(
        "UPDATE gc_cycles SET state = 'waiting', gc_id = $1, published_at = $2, \
         delete_after = $3, progress_json = $4, report_json = $5 \
         WHERE id = $6 AND state IN ('marking', 'repacking')",
    )
    .bind(gc_id)
    .bind(published_at)
    .bind(delete_after)
    .bind(serde_json::to_string(progress)?)
    .bind(serde_json::to_string(plan)?)
    .bind(id)
    .execute(&state.db.pool)
    .await?
    .rows_affected()
        == 1)
}

/// Whether a cycle is deleting packs in the repository, or in any
/// repository when it is not known.
pub async fn sweeping(state: &AppState, repository_id: Option<&str>) -> anyhow::Result<bool> {
    let query = match repository_id {
        Some(id) => sqlx::query(
            "SELECT COUNT(*) AS n FROM gc_cycles WHERE state = 'sweeping' AND repository_id = $1",
        )
        .bind(id),
        None => sqlx::query("SELECT COUNT(*) AS n FROM gc_cycles WHERE state = 'sweeping'"),
    };
    let n: i64 = query.fetch_one(&state.db.pool).await?.try_get("n")?;
    Ok(n > 0)
}

/// Marker ids of the repository's cycles that will never go on: done,
/// aborted, or failed before deleting anything.
pub async fn stale_gc_ids(
    state: &AppState,
    repository_id: &str,
) -> anyhow::Result<std::collections::HashSet<String>> {
    sqlx::query(
        "SELECT gc_id FROM gc_cycles WHERE repository_id = $1 AND gc_id IS NOT NULL \
         AND (state IN ('done', 'aborted') OR (state = 'failed' AND error_after_delete = 0))",
    )
    .bind(repository_id)
    .fetch_all(&state.db.pool)
    .await?
    .iter()
    .map(|row| Ok(row.try_get("gc_id")?))
    .collect()
}

/// Moves the end of the pause of a waiting cycle.
pub async fn set_delete_after(state: &AppState, id: &str, until_ms: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE gc_cycles SET delete_after = $1 WHERE id = $2 AND state = 'waiting'")
        .bind(until_ms)
        .bind(id)
        .execute(&state.db.pool)
        .await?;
    Ok(())
}

/// The point of no return: from here on the cycle can only go forward.
pub async fn begin_sweep(
    state: &AppState,
    id: &str,
    resurrected: &[String],
    rewrite: &[String],
) -> anyhow::Result<bool> {
    Ok(sqlx::query(
        "UPDATE gc_cycles SET state = 'sweeping', sweep_started = 1, resurrected_json = $1, \
         rewrite_json = $2 WHERE id = $3 AND state = 'verifying' AND sweep_started = 0",
    )
    .bind(serde_json::to_string(resurrected)?)
    .bind(serde_json::to_string(rewrite)?)
    .bind(id)
    .execute(&state.db.pool)
    .await?
    .rows_affected()
        == 1)
}

/// Aborts a cycle that has not started deleting.
pub async fn abort_cycle(state: &AppState, id: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE gc_cycles SET state = 'aborted', finished_at = $1 \
         WHERE id = $2 AND sweep_started = 0 AND state NOT IN {FINISHED}"
    )))
    .bind(now_ms())
    .bind(id)
    .execute(&state.db.pool)
    .await?
    .rows_affected()
        == 1)
}

/// Puts a cycle that failed while deleting back to `sweeping`.
pub async fn resume_failed(state: &AppState, id: &str) -> Result<bool, ApiError> {
    let resumed = sqlx::query(
        "UPDATE gc_cycles SET state = 'sweeping', error = NULL, error_after_delete = 0, \
         finished_at = NULL WHERE id = $1 AND state = 'failed' AND error_after_delete = 1",
    )
    .bind(id)
    .execute(&state.db.pool)
    .await;
    match resumed {
        Ok(done) => Ok(done.rows_affected() == 1),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err(ApiError::Conflict(
            "A cleanup cycle is already running in this repository.".into(),
        )),
        Err(e) => Err(e.into()),
    }
}

/// The newest cycle that was not a dry run.
pub async fn last_real_cycle(
    state: &AppState,
    repository_id: &str,
) -> anyhow::Result<Option<CycleRow>> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {CYCLE_COLUMNS} FROM gc_cycles WHERE repository_id = $1 AND dry_run = 0 \
         ORDER BY created_at DESC, id DESC LIMIT 1"
    )))
    .bind(repository_id)
    .fetch_optional(&state.db.pool)
    .await?
    .as_ref()
    .map(cycle_from_row)
    .transpose()
}

/// Cycles that real work is left for.
pub async fn unfinished_real_cycles(state: &AppState) -> anyhow::Result<Vec<CycleRow>> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {CYCLE_COLUMNS} FROM gc_cycles WHERE dry_run = 0 AND state NOT IN {FINISHED} \
         ORDER BY created_at"
    )))
    .fetch_all(&state.db.pool)
    .await?
    .iter()
    .map(cycle_from_row)
    .collect()
}

/// Cycles that failed after deleting began, oldest first.
pub async fn stopped_while_deleting(state: &AppState) -> anyhow::Result<Vec<CycleRow>> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {CYCLE_COLUMNS} FROM gc_cycles WHERE dry_run = 0 AND state = 'failed' \
         AND error_after_delete = 1 ORDER BY created_at"
    )))
    .fetch_all(&state.db.pool)
    .await?
    .iter()
    .map(cycle_from_row)
    .collect()
}

pub async fn count_events(state: &AppState, cycle_id: &str, kind: &str) -> anyhow::Result<u32> {
    let n: i64 =
        sqlx::query("SELECT COUNT(*) AS n FROM gc_events WHERE cycle_id = $1 AND kind = $2")
            .bind(cycle_id)
            .bind(kind)
            .fetch_one(&state.db.pool)
            .await?
            .try_get("n")?;
    Ok(u32::try_from(n).unwrap_or(u32::MAX))
}

pub async fn load_plan_report(state: &AppState, id: &str) -> anyhow::Result<Option<GcFullReport>> {
    let row = sqlx::query("SELECT report_json FROM gc_cycles WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db.pool)
        .await?;
    let json: Option<String> = row.map(|r| r.try_get("report_json")).transpose()?.flatten();
    json.map(|json| serde_json::from_str(&json).context("reading the stored cleanup plan"))
        .transpose()
}

pub async fn set_progress(
    state: &AppState,
    id: &str,
    progress: &StoredProgress,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE gc_cycles SET progress_json = $1 WHERE id = $2")
        .bind(serde_json::to_string(progress)?)
        .bind(id)
        .execute(&state.db.pool)
        .await?;
    Ok(())
}

/// Ends the cycle as `done`. The summary is kept apart from the full report
/// so lists and status reads never load the pack listings.
pub async fn finish_cycle(state: &AppState, id: &str, report: &GcFullReport) -> anyhow::Result<()> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE gc_cycles SET state = 'done', summary_json = $1, report_json = $2, \
         finished_at = $3 WHERE id = $4 AND state NOT IN {FINISHED}"
    )))
    .bind(serde_json::to_string(&report.summary)?)
    .bind(serde_json::to_string(report)?)
    .bind(now_ms())
    .bind(id)
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

pub async fn fail_cycle(state: &AppState, id: &str, error: &str) -> anyhow::Result<()> {
    let failed = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE gc_cycles SET state = 'failed', error = $1, finished_at = $2, \
         error_after_delete = sweep_started WHERE id = $3 AND state NOT IN {FINISHED}"
    )))
    .bind(clip(error))
    .bind(now_ms())
    .bind(id)
    .execute(&state.db.pool)
    .await?
    .rows_affected()
        == 1;
    if failed {
        append_event(state, id, "error", "failed", error).await?;
    }
    Ok(())
}

const EVENT_ATTEMPTS: usize = 3;

/// Appends an event with the next sequence number. Two writers that read
/// the same number race on the insert, and SQLite may refuse the write while
/// another one holds the lock, so the loser tries again.
pub async fn append_event(
    state: &AppState,
    cycle_id: &str,
    level: &str,
    kind: &str,
    message: &str,
) -> anyhow::Result<()> {
    let mut last_error = None;
    for _ in 0..EVENT_ATTEMPTS {
        match try_append_event(state, cycle_id, level, kind, message).await {
            Ok(true) => return Ok(()),
            Ok(false) => last_error = None,
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error
        .unwrap_or_else(|| anyhow::anyhow!("another writer kept taking the next event number")))
}

async fn try_append_event(
    state: &AppState,
    cycle_id: &str,
    level: &str,
    kind: &str,
    message: &str,
) -> anyhow::Result<bool> {
    let mut tx = state.db.pool.begin().await?;
    let seq: i64 =
        sqlx::query("SELECT COALESCE(MAX(seq), 0) + 1 AS next FROM gc_events WHERE cycle_id = $1")
            .bind(cycle_id)
            .fetch_one(&mut *tx)
            .await?
            .try_get("next")?;
    let inserted = sqlx::query(
        "INSERT INTO gc_events (cycle_id, seq, at, level, kind, message) \
         VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING",
    )
    .bind(cycle_id)
    .bind(seq)
    .bind(now_ms())
    .bind(level)
    .bind(kind)
    .bind(clip(message))
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    tx.commit().await?;
    Ok(inserted)
}

pub async fn last_report(
    state: &AppState,
    repository_id: &str,
    dry_run: bool,
) -> anyhow::Result<Option<GcReport>> {
    let row = sqlx::query(
        "SELECT summary_json FROM gc_cycles \
         WHERE repository_id = $1 AND dry_run = $2 AND state = 'done' AND summary_json IS NOT NULL \
         ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(repository_id)
    .bind(i64::from(dry_run))
    .fetch_optional(&state.db.pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let json: String = row.try_get("summary_json")?;
    Ok(Some(
        serde_json::from_str(&json).context("reading a stored cleanup report")?,
    ))
}

pub async fn last_aborted_at(state: &AppState, repository_id: &str) -> anyhow::Result<Option<i64>> {
    let row = sqlx::query(
        "SELECT finished_at FROM gc_cycles WHERE repository_id = $1 AND state = 'aborted' \
         ORDER BY finished_at DESC LIMIT 1",
    )
    .bind(repository_id)
    .fetch_optional(&state.db.pool)
    .await?;
    Ok(row
        .map(|r| r.try_get::<Option<i64>, _>("finished_at"))
        .transpose()?
        .flatten())
}

/// The newest events of the `shown` cycle, or else of the repository's
/// latest cycle, oldest first, and how many that cycle has in all.
pub async fn latest_events(
    state: &AppState,
    repository_id: &str,
    shown: Option<&str>,
) -> anyhow::Result<(Vec<GcEventDto>, u64)> {
    let cycle_id: String = match shown {
        Some(id) => id.to_string(),
        None => {
            let cycle = sqlx::query(
                "SELECT id FROM gc_cycles WHERE repository_id = $1 \
                 ORDER BY created_at DESC, id DESC LIMIT 1",
            )
            .bind(repository_id)
            .fetch_optional(&state.db.pool)
            .await?;
            let Some(cycle) = cycle else {
                return Ok((Vec::new(), 0));
            };
            cycle.try_get("id")?
        }
    };
    let total: i64 = sqlx::query("SELECT COUNT(*) AS n FROM gc_events WHERE cycle_id = $1")
        .bind(&cycle_id)
        .fetch_one(&state.db.pool)
        .await?
        .try_get("n")?;
    let mut events = sqlx::query(
        "SELECT seq, at, level, kind, message FROM gc_events WHERE cycle_id = $1 \
         ORDER BY seq DESC LIMIT $2",
    )
    .bind(&cycle_id)
    .bind(EVENTS_SHOWN)
    .fetch_all(&state.db.pool)
    .await?
    .iter()
    .map(|row| {
        Ok(GcEventDto {
            seq: row.try_get("seq")?,
            at: row.try_get("at")?,
            level: row.try_get("level")?,
            kind: row.try_get("kind")?,
            message: row.try_get("message")?,
        })
    })
    .collect::<Result<Vec<_>, sqlx::Error>>()?;
    events.reverse();
    Ok((events, clamp(total)))
}

/// The policy for the walk of the snapshots, as the database and the bucket
/// state it now: read again by every step, so an exclusion made while a cycle
/// runs counts at its next step.
pub(crate) async fn load_mark_policy(
    state: &AppState,
    repository_id: &str,
    source: &OpenRepository,
) -> anyhow::Result<MarkPolicy> {
    let mut policy = MarkPolicy {
        quarantine: true,
        ..MarkPolicy::default()
    };
    let rows = sqlx::query(
        "SELECT snapshot_id FROM gc_quarantine WHERE repository_id = $1 AND confirmed_at IS NOT NULL",
    )
    .bind(repository_id)
    .fetch_all(&state.db.pool)
    .await?;
    for row in rows {
        let id: String = row.try_get("snapshot_id")?;
        policy.excluded.extend(parse_hex32(&id));
    }
    let rows = sqlx::query(
        "SELECT id FROM snapshots WHERE repository_id = $1 \
         AND host_id IS NOT NULL AND job_id IS NOT NULL",
    )
    .bind(repository_id)
    .fetch_all(&state.db.pool)
    .await?;
    for row in rows {
        let id: String = row.try_get("id")?;
        policy.bound.extend(parse_hex32(&id));
    }
    // A snapshot that retention already ordered deleted is not bound to a job,
    // even while its object stays in the bucket under Object Lock.
    let rows = sqlx::query("SELECT id FROM snapshot_tombstones WHERE repository_id = $1")
        .bind(repository_id)
        .fetch_all(&state.db.pool)
        .await?;
    let forgotten: HashSet<BlobId> = rows
        .iter()
        .filter_map(|row| row.try_get::<String, _>("id").ok())
        .filter_map(|id| parse_hex32(&id))
        .collect();
    let attested = read_attestations(&source.store, &source.keys).await?;
    let provenance = ProvenanceSet::from_attested(&attested, &source.keys);
    if !provenance.is_empty() {
        let listed = source.store.list("snapshots/").await?;
        policy.bound.extend(
            listed
                .iter()
                .filter_map(|e| e.key.strip_prefix("snapshots/").and_then(parse_hex32))
                .filter(|id| provenance.signer(id) == Some(Signer::Server)),
        );
    }
    policy.bound.retain(|id| !forgotten.contains(id));
    Ok(policy)
}

/// The snapshots this survey could not walk: the quarantined ones, and those
/// the walk ignored under their own key, which is what an excluded snapshot
/// that still does not open becomes. A snapshot deleted while the walk ran
/// still counts as failing, so its exclusion survives until the next walk,
/// whose listing no longer has it.
pub fn failing_ids(quarantined: &[Quarantined], ignored: &[(String, String)]) -> HashSet<BlobId> {
    quarantined
        .iter()
        .map(|q| q.id)
        .chain(
            ignored
                .iter()
                .filter_map(|(key, _)| key.strip_prefix("snapshots/").and_then(parse_hex32)),
        )
        .collect()
}

/// Replaces the unconfirmed rows of the repository with the snapshots the
/// walk just quarantined. A confirmed row stays while its snapshot still fails
/// (`failing`), also when the walk that reports it started before the
/// confirmation, and while the snapshot is not bound to a job. A confirmed row
/// of a snapshot that read fine or is missing from the walk's listing is
/// dropped when the confirmation predates the start of the walk
/// (`survey_started_ms`): the exclusion lasts as long as the snapshot is
/// broken, a snapshot that breaks again waits for a new confirmation, and a
/// confirmation made while the walk ran is judged by the next walk.
pub async fn replace_quarantine(
    state: &AppState,
    repository_id: &str,
    items: &[Quarantined],
    failing: &HashSet<BlobId>,
    now_ms: i64,
    survey_started_ms: i64,
) -> anyhow::Result<()> {
    let mut tx = state.db.pool.begin().await?;
    sqlx::query("DELETE FROM gc_quarantine WHERE repository_id = $1 AND confirmed_at IS NULL")
        .bind(repository_id)
        .execute(&mut *tx)
        .await?;
    for item in items {
        sqlx::query(
            "INSERT INTO gc_quarantine (repository_id, snapshot_id, object_key, reason, \
             confirmable, detected_at) VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (repository_id, snapshot_id) DO UPDATE SET \
             object_key = excluded.object_key, reason = excluded.reason, \
             confirmable = excluded.confirmable, \
             confirmed_at = CASE WHEN excluded.confirmable = 0 THEN NULL \
             ELSE gc_quarantine.confirmed_at END, \
             confirmed_by = CASE WHEN excluded.confirmable = 0 THEN NULL \
             ELSE gc_quarantine.confirmed_by END",
        )
        .bind(repository_id)
        .bind(hex32(&item.id))
        .bind(&item.key)
        .bind(&item.reason)
        .bind(i64::from(!item.bound))
        .bind(now_ms)
        .execute(&mut *tx)
        .await?;
    }
    let confirmed = sqlx::query(
        "SELECT snapshot_id FROM gc_quarantine WHERE repository_id = $1 \
         AND confirmed_at IS NOT NULL AND confirmed_at < $2",
    )
    .bind(repository_id)
    .bind(survey_started_ms)
    .fetch_all(&mut *tx)
    .await?;
    for row in confirmed {
        let id: String = row.try_get("snapshot_id")?;
        if parse_hex32(&id).is_some_and(|blob| failing.contains(&blob)) {
            continue;
        }
        sqlx::query("DELETE FROM gc_quarantine WHERE repository_id = $1 AND snapshot_id = $2")
            .bind(repository_id)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        tracing::info!(
            repository = repository_id,
            snapshot = %id,
            "exclusion from cleanup lifted: the snapshot reads again or is gone"
        );
    }
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn list_quarantine(
    state: &AppState,
    repository_id: &str,
) -> anyhow::Result<Vec<GcQuarantineDto>> {
    let rows = sqlx::query(
        "SELECT snapshot_id, object_key, reason, confirmable, detected_at, confirmed_at, \
         confirmed_by FROM gc_quarantine WHERE repository_id = $1 \
         ORDER BY detected_at, snapshot_id",
    )
    .bind(repository_id)
    .fetch_all(&state.db.pool)
    .await?;
    rows.iter()
        .map(|row| {
            Ok(GcQuarantineDto {
                snapshot_id: row.try_get("snapshot_id")?,
                key: row.try_get("object_key")?,
                reason: row.try_get("reason")?,
                confirmable: row.try_get::<i64, _>("confirmable")? != 0,
                detected_at: row.try_get("detected_at")?,
                confirmed_at: row.try_get("confirmed_at")?,
                confirmed_by: row.try_get("confirmed_by")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map_err(Into::into)
}

pub(crate) enum Excluded {
    Done,
    Missing,
    Bound,
    Already,
}

/// Confirms a quarantined snapshot as excluded from cleanup. Only the
/// database changes; the snapshot object is left alone.
pub(crate) async fn exclude_from_gc(
    state: &AppState,
    repository_id: &str,
    snapshot_id: &BlobId,
    actor: &str,
    now_ms: i64,
) -> anyhow::Result<Excluded> {
    let id = hex32(snapshot_id);
    let changed = sqlx::query(
        "UPDATE gc_quarantine SET confirmed_at = $1, confirmed_by = $2 \
         WHERE repository_id = $3 AND snapshot_id = $4 AND confirmed_at IS NULL \
         AND confirmable = 1",
    )
    .bind(now_ms)
    .bind(actor)
    .bind(repository_id)
    .bind(&id)
    .execute(&state.db.pool)
    .await?
    .rows_affected();
    if changed == 1 {
        return Ok(Excluded::Done);
    }
    let row = sqlx::query(
        "SELECT confirmable FROM gc_quarantine WHERE repository_id = $1 AND snapshot_id = $2",
    )
    .bind(repository_id)
    .bind(&id)
    .fetch_optional(&state.db.pool)
    .await?;
    Ok(match row {
        None => Excluded::Missing,
        Some(row) if row.try_get::<i64, _>("confirmable")? == 0 => Excluded::Bound,
        Some(_) => Excluded::Already,
    })
}
