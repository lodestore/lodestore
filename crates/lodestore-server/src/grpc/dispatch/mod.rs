pub mod live;
pub mod messages;

use std::collections::HashSet;
use std::time::Duration;

use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{JobAssign, JobCancel, JobSpec, ServerMessage};
use prost::Message;
use sqlx::Row;
use tokio_util::sync::CancellationToken;

use crate::db::now_ms;
use crate::state::AppState;

const TICK: Duration = Duration::from_secs(1);
const JANITOR_EVERY: Duration = Duration::from_secs(60);
const PROGRESS_FLUSH: Duration = Duration::from_secs(10);
const QUEUED_TTL_MS: i64 = 24 * 3600 * 1000;
const UNACCEPTED_TTL_MS: i64 = 2 * 60 * 1000;
const RUNNING_OFFLINE_TTL_MS: i64 = 24 * 3600 * 1000;
const LOG_TTL_MS: i64 = 30 * 24 * 3600 * 1000;
const JOB_TTL_MS: i64 = 90 * 24 * 3600 * 1000;

fn server_message(body: ServerBody) -> ServerMessage {
    ServerMessage { body: Some(body) }
}

pub async fn run(state: AppState, stop: CancellationToken) -> anyhow::Result<()> {
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_janitor = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            _ = tick.tick() => {}
        }
        if let Err(error) = assign_queued(&state, None).await {
            tracing::warn!(%error, "assigning queued jobs failed");
        }
        if let Err(error) = push_cancels(&state).await {
            tracing::warn!(%error, "sending job cancels failed");
        }
        flush_progress(&state).await;
        if last_janitor.elapsed() >= JANITOR_EVERY {
            last_janitor = tokio::time::Instant::now();
            if let Err(error) = janitor_once(&state, now_ms()).await {
                tracing::warn!(%error, "job janitor failed");
            }
        }
    }
}

pub async fn assign_queued(state: &AppState, only_agent: Option<&str>) -> anyhow::Result<()> {
    let rows =
        match only_agent {
            Some(agent) => sqlx::query(
                "SELECT id, host_id, spec FROM jobs WHERE state = 'queued' AND source = 'manual' \
                 AND host_id = $1 ORDER BY queued_at, id",
            )
            .bind(agent)
            .fetch_all(&state.db.pool)
            .await?,
            None => sqlx::query(
                "SELECT id, host_id, spec FROM jobs WHERE state = 'queued' AND source = 'manual' \
                 ORDER BY queued_at, id",
            )
            .fetch_all(&state.db.pool)
            .await?,
        };
    let mut seen = HashSet::new();
    for row in rows {
        let job_id: String = row.try_get("id")?;
        let host_id: String = row.try_get("host_id")?;
        if !seen.insert(host_id.clone())
            || !state.registry.is_online(&host_id)
            || !state.registry.live.is_ready(&host_id)
        {
            continue;
        }
        let Ok(spec) = JobSpec::decode(row.try_get::<Vec<u8>, _>("spec")?.as_slice()) else {
            fail_unreadable(state, &job_id).await?;
            continue;
        };
        let claimed = sqlx::query(
            "UPDATE jobs SET state = 'assigned', assigned_at = $1 WHERE id = $2 AND state = 'queued' \
             AND NOT EXISTS (SELECT 1 FROM jobs WHERE host_id = $3 AND source = 'manual' \
             AND state IN ('assigned', 'running'))",
        )
        .bind(now_ms())
        .bind(&job_id)
        .bind(&host_id)
        .execute(&state.db.pool)
        .await;
        match claimed {
            Ok(done) if done.rows_affected() == 1 => {}
            Ok(_) => continue,
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => continue,
            Err(error) => return Err(error.into()),
        }
        let sent = state.registry.send(
            &host_id,
            server_message(ServerBody::JobAssign(JobAssign {
                job_id: job_id.clone(),
                spec: Some(spec),
            })),
        );
        if !sent {
            requeue_assigned(state, &job_id, &host_id).await?;
        }
    }
    Ok(())
}

pub async fn requeue_assigned(state: &AppState, job_id: &str, host_id: &str) -> anyhow::Result<()> {
    let cancelled = sqlx::query(
        "UPDATE jobs SET state = 'cancelled', finished_at = $1, assigned_at = NULL \
         WHERE id = $2 AND host_id = $3 AND state = 'assigned' AND cancel_requested = 1",
    )
    .bind(now_ms())
    .bind(job_id)
    .bind(host_id)
    .execute(&state.db.pool)
    .await?
    .rows_affected();
    if cancelled == 0 {
        sqlx::query(
            "UPDATE jobs SET state = 'queued', assigned_at = NULL \
             WHERE id = $1 AND host_id = $2 AND state = 'assigned'",
        )
        .bind(job_id)
        .bind(host_id)
        .execute(&state.db.pool)
        .await?;
    }
    state.registry.live.cancel_unsent(job_id);
    Ok(())
}

async fn fail_unreadable(state: &AppState, job_id: &str) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE jobs SET state = 'failed', finished_at = $1, error_code = 'backup_failed', \
         error_message = 'The job spec could not be read.' WHERE id = $2 AND state = 'queued'",
    )
    .bind(now_ms())
    .bind(job_id)
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

async fn push_cancels(state: &AppState) -> anyhow::Result<()> {
    let rows = sqlx::query(
        "SELECT id, host_id FROM jobs WHERE cancel_requested = 1 AND state IN ('assigned', 'running')",
    )
    .fetch_all(&state.db.pool)
    .await?;
    for row in rows {
        let job_id: String = row.try_get("id")?;
        let host_id: String = row.try_get("host_id")?;
        if !state.registry.is_online(&host_id) || !state.registry.live.cancel_is_new(&job_id) {
            continue;
        }
        let sent = state.registry.send(
            &host_id,
            server_message(ServerBody::JobCancel(JobCancel {
                job_id: job_id.clone(),
            })),
        );
        if !sent {
            state.registry.live.cancel_unsent(&job_id);
        }
    }
    Ok(())
}

async fn flush_progress(state: &AppState) {
    for (job_id, progress) in state.registry.live.take_due(PROGRESS_FLUSH) {
        let Ok(json) = serde_json::to_string(&progress) else {
            continue;
        };
        let result = sqlx::query(
            "UPDATE jobs SET progress_json = $1 WHERE id = $2 AND state IN ('assigned', 'running')",
        )
        .bind(json)
        .bind(&job_id)
        .execute(&state.db.pool)
        .await;
        if let Err(error) = result {
            tracing::warn!(%error, job_id, "could not store job progress");
        }
    }
}

pub async fn janitor_once(state: &AppState, now_ms: i64) -> anyhow::Result<()> {
    let pool = &state.db.pool;
    sqlx::query(
        "UPDATE jobs SET state = 'not_started', finished_at = $1, error_code = 'not_started', \
         error_message = 'The job was not started within 24 hours.' \
         WHERE state = 'queued' AND queued_at < $2",
    )
    .bind(now_ms)
    .bind(now_ms - QUEUED_TTL_MS)
    .execute(pool)
    .await?;

    let stale =
        sqlx::query("SELECT id, host_id FROM jobs WHERE state = 'assigned' AND assigned_at < $1")
            .bind(now_ms - UNACCEPTED_TTL_MS)
            .fetch_all(pool)
            .await?;
    for row in stale {
        let host_id: String = row.try_get("host_id")?;
        requeue_assigned(state, &row.try_get::<String, _>("id")?, &host_id).await?;
    }

    let lost = sqlx::query(
        "SELECT j.id, j.host_id FROM jobs j JOIN agents a ON a.id = j.host_id \
         WHERE j.state = 'running' AND (a.last_seen_at IS NULL OR a.last_seen_at < $1)",
    )
    .bind(now_ms - RUNNING_OFFLINE_TTL_MS)
    .fetch_all(pool)
    .await?;
    for row in lost {
        let host_id: String = row.try_get("host_id")?;
        if !state.registry.is_online(&host_id) {
            messages::interrupt(state, &host_id, &row.try_get::<String, _>("id")?).await?;
        }
    }

    sqlx::query(
        "DELETE FROM job_events WHERE job_id IN \
         (SELECT id FROM jobs WHERE finished_at < $1 AND logs_purged = 0)",
    )
    .bind(now_ms - LOG_TTL_MS)
    .execute(pool)
    .await?;
    sqlx::query("UPDATE jobs SET logs_purged = 1 WHERE finished_at < $1 AND logs_purged = 0")
        .bind(now_ms - LOG_TTL_MS)
        .execute(pool)
        .await?;

    sqlx::query(
        "DELETE FROM job_events WHERE job_id IN (SELECT id FROM jobs WHERE finished_at < $1)",
    )
    .bind(now_ms - JOB_TTL_MS)
    .execute(pool)
    .await?;
    sqlx::query("DELETE FROM jobs WHERE finished_at < $1")
        .bind(now_ms - JOB_TTL_MS)
        .execute(pool)
        .await?;
    Ok(())
}
