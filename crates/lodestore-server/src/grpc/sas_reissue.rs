use std::time::Duration;

use sqlx::Row;
use tokio_util::sync::CancellationToken;

use crate::grpc::agent::{CAP_LOCATION, location_message};
use crate::grpc::registry::Registry;
use crate::repos::db::{record_sas_issued, writer_material};
use crate::state::AppState;

pub const REISSUE_EVERY: Duration = Duration::from_secs(3600);

fn recipients(registry: &Registry, repository_id: &str) -> Vec<String> {
    registry
        .online_in_repository(repository_id)
        .into_iter()
        .filter(|agent| !registry.is_revoked(agent) && registry.supports(agent, CAP_LOCATION))
        .collect()
}

async fn reissue_repository(state: &AppState, repository_id: &str) -> anyhow::Result<u32> {
    let access = state.repos.access_lock(repository_id);
    let _held = access.lock().await;
    let agents = recipients(&state.registry, repository_id);
    if agents.is_empty() {
        return Ok(0);
    }
    let material = writer_material(state, repository_id).await?;
    let Some(expires_at_ms) = material.sas_expires_at_ms else {
        return Ok(0);
    };
    let mut sent = 0;
    for agent in agents {
        if state.registry.is_revoked(&agent) {
            continue;
        }
        if let Err(error) = record_sas_issued(state, &agent, expires_at_ms).await {
            tracing::warn!(
                agent_id = %agent,
                error = %format!("{error:#}"),
                "could not record the expiry of a storage token; it was not sent"
            );
            continue;
        }
        if state
            .registry
            .send(&agent, location_message(&material.location, repository_id))
        {
            sent += 1;
        }
    }
    Ok(sent)
}

/// One pass: sends a freshly signed SAS to every online, non-revoked agent of
/// each repository that vends one. Returns how many agents were sent a token.
pub async fn reissue_once(state: &AppState) -> anyhow::Result<u32> {
    let ids: Vec<String> = sqlx::query("SELECT id FROM repositories WHERE agent_sas_vending <> 0")
        .fetch_all(&state.db.pool)
        .await?
        .iter()
        .map(|row| row.try_get("id"))
        .collect::<Result<_, _>>()?;
    let mut sent = 0;
    for id in ids {
        match reissue_repository(state, &id).await {
            Ok(count) => sent += count,
            Err(error) => tracing::warn!(
                repository_id = %id,
                error = %format!("{error:#}"),
                "could not reissue the storage token of a repository"
            ),
        }
    }
    Ok(sent)
}

pub async fn run(state: AppState, stop: CancellationToken) -> anyhow::Result<()> {
    let mut interval =
        tokio::time::interval_at(tokio::time::Instant::now() + REISSUE_EVERY, REISSUE_EVERY);
    loop {
        tokio::select! {
            () = stop.cancelled() => return Ok(()),
            _ = interval.tick() => {}
        }
        if let Err(error) = reissue_once(&state).await {
            tracing::warn!(error = %format!("{error:#}"), "the storage token reissue failed");
        }
    }
}
