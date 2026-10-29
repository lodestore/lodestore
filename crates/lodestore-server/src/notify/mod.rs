pub mod api;
pub mod channels;
pub mod email;
pub mod message;
pub mod net;
pub mod slack;
pub mod telegram;
pub mod webhook;

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::time::{Duration, Instant};

use anyhow::Context;
use sqlx::Row;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::alerts::model::AlertType;
use crate::config::ConfigChannel;
use crate::db::now_ms;
use crate::state::AppState;
use channels::{Channel, ChannelKind, ChannelSource, load_channel, split_spec};
use message::{Message, MessageKind};

pub const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(30 * 60),
];
pub const MAX_ATTEMPTS: i64 = 4;

const BATCH: i64 = 100;
const PASS_BUDGET: Duration = Duration::from_secs(120);
const KEEP_SENT_MS: i64 = 30 * 86_400_000;
const KEEP_HISTORY_MS: i64 = 90 * 86_400_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryError(String);

impl DeliveryError {
    pub fn new(text: impl Into<String>) -> DeliveryError {
        DeliveryError(text.into())
    }
}

impl fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DeliveryError {}

pub async fn send(
    state: &AppState,
    channel: &Channel,
    message: &Message,
) -> Result<(), DeliveryError> {
    match channel.kind {
        ChannelKind::Webhook => webhook::send(state, channel, message).await,
        ChannelKind::Telegram => telegram::send(state, channel, message).await,
        ChannelKind::Slack => slack::send(state, channel, message).await,
        ChannelKind::Email => email::send(state, channel, message).await,
    }
}

pub async fn send_test(state: &AppState, channel: &Channel) -> Result<(), DeliveryError> {
    send(
        state,
        channel,
        &message::test_message(&state.config.public_url),
    )
    .await
}

pub async fn sync_config_channels(state: &AppState) -> anyhow::Result<()> {
    let now = now_ms();
    let mut tx = state
        .db
        .pool
        .begin()
        .await
        .context("starting a transaction")?;
    let mut names = HashSet::new();
    for channel in &state.config.notifications.channels {
        names.insert(channel.name.clone());
        upsert_config_channel(&mut tx, channel, now).await?;
    }
    let rows = sqlx::query("SELECT id, name FROM notification_channels WHERE source = 'config'")
        .fetch_all(&mut *tx)
        .await
        .context("reading the config notification channels")?;
    for row in &rows {
        let name: String = row.try_get("name")?;
        if !names.contains(&name) {
            sqlx::query("DELETE FROM notification_channels WHERE id = $1")
                .bind(row.try_get::<String, _>("id")?)
                .execute(&mut *tx)
                .await
                .context("removing a notification channel")?;
        }
    }
    tx.commit()
        .await
        .context("committing notification channels")
}

async fn upsert_config_channel(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    channel: &ConfigChannel,
    now: i64,
) -> anyhow::Result<()> {
    let (kind, settings, _) = split_spec(&channel.kind);
    let types = serde_json::to_string(&channel.alerts)?;
    let settings = serde_json::to_string(&settings)?;
    let existing = sqlx::query("SELECT id, source FROM notification_channels WHERE name = $1")
        .bind(&channel.name)
        .fetch_optional(&mut **tx)
        .await
        .context("reading a notification channel")?;
    match existing {
        Some(row) => {
            let source: String = row.try_get("source")?;
            anyhow::ensure!(
                ChannelSource::parse(&source) == Some(ChannelSource::Config),
                "notification channel {} from the server config clashes with a channel created in the UI",
                channel.name
            );
            sqlx::query(
                "UPDATE notification_channels SET kind = $1, enabled = $2, types_json = $3, \
                 config_json = $4, updated_at = $5 WHERE id = $6",
            )
            .bind(kind.as_str())
            .bind(i64::from(channel.enabled))
            .bind(&types)
            .bind(&settings)
            .bind(now)
            .bind(row.try_get::<String, _>("id")?)
            .execute(&mut **tx)
            .await
            .context("updating a notification channel")?;
        }
        None => {
            sqlx::query(
                "INSERT INTO notification_channels (id, name, kind, source, enabled, types_json, \
                 config_json, created_at, updated_at) VALUES ($1, $2, $3, 'config', $4, $5, $6, $7, $7)",
            )
            .bind(uuid::Uuid::now_v7().to_string())
            .bind(&channel.name)
            .bind(kind.as_str())
            .bind(i64::from(channel.enabled))
            .bind(&types)
            .bind(&settings)
            .bind(now)
            .execute(&mut **tx)
            .await
            .context("inserting a notification channel")?;
        }
    }
    Ok(())
}

pub async fn tick(state: &AppState, now_ms: i64) -> anyhow::Result<()> {
    tick_until(state, now_ms, &CancellationToken::new()).await
}

async fn tick_until(state: &AppState, now_ms: i64, stop: &CancellationToken) -> anyhow::Result<()> {
    let queued = fan_out(state, now_ms).await;
    let delivered = deliver(state, now_ms, stop).await;
    queued.and(delivered)
}

pub async fn run(state: AppState, stop: CancellationToken) -> anyhow::Result<()> {
    let period = Duration::from_secs(state.config.schedule_tick_secs.max(1));
    let mut interval = tokio::time::interval(period);
    loop {
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            _ = interval.tick() => {}
        }
        if let Err(error) = tick_until(&state, now_ms(), &stop).await {
            tracing::warn!(%error, "notification delivery failed");
        }
    }
}

async fn fan_out(state: &AppState, now_ms: i64) -> anyhow::Result<()> {
    let rows = sqlx::query("SELECT id, types_json FROM notification_channels WHERE enabled = 1")
        .fetch_all(&state.db.pool)
        .await
        .context("reading notification channels")?;
    let mut channels: Vec<(String, Vec<AlertType>)> = Vec::new();
    for row in &rows {
        let id: String = row.try_get("id")?;
        let types: String = row.try_get("types_json")?;
        match serde_json::from_str(&types) {
            Ok(types) => channels.push((id, types)),
            Err(error) => {
                tracing::warn!(channel = %id, %error, "skipping a channel with unreadable alert types");
            }
        }
    }

    let mut tx = state
        .db
        .pool
        .begin()
        .await
        .context("starting a transaction")?;
    let events = sqlx::query(
        "SELECT e.id, a.type AS kind FROM alert_events e JOIN alerts a ON a.id = e.alert_id \
         WHERE e.fanned_out_at IS NULL ORDER BY e.at, e.id",
    )
    .fetch_all(&mut *tx)
    .await
    .context("reading alert events")?;
    for event in &events {
        let id: String = event.try_get("id")?;
        let kind: String = event.try_get("kind")?;
        let kind = AlertType::parse(&kind);
        for (channel_id, types) in &channels {
            if !kind.is_some_and(|k| types.contains(&k)) {
                continue;
            }
            sqlx::query(
                "INSERT INTO alert_deliveries (id, event_id, channel_id, state, attempts, \
                 next_attempt_at, created_at) VALUES ($1, $2, $3, 'pending', 0, $4, $4) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(uuid::Uuid::now_v7().to_string())
            .bind(&id)
            .bind(channel_id)
            .bind(now_ms)
            .execute(&mut *tx)
            .await
            .context("queueing a notification")?;
        }
        sqlx::query("UPDATE alert_events SET fanned_out_at = $1 WHERE id = $2")
            .bind(now_ms)
            .bind(&id)
            .execute(&mut *tx)
            .await
            .context("marking an alert event as queued")?;
    }
    tx.commit().await.context("committing queued notifications")
}

struct Due {
    id: String,
    channel_id: String,
    attempts: i64,
    kind: String,
    alert_id: String,
}

fn due_of(row: &sqlx::any::AnyRow) -> Result<Due, sqlx::Error> {
    Ok(Due {
        id: row.try_get("id")?,
        channel_id: row.try_get("channel_id")?,
        attempts: row.try_get("attempts")?,
        kind: row.try_get("kind")?,
        alert_id: row.try_get("alert_id")?,
    })
}

async fn deliver(state: &AppState, now_ms: i64, stop: &CancellationToken) -> anyhow::Result<()> {
    let rows = sqlx::query(
        "SELECT d.id, d.channel_id, d.attempts, e.kind, e.alert_id \
         FROM alert_deliveries d JOIN alert_events e ON e.id = d.event_id \
         WHERE d.state = 'pending' AND d.next_attempt_at <= $1 \
         ORDER BY d.next_attempt_at, d.id LIMIT $2",
    )
    .bind(now_ms)
    .bind(BATCH)
    .fetch_all(&state.db.pool)
    .await
    .context("reading due notifications")?;
    let mut by_channel: BTreeMap<String, Vec<Due>> = BTreeMap::new();
    for row in &rows {
        match due_of(row) {
            Ok(due) => by_channel
                .entry(due.channel_id.clone())
                .or_default()
                .push(due),
            Err(error) => tracing::warn!(%error, "skipping an unreadable notification"),
        }
    }
    let deadline = Instant::now() + PASS_BUDGET;
    let mut passes = JoinSet::new();
    for batch in by_channel.into_values() {
        let state = state.clone();
        let stop = stop.clone();
        passes.spawn(async move { deliver_channel(&state, batch, now_ms, &stop, deadline).await });
    }
    while let Some(joined) = passes.join_next().await {
        if let Err(error) = joined {
            tracing::warn!(%error, "notification delivery task failed");
        }
    }
    prune(state, now_ms).await
}

async fn prune(state: &AppState, now_ms: i64) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM alert_deliveries WHERE state = 'sent' AND sent_at < $1")
        .bind(now_ms - KEEP_SENT_MS)
        .execute(&state.db.pool)
        .await
        .context("pruning sent notifications")?;
    sqlx::query("DELETE FROM alert_deliveries WHERE state = 'failed' AND created_at < $1")
        .bind(now_ms - KEEP_HISTORY_MS)
        .execute(&state.db.pool)
        .await
        .context("pruning failed notifications")?;
    sqlx::query("DELETE FROM alerts WHERE state = 'resolved' AND resolved_at < $1")
        .bind(now_ms - KEEP_HISTORY_MS)
        .execute(&state.db.pool)
        .await
        .context("pruning resolved alerts")?;
    Ok(())
}

async fn deliver_channel(
    state: &AppState,
    batch: Vec<Due>,
    now_ms: i64,
    stop: &CancellationToken,
    deadline: Instant,
) {
    for due in batch {
        if stop.is_cancelled() || Instant::now() >= deadline {
            return;
        }
        let result = attempt(state, &due.channel_id, &due.alert_id, &due.kind, now_ms).await;
        if let Err(error) = &result {
            tracing::warn!(channel = %due.channel_id, %error, "notification delivery failed");
        }
        if let Err(error) = record(
            state,
            &due.id,
            &due.channel_id,
            due.attempts,
            &result,
            now_ms,
        )
        .await
        {
            tracing::warn!(channel = %due.channel_id, %error, "recording a delivery failed");
        }
    }
}

async fn attempt(
    state: &AppState,
    channel_id: &str,
    alert_id: &str,
    event_kind: &str,
    now_ms: i64,
) -> Result<(), DeliveryError> {
    let internal = |e: anyhow::Error| DeliveryError::new(format!("{e:#}"));
    let channel = load_channel(state, channel_id)
        .await
        .map_err(internal)?
        .ok_or_else(|| DeliveryError::new("The channel no longer exists."))?;
    if !channel.enabled {
        return Err(DeliveryError::new("The channel is disabled."));
    }
    let alert = message::load_alert(state, alert_id)
        .await
        .map_err(internal)?
        .ok_or_else(|| DeliveryError::new("The alert no longer exists."))?;
    let kind = MessageKind::parse(event_kind)
        .ok_or_else(|| DeliveryError::new(format!("Unknown event kind {event_kind}.")))?;
    send(
        state,
        &channel,
        &message::for_event(&state.config.public_url, &alert, kind, now_ms),
    )
    .await
}

async fn record(
    state: &AppState,
    delivery_id: &str,
    channel_id: &str,
    attempts: i64,
    result: &Result<(), DeliveryError>,
    now_ms: i64,
) -> anyhow::Result<()> {
    let attempts = attempts + 1;
    match result {
        Ok(()) => {
            sqlx::query(
                "UPDATE alert_deliveries SET state = 'sent', attempts = $1, sent_at = $2, \
                 last_error = NULL WHERE id = $3",
            )
            .bind(attempts)
            .bind(now_ms)
            .bind(delivery_id)
            .execute(&state.db.pool)
            .await
            .context("recording a delivery")?;
        }
        Err(error) => {
            let (next_state, next_at) = match RETRY_DELAYS.get((attempts - 1) as usize) {
                Some(delay) if attempts < MAX_ATTEMPTS => {
                    ("pending", now_ms + delay.as_millis() as i64)
                }
                _ => ("failed", now_ms),
            };
            sqlx::query(
                "UPDATE alert_deliveries SET state = $1, attempts = $2, next_attempt_at = $3, \
                 last_error = $4 WHERE id = $5",
            )
            .bind(next_state)
            .bind(attempts)
            .bind(next_at)
            .bind(error.to_string())
            .bind(delivery_id)
            .execute(&state.db.pool)
            .await
            .context("recording a delivery")?;
        }
    }
    sqlx::query(
        "UPDATE notification_channels SET last_ok = $1, last_at = $2, last_error = $3 WHERE id = $4",
    )
    .bind(i64::from(result.is_ok()))
    .bind(now_ms)
    .bind(result.as_ref().err().map(ToString::to_string))
    .bind(channel_id)
    .execute(&state.db.pool)
    .await
    .context("recording the channel state")?;
    Ok(())
}
