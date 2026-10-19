//! Scheduled cleanup: one cycle for the latest cron slot of each repository
//! that has cleanup turned on.

use lodestore_policy::Schedule;
use lodestore_policy::jiff::Timestamp;
use sqlx::Row;
use sqlx::any::AnyRow;

use super::api::GcFailure;
use super::preflight::PreflightProblem;
use super::{GcOrigin, db, start_cycle, start_real_cycle};
use crate::state::AppState;

const ACTOR: &str = "schedule";

/// Starts or continues the cycle of every repository whose slot has come
/// and returns their ids. After settings change, or when cleanup is first
/// turned on, the cursor is set past the latest slot, so nothing starts at
/// once; slots missed while the server was down start one cycle.
pub async fn tick(state: &AppState, now: Timestamp) -> anyhow::Result<Vec<String>> {
    let rows = sqlx::query(
        "SELECT repository_id, cron, timezone, last_slot_ms, updated_at \
         FROM gc_settings WHERE enabled = 1 ORDER BY repository_id",
    )
    .fetch_all(&state.db.pool)
    .await?;
    let mut started = Vec::new();
    for row in &rows {
        let repository_id: String = row.try_get("repository_id")?;
        match slot(state, now, &repository_id, row).await {
            Ok(Some(id)) => started.push(id),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%repository_id, %error, "scheduled cleanup failed to start");
            }
        }
    }
    Ok(started)
}

async fn slot(
    state: &AppState,
    now: Timestamp,
    repository_id: &str,
    row: &AnyRow,
) -> anyhow::Result<Option<String>> {
    let cron: String = row.try_get("cron")?;
    if cron.trim().is_empty() {
        return Ok(None);
    }
    let schedule = Schedule {
        cron,
        timezone: Some(row.try_get("timezone")?),
        jitter: None,
        catch_up_window: None,
    };
    let compiled = match schedule.compile() {
        Ok(compiled) => compiled,
        Err(problem) => {
            tracing::warn!(%repository_id, ?problem, "stored cleanup schedule is invalid");
            return Ok(None);
        }
    };
    let Some(latest) = compiled.previous_slot(now) else {
        return Ok(None);
    };
    let latest_ms = latest.as_millisecond();
    let updated_at: i64 = row.try_get("updated_at")?;
    let cursor = match row.try_get::<Option<i64>, _>("last_slot_ms")? {
        Some(cursor) if cursor >= updated_at => cursor,
        _ => {
            store_cursor(state, repository_id, latest_ms.max(updated_at)).await?;
            return Ok(None);
        }
    };
    if latest_ms <= cursor {
        return Ok(None);
    }
    if db::active_cycle_of(state, repository_id, false)
        .await?
        .is_some()
    {
        store_cursor(state, repository_id, latest_ms).await?;
        return Ok(None);
    }
    let id = match start_real_cycle(state, repository_id, GcOrigin::Schedule, ACTOR).await {
        Ok(id) => id,
        Err(GcFailure::Preflight(problems)) => {
            record_refused(state, repository_id, &problems).await?
        }
        Err(GcFailure::Api(error)) => return Err(error.into()),
        Err(GcFailure::Fields(_)) => anyhow::bail!("the stored cleanup settings are invalid"),
    };
    store_cursor(state, repository_id, latest_ms).await?;
    Ok(Some(id))
}

async fn store_cursor(state: &AppState, repository_id: &str, slot_ms: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE gc_settings SET last_slot_ms = $1 WHERE repository_id = $2")
        .bind(slot_ms)
        .bind(repository_id)
        .execute(&state.db.pool)
        .await?;
    Ok(())
}

/// A slot that can't run leaves a failed cycle behind, so the jobs list says
/// why the weekly cleanup did not happen.
async fn record_refused(
    state: &AppState,
    repository_id: &str,
    problems: &[PreflightProblem],
) -> anyhow::Result<String> {
    let reasons: Vec<String> = problems
        .iter()
        .map(|problem| {
            let hosts: Vec<&str> = problem.hosts.iter().map(|h| h.hostname.as_str()).collect();
            if hosts.is_empty() {
                problem.message.clone()
            } else {
                format!("{} ({})", problem.message, hosts.join(", "))
            }
        })
        .collect();
    let id = start_cycle(state, repository_id, false, GcOrigin::Schedule, ACTOR).await?;
    db::fail_cycle(
        state,
        &id,
        &format!("Scheduled cleanup did not start. {}", reasons.join(" ")),
    )
    .await?;
    Ok(id)
}
