pub mod api;
pub mod evaluate;
pub mod inputs;
pub mod model;
pub mod reconcile;
pub mod text;

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Context;
use sqlx::Row;
use tokio_util::sync::CancellationToken;

use crate::db::now_ms;
use crate::state::AppState;

pub async fn tick(state: &AppState, now_ms: i64) -> anyhow::Result<reconcile::Reconciled> {
    let thresholds = model::load_thresholds(state).await?;
    let inputs = inputs::gather(state, now_ms).await?;
    let candidates = evaluate::evaluate(&inputs, now_ms, state.started_at_ms, &thresholds);
    let unknown = reconcile::Unknown {
        server_cert: inputs.server_cert.is_none(),
        policies: inputs.unreadable_policies.iter().cloned().collect(),
    };
    reconcile::reconcile(state, candidates, &unknown, now_ms).await
}

pub async fn run(state: AppState, stop: CancellationToken) -> anyhow::Result<()> {
    let period = Duration::from_secs(state.config.schedule_tick_secs.max(1));
    let mut interval = tokio::time::interval(period);
    loop {
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            _ = interval.tick() => {}
        }
        if let Err(error) = tick(&state, now_ms()).await {
            tracing::warn!(%error, "alert evaluation failed");
        }
    }
}

pub async fn silences(
    state: &AppState,
    now_ms: i64,
) -> anyhow::Result<HashMap<String, Option<i64>>> {
    let rows = sqlx::query(
        "SELECT host_id, until_ms FROM alert_silences WHERE until_ms IS NULL OR until_ms > $1",
    )
    .bind(now_ms)
    .fetch_all(&state.db.pool)
    .await
    .context("reading alert silences")?;
    rows.iter()
        .map(|row| Ok((row.try_get("host_id")?, row.try_get("until_ms")?)))
        .collect()
}

pub async fn is_silenced(state: &AppState, host_id: &str, now_ms: i64) -> anyhow::Result<bool> {
    let row = sqlx::query(
        "SELECT host_id FROM alert_silences \
         WHERE host_id = $1 AND (until_ms IS NULL OR until_ms > $2)",
    )
    .bind(host_id)
    .bind(now_ms)
    .fetch_optional(&state.db.pool)
    .await
    .context("reading an alert silence")?;
    Ok(row.is_some())
}
