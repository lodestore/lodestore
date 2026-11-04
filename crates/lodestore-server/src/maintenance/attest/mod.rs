//! Signs the index of repositories written before attestations existed.
//! The bucket holds the truth (the completion marker); the database flag is
//! only a cache, so losing the database never makes the server trust shards
//! planted after the first upgrade.

pub mod verify;

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Context;
use lodestore_format::crypto::{BlobId, Entropy};
use lodestore_format::index::{hex32, parse_hex32};
use sqlx::Row;
use tokio_util::sync::CancellationToken;

use crate::catalog::open_repository;
use crate::db::now_ms;
use crate::state::AppState;

pub async fn ensure_upgraded(state: &AppState, repository_id: &str) -> anyhow::Result<()> {
    let flag: Option<i64> =
        match sqlx::query("SELECT attest_upgraded_at FROM repositories WHERE id = $1")
            .bind(repository_id)
            .fetch_optional(&state.db.pool)
            .await?
        {
            Some(row) => row.try_get("attest_upgraded_at")?,
            None => None,
        };
    if flag.is_some() {
        return Ok(());
    }
    let source = open_repository(state, repository_id).await?;
    let outcome =
        lodestore_format::attest::ensure_upgraded(&source.store, &source.keys, &mut Entropy::os())
            .await
            .map_err(|error| {
                tracing::error!(
                    repository = repository_id,
                    %error,
                    "refusing to sign the repository's existing index"
                );
                error
            })
            .context("signing the repository's existing index")?;
    sqlx::query("UPDATE repositories SET attest_upgraded_at = $1 WHERE id = $2")
        .bind(now_ms())
        .bind(repository_id)
        .execute(&state.db.pool)
        .await?;
    tracing::info!(
        repository = repository_id,
        already = outcome.already,
        attested = outcome.attested,
        "repository index attested"
    );
    Ok(())
}

pub async fn load_verified(
    state: &AppState,
    repository_id: &str,
) -> anyhow::Result<HashSet<BlobId>> {
    let rows = sqlx::query("SELECT shard_id FROM verified_shards WHERE repository_id = $1")
        .bind(repository_id)
        .fetch_all(&state.db.pool)
        .await?;
    let mut out = HashSet::with_capacity(rows.len());
    for row in rows {
        let hex: String = row.try_get("shard_id")?;
        match parse_hex32(&hex) {
            Some(id) => {
                out.insert(id);
            }
            None => tracing::warn!(
                repository = repository_id,
                shard = %hex,
                "skipping a malformed verified shard id"
            ),
        }
    }
    Ok(out)
}

pub async fn record_verified(
    state: &AppState,
    repository_id: &str,
    shard_ids: &[BlobId],
) -> anyhow::Result<()> {
    let at = now_ms();
    for id in shard_ids {
        sqlx::query(
            "INSERT INTO verified_shards (repository_id, shard_id, verified_at) \
             VALUES ($1, $2, $3) ON CONFLICT (repository_id, shard_id) DO NOTHING",
        )
        .bind(repository_id)
        .bind(hex32(id))
        .bind(at)
        .execute(&state.db.pool)
        .await?;
    }
    Ok(())
}

pub async fn tick_once(state: &AppState) -> anyhow::Result<()> {
    let rows = sqlx::query("SELECT id FROM repositories WHERE attest_upgraded_at IS NULL")
        .fetch_all(&state.db.pool)
        .await?;
    for row in rows {
        let id: String = row.try_get("id")?;
        if let Err(error) = ensure_upgraded(state, &id).await {
            tracing::warn!(repository = %id, error = %format!("{error:#}"), "attestation upgrade failed");
        }
    }
    let pending = sqlx::query(
        "SELECT id FROM jobs WHERE verify_state = 'pending' ORDER BY finished_at LIMIT 32",
    )
    .fetch_all(&state.db.pool)
    .await?;
    for row in pending {
        verify::verify_then_retain(state.clone(), row.try_get("id")?).await;
    }
    Ok(())
}

pub async fn resume_interrupted(state: &AppState) -> anyhow::Result<()> {
    sqlx::query("UPDATE jobs SET verify_state = 'pending' WHERE verify_state = 'running'")
        .execute(&state.db.pool)
        .await?;
    Ok(())
}

pub async fn run(state: AppState, stop: CancellationToken) -> anyhow::Result<()> {
    resume_interrupted(&state).await?;
    let mut interval =
        tokio::time::interval(Duration::from_secs(state.config.schedule_tick_secs.max(1)));
    loop {
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            _ = interval.tick() => {}
        }
        if let Err(error) = tick_once(&state).await {
            tracing::warn!(%error, "attestation tick failed");
        }
    }
}
