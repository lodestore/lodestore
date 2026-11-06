//! Signs the index of repositories written before attestations existed.
//! The bucket holds the truth (the completion marker); the database flag is
//! only a cache, so losing the database never makes the server trust shards
//! planted after the first upgrade.

pub mod verify;

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Context;
use lodestore_format::attest::{
    MAX_ATTEST_PROVENANCE, put_attestation, read_attestations, seal_provenance,
};
use lodestore_format::crypto::{BlobId, Entropy};
use lodestore_format::index::{hex32, parse_hex32};
use lodestore_format::keys::ReadKeys;
use lodestore_format::pb;
use lodestore_format::provenance::{ProvenanceSet, Signer};
use lodestore_format::store::BlobStore;
use sqlx::Row;
use tokio::sync::Mutex;
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

/// Signs what a 0.1 catalog already tied to a host and a job. The snapshot
/// row is the tie: job rows are purged after 90 days, so a missing one does
/// not count against it. The database flag is a cache: the signatures are
/// additive, so a repeat after a crash only duplicates records, which
/// collapse by snapshot id. A repository created by 0.2 starts flagged.
pub async fn ensure_provenance_backfilled(
    state: &AppState,
    repository_id: &str,
) -> anyhow::Result<usize> {
    if backfilled(state, repository_id).await? {
        return Ok(0);
    }
    let _one = BACKFILL.lock().await;
    if backfilled(state, repository_id).await? {
        return Ok(0);
    }
    ensure_upgraded(state, repository_id).await?;
    let rows = sqlx::query(
        "SELECT s.id, s.host_id, s.job_id, a.hostname FROM snapshots s \
         LEFT JOIN jobs j ON j.id = s.job_id JOIN agents a ON a.id = s.host_id \
         WHERE s.repository_id = $1 AND s.host_id IS NOT NULL AND s.job_id IS NOT NULL \
         AND s.unverified_reason IS NULL AND s.verified_at IS NULL \
         AND (j.id IS NULL OR (j.state IN ('succeeded', 'warnings') \
         AND (j.verify_state IS NULL OR j.verify_state = 'verified'))) ORDER BY s.id",
    )
    .bind(repository_id)
    .fetch_all(&state.db.pool)
    .await?;
    let source = open_repository(state, repository_id).await?;
    let at = now_ms();
    let mut entropy = Entropy::os();
    let mut sealed = Vec::with_capacity(rows.len());
    for row in &rows {
        let id: String = row.try_get("id")?;
        let Some(snapshot) = parse_hex32(&id) else {
            continue;
        };
        let record = pb::ProvenanceRecord {
            snapshot_id: snapshot.to_vec(),
            host_id: row.try_get("host_id")?,
            hostname: row.try_get("hostname")?,
            job_id: row.try_get("job_id")?,
            verified_at: Some(verify::timestamp_ms(at)),
        };
        sealed.push((id, seal_provenance(&source.keys, &record, &mut entropy)?));
    }
    let mut signed = 0;
    for part in parts(sealed, MAX_ATTEST_PROVENANCE, MAX_PART_BYTES) {
        let (ids, records): (Vec<String>, Vec<Vec<u8>>) = part.into_iter().unzip();
        put_attestation(&source.store, &source.keys, &[], &[], records, &mut entropy).await?;
        // A retry after a failed part signs only the rows still unmarked.
        for id in &ids {
            verify::mark_verified(state, id, repository_id, at, "server").await?;
        }
        signed += ids.len();
    }
    sqlx::query("UPDATE repositories SET provenance_backfilled_at = $1 WHERE id = $2")
        .bind(at)
        .bind(repository_id)
        .execute(&state.db.pool)
        .await?;
    tracing::info!(
        repository = repository_id,
        signed,
        "snapshot provenance backfilled"
    );
    Ok(signed)
}

/// Held by a backfill: a second one started meanwhile would sign the same rows again.
static BACKFILL: Mutex<()> = Mutex::const_new(());

/// The format's budget for the sealed records of one attestation.
const MAX_PART_BYTES: usize = 6 << 20;

async fn backfilled(state: &AppState, repository_id: &str) -> anyhow::Result<bool> {
    let row = sqlx::query("SELECT provenance_backfilled_at FROM repositories WHERE id = $1")
        .bind(repository_id)
        .fetch_optional(&state.db.pool)
        .await?;
    Ok(match row {
        Some(row) => row
            .try_get::<Option<i64>, _>("provenance_backfilled_at")?
            .is_some(),
        None => false,
    })
}

/// Cuts sealed records into parts of at most `max_records` records and
/// `max_bytes` bytes, counting a record's length prefix.
fn parts<T>(
    records: Vec<(T, Vec<u8>)>,
    max_records: usize,
    max_bytes: usize,
) -> Vec<Vec<(T, Vec<u8>)>> {
    let mut out: Vec<Vec<(T, Vec<u8>)>> = Vec::new();
    let mut bytes = 0usize;
    for record in records {
        let size = record.1.len().saturating_add(8);
        match out.last_mut() {
            Some(part) if part.len() < max_records && bytes.saturating_add(size) <= max_bytes => {
                bytes += size;
                part.push(record);
            }
            _ => {
                bytes = size;
                out.push(vec![record]);
            }
        }
    }
    out
}

/// Bucket to database only: the catalog gets the mark for every snapshot the
/// bucket's signatures cover, including those a rebuilt catalog lost. A
/// server record marks a row only when the row names the record's host: a
/// rebuilt row takes its host from the manifest, which the agent wrote.
pub async fn reconcile_provenance<S: BlobStore>(
    state: &AppState,
    repository_id: &str,
    store: &S,
    keys: &ReadKeys,
) -> anyhow::Result<usize> {
    let attested = read_attestations(store, keys).await?;
    if attested.incomplete {
        return Ok(0);
    }
    let set = ProvenanceSet::from_attested(&attested, keys);
    let rows = sqlx::query(
        "SELECT id, host_id FROM snapshots WHERE repository_id = $1 AND verified_at IS NULL \
         AND unverified_reason IS NULL",
    )
    .bind(repository_id)
    .fetch_all(&state.db.pool)
    .await?;
    let mut marked = 0;
    let mut other_host = 0;
    for row in rows {
        let id: String = row.try_get("id")?;
        let Some(snapshot) = parse_hex32(&id) else {
            continue;
        };
        let (Some(record), Some(signer)) = (set.get(&snapshot), set.signer(&snapshot)) else {
            continue;
        };
        let by = match signer {
            Signer::Server => {
                let host: Option<String> = row.try_get("host_id")?;
                if host.as_deref() != Some(record.host_id.as_str()) {
                    other_host += 1;
                    continue;
                }
                "server"
            }
            Signer::Owner => "owner",
        };
        let at = record.verified_at.as_ref().map_or(0, |t| {
            t.seconds
                .saturating_mul(1000)
                .saturating_add(i64::from(t.nanos / 1_000_000))
        });
        sqlx::query(
            "UPDATE snapshots SET verified_at = $1, verified_by = $2 \
             WHERE id = $3 AND repository_id = $4 AND verified_at IS NULL",
        )
        .bind(at)
        .bind(by)
        .bind(&id)
        .bind(repository_id)
        .execute(&state.db.pool)
        .await?;
        marked += 1;
    }
    if other_host > 0 {
        tracing::warn!(
            repository = repository_id,
            snapshots = other_host,
            "left snapshots unverified: the catalog names another host than their signature"
        );
    }
    Ok(marked)
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

pub const MAX_DISTRUSTED_SENT: usize = 65_536;

pub const MAX_LIES_PER_PASS: usize = 64;

pub async fn record_distrusted(
    state: &AppState,
    repository_id: &str,
    shard: &BlobId,
    reason: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO distrusted_shards (repository_id, shard_id, at, reason) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (repository_id, shard_id) DO UPDATE SET at = excluded.at",
    )
    .bind(repository_id)
    .bind(hex32(shard))
    .bind(now_ms())
    .bind(reason)
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

/// Newest first, at most [`MAX_DISTRUSTED_SENT`]: the list sent to agents.
pub async fn load_distrusted(state: &AppState, repository_id: &str) -> anyhow::Result<Vec<BlobId>> {
    load_distrusted_up_to(
        state,
        repository_id,
        i64::try_from(MAX_DISTRUSTED_SENT).unwrap_or(i64::MAX),
    )
    .await
}

/// Every distrusted shard of a repository, for cleanup.
pub async fn load_all_distrusted(
    state: &AppState,
    repository_id: &str,
) -> anyhow::Result<Vec<BlobId>> {
    load_distrusted_up_to(state, repository_id, i64::MAX).await
}

async fn load_distrusted_up_to(
    state: &AppState,
    repository_id: &str,
    limit: i64,
) -> anyhow::Result<Vec<BlobId>> {
    let rows = sqlx::query(
        "SELECT shard_id FROM distrusted_shards WHERE repository_id = $1 \
         ORDER BY at DESC, shard_id LIMIT $2",
    )
    .bind(repository_id)
    .bind(limit)
    .fetch_all(&state.db.pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let hex: String = row.try_get("shard_id")?;
        match parse_hex32(&hex) {
            Some(id) => out.push(id),
            None => tracing::warn!(
                repository = repository_id,
                shard = %hex,
                "skipping a malformed distrusted shard id"
            ),
        }
    }
    Ok(out)
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
    let rows = sqlx::query("SELECT id FROM repositories WHERE provenance_backfilled_at IS NULL")
        .fetch_all(&state.db.pool)
        .await?;
    for row in rows {
        let id: String = row.try_get("id")?;
        if let Err(error) = ensure_provenance_backfilled(state, &id).await {
            tracing::warn!(repository = %id, error = %format!("{error:#}"), "provenance backfill failed");
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

/// Runs at startup, before agents can finish jobs: a check started after
/// this would otherwise be reset and run a second time.
pub async fn resume_interrupted(state: &AppState) -> anyhow::Result<()> {
    sqlx::query("UPDATE jobs SET verify_state = 'pending' WHERE verify_state = 'running'")
        .execute(&state.db.pool)
        .await?;
    Ok(())
}

pub async fn run(state: AppState, stop: CancellationToken) -> anyhow::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::parts;

    fn sizes(parts: &[Vec<(usize, Vec<u8>)>]) -> Vec<usize> {
        parts.iter().map(Vec::len).collect()
    }

    #[test]
    fn parts_respect_record_and_byte_limits() {
        let small: Vec<_> = (0..5).map(|n| (n, vec![0u8; 1])).collect();
        assert_eq!(sizes(&parts(small, 2, 1 << 20)), [2, 2, 1]);
        let big: Vec<_> = (0..5).map(|n| (n, vec![0u8; 92])).collect();
        assert_eq!(sizes(&parts(big, 100, 250)), [2, 2, 1]);
        let huge = vec![(0, vec![0u8; 500])];
        assert_eq!(sizes(&parts(huge, 100, 250)), [1]);
    }
}
