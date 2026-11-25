//! The daily recheck of every repository's storage guarantee: the same full
//! probe as the manual one, so a weakened protection is noticed without a human.

use std::time::Duration;

use sqlx::Row;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::db::now_ms;
use crate::error::ApiError;
use crate::repos::agent_key::recheck_repository_at;
use crate::repos::db;
use crate::state::AppState;

pub const RECHECK_AFTER_MS: i64 = 24 * 3_600_000;
const RUN_EVERY: Duration = Duration::from_secs(3600);
/// The first pass waits, so a starting server does not probe every repository at once.
const START_DELAY: Duration = Duration::from_secs(600);
const RECHECK_TIMEOUT: Duration = Duration::from_secs(120);

/// A repository is due when the later of its last written level and its last
/// daily attempt is a day old or missing. Both marks are written by the
/// guarantee code only; a manual check that measured nothing moves neither.
/// A mark in the future was written by a wrong clock and does not count.
fn due(now_ms: i64, tick_at: Option<i64>, guarantee_checked_at: Option<i64>) -> bool {
    tick_at
        .max(guarantee_checked_at)
        .is_none_or(|last| last > now_ms || now_ms - last >= RECHECK_AFTER_MS)
}

#[derive(Debug)]
pub enum Outcome<T> {
    Done(T),
    Panicked,
    TimedOut,
    Stopped,
}

/// Waits for `work` for at most `timeout`; a timeout and a stop cancel it.
pub async fn guarded<T: Send + 'static>(
    mut work: JoinHandle<T>,
    timeout: Duration,
    stop: &CancellationToken,
) -> Outcome<T> {
    tokio::select! {
        done = &mut work => match done {
            Ok(value) => Outcome::Done(value),
            Err(_) => Outcome::Panicked,
        },
        () = tokio::time::sleep(timeout) => {
            work.abort();
            Outcome::TimedOut
        }
        () = stop.cancelled() => {
            work.abort();
            Outcome::Stopped
        }
    }
}

/// Rechecks the repositories that are due, one at a time, and returns their ids.
/// A failure of one is logged and does not stop the others.
pub async fn tick(
    state: &AppState,
    now_ms: i64,
    stop: &CancellationToken,
) -> anyhow::Result<Vec<String>> {
    let rows = sqlx::query(
        "SELECT id, guarantee_checked_at, guarantee_tick_at FROM repositories \
         ORDER BY created_at, id",
    )
    .fetch_all(&state.db.pool)
    .await?;
    let mut checked = Vec::new();
    for row in &rows {
        if stop.is_cancelled() {
            break;
        }
        let id: String = row.try_get("id")?;
        let measured: Option<i64> = row.try_get("guarantee_checked_at")?;
        let tick_at: Option<i64> = row.try_get("guarantee_tick_at")?;
        if !due(now_ms, tick_at, measured) {
            continue;
        }
        let work = tokio::spawn({
            let (state, id) = (state.clone(), id.clone());
            async move { recheck_repository_at(&state, &id, now_ms, true).await }
        });
        let outcome = guarded(work, RECHECK_TIMEOUT, stop).await;
        let failed = match outcome {
            Outcome::Done(Ok(_)) => None,
            Outcome::Done(Err(ApiError::NotFound(_))) => {
                tracing::debug!(repository_id = %id, "the repository was deleted during the daily recheck");
                continue;
            }
            Outcome::Done(Err(ApiError::Conflict(reason))) => {
                tracing::debug!(repository_id = %id, %reason, "the daily recheck result was discarded");
                None
            }
            Outcome::Done(Err(error)) => {
                tracing::warn!(repository_id = %id, %error, "the daily recheck failed");
                Some(true)
            }
            Outcome::Panicked => {
                tracing::warn!(repository_id = %id, "the daily recheck panicked");
                Some(true)
            }
            Outcome::TimedOut => {
                tracing::warn!(repository_id = %id, "the daily recheck timed out");
                Some(true)
            }
            Outcome::Stopped => Some(false),
        };
        if let Some(failed) = failed
            && let Err(error) = db::record_recheck_attempt(state, &id, now_ms, failed).await
        {
            tracing::warn!(repository_id = %id, error = %format!("{error:#}"), "recording the daily recheck attempt failed");
        }
        checked.push(id);
    }
    Ok(checked)
}

pub async fn run(state: AppState, stop: CancellationToken) -> anyhow::Result<()> {
    let mut interval =
        tokio::time::interval_at(tokio::time::Instant::now() + START_DELAY, RUN_EVERY);
    loop {
        tokio::select! {
            () = stop.cancelled() => return Ok(()),
            _ = interval.tick() => {}
        }
        if let Err(error) = tick(&state, now_ms(), &stop).await {
            tracing::warn!(error = %format!("{error:#}"), "the daily guarantee recheck failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::pending;

    use super::*;

    const HOUR: i64 = 3_600_000;

    #[test]
    fn due_follows_the_last_check() {
        assert!(due(0, None, None));
        assert!(!due(23 * HOUR, Some(0), None));
        assert!(!due(23 * HOUR, None, Some(0)));
        assert!(due(24 * HOUR, Some(0), None));
        assert!(due(30 * HOUR, Some(0), Some(0)));
        assert!(!due(30 * HOUR, Some(10 * HOUR), Some(0)));
        assert!(!due(30 * HOUR, Some(0), Some(10 * HOUR)));
        assert!(due(30 * HOUR, Some(31 * HOUR), None));
        assert!(due(30 * HOUR, None, Some(31 * HOUR)));
    }

    async fn dropped(rx: tokio::sync::oneshot::Receiver<()>) -> bool {
        tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("the task was not dropped")
            .is_err()
    }

    #[tokio::test]
    async fn guarded_counts_a_panic_a_timeout_and_a_stop() {
        let stop = CancellationToken::new();
        let long = Duration::from_secs(60);

        let done = guarded(tokio::spawn(async { 7 }), long, &stop).await;
        assert!(matches!(done, Outcome::Done(7)));

        let panicked = guarded(
            tokio::spawn(async {
                panic!("boom");
            }),
            long,
            &stop,
        )
        .await;
        assert!(matches!(panicked, Outcome::Panicked));

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let hung = tokio::spawn(async move {
            let _tx = tx;
            pending::<()>().await
        });
        let timed_out = guarded(hung, Duration::from_millis(50), &stop).await;
        assert!(matches!(timed_out, Outcome::TimedOut));
        assert!(dropped(rx).await);

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let hung = tokio::spawn(async move {
            let _tx = tx;
            pending::<()>().await
        });
        stop.cancel();
        let stopped = guarded(hung, long, &stop).await;
        assert!(matches!(stopped, Outcome::Stopped));
        assert!(dropped(rx).await);
    }
}
