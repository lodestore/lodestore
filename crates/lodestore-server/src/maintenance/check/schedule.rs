//! Scheduled checks: one run for the latest cron slot of every enabled level
//! of every repository.

use std::collections::HashMap;

use lodestore_policy::Schedule;
use lodestore_policy::jiff::Timestamp;
use sqlx::Row;

use super::{CheckLevel, CheckLevelSettings, CheckOrigin, db, start_check};
use crate::error::ApiError;
use crate::state::AppState;

const ACTOR: &str = "schedule";

/// Starts the checks whose slot has come and returns the run ids. After
/// settings change, or when a level is first seen, the cursor is set past
/// the latest slot, so nothing starts at once; slots missed while the server
/// was down start one run. A slot that meets another check or a cleanup that
/// deletes keeps its cursor and tries again on the next tick. A repository
/// that never had a structure check gets one right away; one whose first check
/// ended any way, cancelled included, waits for its slot like the others.
pub async fn tick(state: &AppState, now: Timestamp) -> anyhow::Result<Vec<String>> {
    let repositories = sqlx::query("SELECT id FROM repositories ORDER BY created_at, id")
        .fetch_all(&state.db.pool)
        .await?;
    let mut started = Vec::new();
    for row in &repositories {
        let repository_id: String = row.try_get("id")?;
        let rows = sqlx::query(
            "SELECT level, last_slot_ms, updated_at FROM check_settings WHERE repository_id = $1",
        )
        .bind(&repository_id)
        .fetch_all(&state.db.pool)
        .await?;
        let mut cursors = HashMap::new();
        for row in &rows {
            cursors.insert(
                row.try_get::<String, _>("level")?,
                (
                    row.try_get::<Option<i64>, _>("last_slot_ms")?,
                    row.try_get::<i64, _>("updated_at")?,
                ),
            );
        }
        for (level, settings) in db::load_settings(state, &repository_id).await? {
            let (cursor, updated_at) = cursors.get(level.as_str()).copied().unwrap_or((None, 0));
            let due = Due {
                repository_id: &repository_id,
                level,
                settings: &settings,
                cursor,
                updated_at,
            };
            match slot(state, now, &due).await {
                Ok(Some(id)) => started.push(id),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        %repository_id, level = level.as_str(), %error,
                        "scheduled check failed to start"
                    );
                }
            }
        }
    }
    Ok(started)
}

struct Due<'a> {
    repository_id: &'a str,
    level: CheckLevel,
    settings: &'a CheckLevelSettings,
    cursor: Option<i64>,
    updated_at: i64,
}

async fn slot(state: &AppState, now: Timestamp, due: &Due<'_>) -> anyhow::Result<Option<String>> {
    let Due {
        repository_id,
        level,
        settings,
        cursor,
        updated_at,
    } = *due;
    if !settings.enabled || settings.cron.trim().is_empty() {
        return Ok(None);
    }
    let schedule = Schedule {
        cron: settings.cron.clone(),
        timezone: Some(settings.timezone.clone()),
        jitter: None,
        catch_up_window: None,
    };
    let compiled = match schedule.compile() {
        Ok(compiled) => compiled,
        Err(problem) => {
            tracing::warn!(
                %repository_id, level = level.as_str(), ?problem,
                "stored check schedule is invalid"
            );
            return Ok(None);
        }
    };
    let latest_ms = compiled.previous_slot(now).map(Timestamp::as_millisecond);
    let first_structure =
        level == CheckLevel::Structure && !db::has_run(state, repository_id, level).await?;
    if first_structure {
        let id = try_start(state, repository_id, level).await?;
        if id.is_some() {
            let past = latest_ms.unwrap_or(0).max(updated_at);
            store_cursor(state, repository_id, level, past).await?;
        }
        return Ok(id);
    }
    let Some(latest_ms) = latest_ms else {
        return Ok(None);
    };
    let cursor = match cursor {
        Some(cursor) if cursor >= updated_at => cursor,
        _ => {
            store_cursor(state, repository_id, level, latest_ms.max(updated_at)).await?;
            return Ok(None);
        }
    };
    if latest_ms <= cursor {
        return Ok(None);
    }
    let id = try_start(state, repository_id, level).await?;
    if id.is_some() {
        store_cursor(state, repository_id, level, latest_ms).await?;
    }
    Ok(id)
}

async fn try_start(
    state: &AppState,
    repository_id: &str,
    level: CheckLevel,
) -> anyhow::Result<Option<String>> {
    match start_check(state, repository_id, level, CheckOrigin::Schedule, ACTOR).await {
        Ok(id) => Ok(Some(id)),
        Err(ApiError::Conflict(_)) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

async fn store_cursor(
    state: &AppState,
    repository_id: &str,
    level: CheckLevel,
    slot_ms: i64,
) -> anyhow::Result<()> {
    let defaults = CheckLevelSettings::defaults(level);
    sqlx::query(
        "INSERT INTO check_settings (repository_id, level, enabled, cron, timezone, \
         last_slot_ms, updated_at) VALUES ($1, $2, 1, $3, $4, $5, 0) \
         ON CONFLICT (repository_id, level) DO UPDATE SET last_slot_ms = excluded.last_slot_ms",
    )
    .bind(repository_id)
    .bind(level.as_str())
    .bind(&defaults.cron)
    .bind(&defaults.timezone)
    .bind(slot_ms)
    .execute(&state.db.pool)
    .await?;
    Ok(())
}
