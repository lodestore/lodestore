use std::collections::HashMap;
use std::time::Duration;

use lodestore_proto::agent::v1::server_message::Body;
use lodestore_proto::agent::v1::{
    ServerMessage, UpdateAssign, UpdateState, UpdateStatus, UpdateStatusAck,
};
use semver::Version;
use sqlx::Row;
use tokio_util::sync::CancellationToken;

use crate::agent_update::{manifest, reached};
use crate::db::now_ms;
use crate::grpc::dispatch::{assign_queued, lock_host};
use crate::grpc::registry::CAP_UPDATE;
use crate::state::AppState;

const ROLLOUT_TICK: Duration = Duration::from_secs(5);
const MIN_TICK: Duration = Duration::from_millis(100);
const MAX_VERSION_LEN: usize = 64;
const MAX_REASON_CHARS: usize = 500;
pub(crate) const NO_CANARY: &str = "No canary host updated. Resume to go on without one.";

fn reason_text(text: &str, fallback: &str) -> String {
    let text = if text.trim().is_empty() {
        fallback
    } else {
        text
    };
    text.chars().take(MAX_REASON_CHARS).collect()
}

fn humanize(duration: Duration) -> String {
    let secs = duration.as_secs().max(1);
    let (minutes, seconds) = (secs / 60, secs % 60);
    let unit = |n: u64, name: &str| format!("{n} {name}{}", if n == 1 { "" } else { "s" });
    match (minutes, seconds) {
        (0, s) => unit(s, "second"),
        (m, 0) => unit(m, "minute"),
        (m, s) => format!("{} {}", unit(m, "minute"), unit(s, "second")),
    }
}

fn health_ms(state: &AppState) -> i64 {
    i64::try_from(state.config.agent_updates.health_timeout.as_millis()).unwrap_or(i64::MAX)
}

/// An assignment the agent never switched on is not a failed update: the
/// host goes back to the queue, or is dropped when its rollout was cancelled.
async fn release(
    state: &AppState,
    id: &str,
    rollout_cancelled: bool,
    now: i64,
) -> anyhow::Result<()> {
    if rollout_cancelled {
        sqlx::query(
            "UPDATE agent_updates SET state = 'cancelled', assigned_at = NULL, progress_at = NULL, \
             finished_at = $1 WHERE id = $2 AND state = 'assigned' AND switched_at IS NULL",
        )
        .bind(now)
        .bind(id)
        .execute(&state.db.pool)
        .await?;
    } else {
        sqlx::query(
            "UPDATE agent_updates SET state = 'queued', assigned_at = NULL, progress_at = NULL \
             WHERE id = $1 AND state = 'assigned' AND switched_at IS NULL",
        )
        .bind(id)
        .execute(&state.db.pool)
        .await?;
    }
    Ok(())
}

async fn roll_back(state: &AppState, id: &str, reason: &str, now: i64) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE agent_updates SET state = 'rolled_back', reason = $1, finished_at = $2 \
         WHERE id = $3 AND state = 'assigned'",
    )
    .bind(reason)
    .bind(now)
    .bind(id)
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

/// Records the version the agent reports on every connection; an assigned
/// update succeeds only when the host comes back with its target version.
/// A new session also means the agent dropped any download of the previous
/// one, so an assignment it never switched on goes back to the queue.
pub async fn on_hello(state: &AppState, agent_id: &str, version: &str) -> anyhow::Result<()> {
    let now = now_ms();
    let version: String = version.chars().take(MAX_VERSION_LEN).collect();
    if !version.is_empty() {
        sqlx::query("UPDATE agents SET version = $1 WHERE id = $2")
            .bind(&version)
            .bind(agent_id)
            .execute(&state.db.pool)
            .await?;
        sqlx::query(
            "UPDATE agent_updates SET state = 'succeeded', finished_at = $1 \
             WHERE host_id = $2 AND state IN ('queued', 'assigned') AND target_version = $3",
        )
        .bind(now)
        .bind(agent_id)
        .bind(&version)
        .execute(&state.db.pool)
        .await?;
    }
    let stale = sqlx::query(
        "SELECT u.id, r.state AS rollout_state FROM agent_updates u \
         JOIN agent_rollouts r ON r.id = u.rollout_id \
         WHERE u.host_id = $1 AND u.state = 'assigned' AND u.switched_at IS NULL",
    )
    .bind(agent_id)
    .fetch_all(&state.db.pool)
    .await?;
    for row in stale {
        let cancelled = row.try_get::<String, _>("rollout_state")? == "cancelled";
        release(state, &row.try_get::<String, _>("id")?, cancelled, now).await?;
    }
    Ok(())
}

pub async fn on_status(state: &AppState, agent_id: &str, m: UpdateStatus) -> anyhow::Result<()> {
    let pool = &state.db.pool;
    let now = now_ms();
    let id = m.update_id.as_str();
    let state_code = UpdateState::try_from(m.state);
    let ends = matches!(
        state_code,
        Ok(UpdateState::Rejected | UpdateState::RolledBack | UpdateState::Confirmed)
    );
    match state_code {
        Ok(UpdateState::Switching) => {
            sqlx::query(
                "UPDATE agent_updates SET switched_at = $1 \
                 WHERE id = $2 AND host_id = $3 AND state = 'assigned' AND switched_at IS NULL",
            )
            .bind(now)
            .bind(id)
            .bind(agent_id)
            .execute(pool)
            .await?;
        }
        Ok(UpdateState::Busy) => {
            sqlx::query(
                "UPDATE agent_updates SET state = 'queued', assigned_at = NULL, progress_at = NULL \
                 WHERE id = $1 AND host_id = $2 AND state = 'assigned'",
            )
            .bind(id)
            .bind(agent_id)
            .execute(pool)
            .await?;
        }
        Ok(UpdateState::Rejected) => {
            let reason = reason_text(&m.reason, "The agent rejected the update.");
            sqlx::query(
                "UPDATE agent_updates SET state = 'rolled_back', reason = $1, finished_at = $2 \
                 WHERE id = $3 AND host_id = $4 AND state = 'assigned'",
            )
            .bind(reason)
            .bind(now)
            .bind(id)
            .bind(agent_id)
            .execute(pool)
            .await?;
            assign_queued(state, Some(agent_id)).await?;
        }
        Ok(UpdateState::RolledBack) => {
            let reason = reason_text(&m.reason, "The agent returned to the previous version.");
            sqlx::query(
                "UPDATE agent_updates SET state = 'rolled_back', reason = $1, finished_at = $2 \
                 WHERE id = $3 AND host_id = $4 \
                 AND state IN ('queued', 'assigned', 'rolled_back', 'succeeded')",
            )
            .bind(reason)
            .bind(now)
            .bind(id)
            .bind(agent_id)
            .execute(pool)
            .await?;
            let own = sqlx::query("SELECT 1 FROM agent_updates WHERE id = $1 AND host_id = $2")
                .bind(id)
                .bind(agent_id)
                .fetch_optional(pool)
                .await?
                .is_some();
            if own {
                state.registry.send(
                    agent_id,
                    ServerMessage {
                        body: Some(Body::UpdateStatusAck(UpdateStatusAck {
                            update_id: m.update_id.clone(),
                        })),
                    },
                );
            }
            assign_queued(state, Some(agent_id)).await?;
        }
        Ok(UpdateState::Confirmed) => {
            sqlx::query(
                "UPDATE agent_updates SET state = 'succeeded', finished_at = $1 \
                 WHERE id = $2 AND host_id = $3 AND state = 'assigned' \
                 AND target_version = (SELECT version FROM agents WHERE id = $3)",
            )
            .bind(now)
            .bind(id)
            .bind(agent_id)
            .execute(pool)
            .await?;
            assign_queued(state, Some(agent_id)).await?;
        }
        _ => {}
    }
    if ends {
        kick(state);
    }
    Ok(())
}

pub fn kick(state: &AppState) {
    let state = state.clone();
    tokio::spawn(async move {
        if let Err(error) = tick_once(&state, now_ms()).await {
            tracing::warn!(%error, "agent update tick failed");
        }
    });
}

async fn cancel_queued(
    state: &AppState,
    update_id: &str,
    reason: String,
    now: i64,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE agent_updates SET state = 'cancelled', reason = $1, finished_at = $2 \
         WHERE id = $3 AND state = 'queued'",
    )
    .bind(reason)
    .bind(now)
    .bind(update_id)
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

async fn pause_with_reason(state: &AppState, rollout: &str, reason: String) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE agent_rollouts SET state = 'paused', pause_reason = $1 \
         WHERE id = $2 AND state = 'running'",
    )
    .bind(reason)
    .bind(rollout)
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

/// Runs over every assigned update whatever its rollout is doing, so a
/// cancelled or paused rollout never leaves one stuck.
async fn settle_assigned(state: &AppState, now: i64) -> anyhow::Result<()> {
    let window = state.config.agent_updates.health_timeout;
    let window_ms = health_ms(state);
    let rows = sqlx::query(
        "SELECT u.id, u.host_id, u.assigned_at, u.switched_at, u.progress_at, \
         r.state AS rollout_state FROM agent_updates u \
         JOIN agent_rollouts r ON r.id = u.rollout_id WHERE u.state = 'assigned'",
    )
    .fetch_all(&state.db.pool)
    .await?;
    for row in rows {
        let id: String = row.try_get("id")?;
        let host: String = row.try_get("host_id")?;
        let switched: Option<i64> = row.try_get("switched_at")?;
        if let Some(switched) = switched {
            if switched.saturating_add(window_ms) < now {
                let reason = format!(
                    "The new version did not connect within {}.",
                    humanize(window)
                );
                roll_back(state, &id, &reason, now).await?;
            }
            continue;
        }
        if !state.registry.is_online(&host) {
            let cancelled = row.try_get::<String, _>("rollout_state")? == "cancelled";
            release(state, &id, cancelled, now).await?;
            continue;
        }
        let last = row
            .try_get::<Option<i64>, _>("progress_at")?
            .or(row.try_get::<Option<i64>, _>("assigned_at")?);
        if last.is_some_and(|last| last.saturating_add(window_ms) < now) {
            let reason = format!("The update download stopped for {}.", humanize(window));
            roll_back(state, &id, &reason, now).await?;
        }
    }
    Ok(())
}

/// Wave 0 has nothing left to try and no success, so nothing would ever open
/// the later waves. A rollout already paused gets the reason too, because
/// resuming past it is what waives the canary.
async fn pause_failed_canary(state: &AppState) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE agent_rollouts SET state = 'paused', pause_reason = $1 \
         WHERE state IN ('running', 'paused') AND pause_reason IS NULL AND canary_waived = 0 \
         AND EXISTS (SELECT 1 FROM agent_updates q \
         WHERE q.rollout_id = agent_rollouts.id AND q.state = 'queued') \
         AND NOT EXISTS (SELECT 1 FROM agent_updates c \
         WHERE c.rollout_id = agent_rollouts.id AND c.wave = 0 \
         AND c.state IN ('queued', 'assigned', 'succeeded'))",
    )
    .bind(NO_CANARY)
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

/// Canaries that cannot act leave nothing to end the wait: the later waves stay
/// shut while wave 0 neither fails nor succeeds. After a health timeout the
/// operator gets the same choice as for a failed canary. The clock also starts
/// with the server, because no agent has reconnected yet right after a restart.
/// With no later wave nothing is shut, and those hosts update when they return.
async fn pause_stuck_canary(state: &AppState, now: i64) -> anyhow::Result<()> {
    let cutoff = now.saturating_sub(health_ms(state));
    if state.started_at_ms >= cutoff {
        return Ok(());
    }
    let rows = sqlx::query(
        "SELECT r.id, u.host_id FROM agent_rollouts r \
         JOIN agent_updates u ON u.rollout_id = r.id AND u.wave = 0 AND u.state = 'queued' \
         WHERE r.state = 'running' AND r.canary_waived = 0 \
         AND COALESCE(r.resumed_at, r.created_at) < $1 \
         AND EXISTS (SELECT 1 FROM agent_updates q WHERE q.rollout_id = r.id \
         AND q.wave > 0 AND q.state = 'queued') \
         AND NOT EXISTS (SELECT 1 FROM agent_updates c WHERE c.rollout_id = r.id \
         AND c.wave = 0 AND c.state IN ('assigned', 'succeeded'))",
    )
    .bind(cutoff)
    .fetch_all(&state.db.pool)
    .await?;
    let mut ready: HashMap<String, bool> = HashMap::new();
    for row in rows {
        let host: String = row.try_get("host_id")?;
        *ready.entry(row.try_get("id")?).or_default() |= ready_for_update(state, &host);
    }
    for (rollout, _) in ready.into_iter().filter(|(_, ready)| !ready) {
        pause_with_reason(state, &rollout, NO_CANARY.to_string()).await?;
    }
    Ok(())
}

async fn auto_pause(state: &AppState) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE agent_rollouts SET state = 'paused' WHERE state = 'running' AND \
         (SELECT COUNT(*) FROM agent_updates u WHERE u.rollout_id = agent_rollouts.id \
         AND u.state = 'rolled_back') - pause_baseline > pause_after",
    )
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

pub async fn claim_update(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    update_id: &str,
    host_id: &str,
    now: i64,
) -> sqlx::Result<bool> {
    let done = sqlx::query(
        "UPDATE agent_updates SET state = 'assigned', assigned_at = $1 \
         WHERE id = $2 AND state = 'queued' \
         AND EXISTS (SELECT 1 FROM agent_rollouts r \
         WHERE r.id = agent_updates.rollout_id AND r.state = 'running' \
         AND (SELECT COUNT(*) FROM agent_updates x WHERE x.rollout_id = r.id \
         AND x.state = 'rolled_back') - r.pause_baseline <= r.pause_after) \
         AND NOT EXISTS \
         (SELECT 1 FROM jobs WHERE host_id = $3 AND state IN ('assigned', 'running'))",
    )
    .bind(now)
    .bind(update_id)
    .bind(host_id)
    .execute(&mut **tx)
    .await?;
    Ok(done.rows_affected() == 1)
}

pub(crate) fn ready_for_update(state: &AppState, host: &str) -> bool {
    state.registry.supports(host, CAP_UPDATE) && state.registry.live.is_ready(host)
}

async fn has_active_job(state: &AppState, host: &str) -> anyhow::Result<bool> {
    Ok(
        sqlx::query("SELECT 1 FROM jobs WHERE host_id = $1 AND state IN ('assigned', 'running')")
            .bind(host)
            .fetch_optional(&state.db.pool)
            .await?
            .is_some(),
    )
}

/// No wave above 0 starts before a host of wave 0 succeeded or the canary was
/// waived, so offline or busy canaries do hold the later waves back until
/// `pause_stuck_canary` hands the choice to the operator. Past that gate, while
/// a rollout has assigned updates only its queued hosts of that wave or an
/// earlier one start; otherwise the lowest wave with a ready host opens, so
/// offline or busy hosts do not hold a later wave back.
async fn assign_waves(state: &AppState, now: i64) -> anyhow::Result<()> {
    let queued = sqlx::query(
        "SELECT u.id, u.rollout_id, u.host_id, u.arch, u.target_version, u.wave, \
         a.version AS host_version \
         FROM agent_updates u JOIN agent_rollouts r ON r.id = u.rollout_id \
         JOIN agents a ON a.id = u.host_id \
         WHERE u.state = 'queued' AND r.state = 'running' \
         AND (u.wave = 0 OR r.canary_waived = 1 OR EXISTS (SELECT 1 FROM agent_updates c \
         WHERE c.rollout_id = u.rollout_id AND c.wave = 0 AND c.state = 'succeeded')) \
         ORDER BY u.wave, u.queued_at",
    )
    .fetch_all(&state.db.pool)
    .await?;
    if queued.is_empty() {
        return Ok(());
    }
    let mut open: HashMap<String, i64> = HashMap::new();
    let assigned = sqlx::query(
        "SELECT rollout_id, MIN(wave) AS wave FROM agent_updates WHERE state = 'assigned' \
         GROUP BY rollout_id",
    )
    .fetch_all(&state.db.pool)
    .await?;
    for row in assigned {
        open.insert(row.try_get("rollout_id")?, row.try_get("wave")?);
    }
    let binaries = manifest::binaries(&state.config).await;
    for row in queued {
        let id: String = row.try_get("id")?;
        let rollout: String = row.try_get("rollout_id")?;
        let host: String = row.try_get("host_id")?;
        let arch: String = row.try_get("arch")?;
        let target: String = row.try_get("target_version")?;
        let wave: i64 = row.try_get("wave")?;
        let version: String = row.try_get("host_version")?;
        if reached(&version, &target) {
            let reason = format!("The host already runs {version}.");
            cancel_queued(state, &id, reason, now).await?;
            continue;
        }
        let channel_ok = match (Version::parse(&version), Version::parse(&target)) {
            (Ok(host_v), Ok(target_v)) => lodestore_proto::accepts_channel(&host_v, &target_v),
            (Err(_), Ok(target_v)) => target_v.pre.is_empty(),
            (_, Err(_)) => true,
        };
        if !channel_ok {
            let reason = format!(
                "The host runs {version} and takes only stable releases; {target} is a pre-release."
            );
            cancel_queued(state, &id, reason, now).await?;
            continue;
        }
        if open.get(&rollout).is_some_and(|open| wave > *open) {
            continue;
        }
        if !ready_for_update(state, &host) || has_active_job(state, &host).await? {
            continue;
        }
        let binary = Version::parse(&target)
            .ok()
            .and_then(|v| manifest::binary_for(&binaries, &arch, &v));
        let Some(binary) = binary else {
            let reason = format!("No signed agent {target} for {arch} on this server.");
            pause_with_reason(state, &rollout, reason).await?;
            continue;
        };
        let mut tx = state.db.pool.begin().await?;
        lock_host(&mut tx, &host).await?;
        let claimed = claim_update(&mut tx, &id, &host, now).await?;
        tx.commit().await?;
        if !claimed {
            continue;
        }
        open.entry(rollout)
            .and_modify(|open| *open = (*open).min(wave))
            .or_insert(wave);
        let sent = state.registry.send(
            &host,
            ServerMessage {
                body: Some(Body::UpdateAssign(UpdateAssign {
                    update_id: id.clone(),
                    target_version: target,
                    arch,
                    size: binary.size,
                    sha256: binary.sha256.clone().unwrap_or_default(),
                })),
            },
        );
        if !sent {
            release(state, &id, false, now).await?;
        }
    }
    Ok(())
}

static TICK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub async fn tick_once(state: &AppState, now: i64) -> anyhow::Result<()> {
    let _one_tick_at_a_time = TICK.lock().await;
    settle_assigned(state, now).await?;
    sqlx::query(
        "UPDATE agent_updates SET state = 'cancelled', finished_at = $1 WHERE state = 'queued' \
         AND rollout_id IN (SELECT id FROM agent_rollouts WHERE state = 'cancelled')",
    )
    .bind(now)
    .execute(&state.db.pool)
    .await?;
    pause_failed_canary(state).await?;
    pause_stuck_canary(state, now).await?;
    auto_pause(state).await?;
    assign_waves(state, now).await?;
    sqlx::query(
        "UPDATE agent_rollouts SET state = 'done', finished_at = $1 WHERE (state = 'running' \
         OR (state = 'paused' AND EXISTS (SELECT 1 FROM agent_updates u0 \
         WHERE u0.rollout_id = agent_rollouts.id))) \
         AND NOT EXISTS (SELECT 1 FROM agent_updates u WHERE u.rollout_id = agent_rollouts.id \
         AND u.state IN ('queued', 'assigned'))",
    )
    .bind(now)
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

pub async fn run(state: AppState, stop: CancellationToken) -> anyhow::Result<()> {
    let period = ROLLOUT_TICK
        .min(state.config.agent_updates.health_timeout / 2)
        .max(MIN_TICK);
    let mut interval = tokio::time::interval(period);
    loop {
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            _ = interval.tick() => {}
        }
        if let Err(error) = tick_once(&state, now_ms()).await {
            tracing::warn!(%error, "agent update tick failed");
        }
    }
}
