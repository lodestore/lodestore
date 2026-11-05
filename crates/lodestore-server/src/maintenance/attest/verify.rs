//! Opens the bytes of every new backup on the server. A snapshot counts as
//! verified only when it resolves through shards whose frames the server
//! opened itself; being attested never skips that (an upgrade signs whatever
//! sat in the bucket at the time).

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Context;
use lodestore_format::attest::{put_attestation, read_attestations};
use lodestore_format::check::is_data_error;
use lodestore_format::crypto::{BlobId, Entropy};
use lodestore_format::index::{hex32, index_object_key, open_shard, parse_hex32};
use lodestore_format::keys::ReadKeys;
use lodestore_format::store::BlobStore;
use lodestore_format::verify_snapshot::{
    check_snapshot_resolves, snapshot_index_refs, verify_shard,
};
use sqlx::Row;
use tokio::sync::{RwLock, Semaphore};

use super::{ensure_upgraded, load_verified, record_verified};
use crate::catalog::retention::{after_backup, append_event};
use crate::catalog::{self, JobLink, open_repository};
use crate::db::now_ms;
use crate::state::AppState;

pub const VERIFICATION_FAILED: &str = "verification_failed";
pub const VERIFY_RETRY_WINDOW: Duration = Duration::from_secs(3600);
const VERIFY_CONCURRENCY: usize = 2;

static PERMITS: Semaphore = Semaphore::const_new(VERIFY_CONCURRENCY);

/// Held shared by a verification and exclusively by a cleanup while it
/// deletes shards and packs. A verification resolves through the verified
/// shards it loaded first; a sweep in between would remove shards whose
/// replacements that set does not know yet.
// shortcut: one lock for every repository, so a sweep in one holds back verifications in all;
// key it by repository if many repositories share a server.
pub static SWEEP: RwLock<()> = RwLock::const_new(());

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Verified,
    Failed(String),
    Retry(String),
}

struct Target {
    snapshot_id: String,
    host_id: String,
    repository_id: String,
}

fn classify(error: &lodestore_format::Error) -> Verdict {
    if is_data_error(error) {
        Verdict::Failed(error.to_string())
    } else {
        Verdict::Retry(error.to_string())
    }
}

pub async fn verify_then_retain(state: AppState, job_id: String) {
    let Ok(_permit) = PERMITS.acquire().await else {
        return;
    };
    if let Err(error) = run_one(&state, &job_id).await {
        tracing::warn!(%job_id, error = %format!("{error:#}"), "verifying a backup failed");
    }
}

async fn run_one(state: &AppState, job_id: &str) -> anyhow::Result<()> {
    let claimed = sqlx::query(
        "UPDATE jobs SET verify_state = 'running' WHERE id = $1 AND verify_state = 'pending'",
    )
    .bind(job_id)
    .execute(&state.db.pool)
    .await?
    .rows_affected();
    if claimed == 0 {
        let unchecked = sqlx::query("SELECT 1 FROM jobs WHERE id = $1 AND verify_state IS NULL")
            .bind(job_id)
            .fetch_optional(&state.db.pool)
            .await?
            .is_some();
        if unchecked {
            after_backup(state, job_id).await?;
        }
        return Ok(());
    }
    let Some(target) = load_target(state, job_id).await? else {
        set_verify_state(state, job_id, None).await?;
        return Ok(());
    };
    let verdict = {
        let _sweep = SWEEP.read().await;
        verify_job(state, job_id, &target).await
    };
    match verdict {
        Ok(Verdict::Verified) => {
            set_verify_state(state, job_id, Some("verified")).await?;
            after_backup(state, job_id).await
        }
        Ok(Verdict::Failed(reason)) => {
            fail_verification(state, job_id, &target.snapshot_id, &reason).await
        }
        Ok(Verdict::Retry(error)) => retry_or_give_up(state, job_id, &target, &error).await,
        Err(error) => retry_or_give_up(state, job_id, &target, &format!("{error:#}")).await,
    }
}

async fn load_target(state: &AppState, job_id: &str) -> anyhow::Result<Option<Target>> {
    let Some(row) = sqlx::query(
        "SELECT j.snapshot_id, j.host_id, a.repository_id FROM jobs j \
         JOIN agents a ON a.id = j.host_id WHERE j.id = $1",
    )
    .bind(job_id)
    .fetch_optional(&state.db.pool)
    .await?
    else {
        return Ok(None);
    };
    let Some(snapshot_id) = row.try_get::<Option<String>, _>("snapshot_id")? else {
        return Ok(None);
    };
    Ok(Some(Target {
        snapshot_id,
        host_id: row.try_get("host_id")?,
        repository_id: row.try_get("repository_id")?,
    }))
}

async fn set_verify_state(
    state: &AppState,
    job_id: &str,
    value: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE jobs SET verify_state = $1 WHERE id = $2 AND verify_state = 'running'")
        .bind(value)
        .bind(job_id)
        .execute(&state.db.pool)
        .await?;
    Ok(())
}

async fn ensure_catalog_row(
    state: &AppState,
    job_id: &str,
    target: &Target,
) -> anyhow::Result<Option<Verdict>> {
    let link = JobLink {
        job_id,
        host_id: &target.host_id,
    };
    match catalog::ingest_snapshot(
        state,
        &target.repository_id,
        &target.snapshot_id,
        Some(link),
    )
    .await
    {
        Ok(_) => Ok(None),
        Err(error) if error.is::<catalog::LinkRefused>() => Ok(Some(Verdict::Failed(
            "the snapshot belongs to another host or job".into(),
        ))),
        Err(error) => match error.downcast_ref::<lodestore_format::Error>() {
            Some(inner) if is_data_error(inner) => Ok(Some(Verdict::Failed(inner.to_string()))),
            _ => Ok(Some(Verdict::Retry(format!("{error:#}")))),
        },
    }
}

async fn verify_job(state: &AppState, job_id: &str, target: &Target) -> anyhow::Result<Verdict> {
    if let Some(verdict) = ensure_catalog_row(state, job_id, target).await? {
        return Ok(verdict);
    }
    let repository = &target.repository_id;
    if let Err(error) = ensure_upgraded(state, repository).await {
        let error = format!("{error:#}");
        tracing::warn!(%job_id, %error, "the repository is not upgraded yet, verification waits");
        return Ok(Verdict::Retry(error));
    }
    let source = open_repository(state, repository)
        .await
        .context("opening the repository")?;
    let snapshot: BlobId = parse_hex32(&target.snapshot_id).context("malformed snapshot id")?;
    let (store, keys) = (&source.store, &source.keys);

    let attested = match read_attestations(store, keys).await {
        Ok(set) if set.incomplete => {
            return Ok(Verdict::Retry(
                "the attestations changed while they were read".into(),
            ));
        }
        Ok(set) => set.shards,
        Err(error) => return Ok(classify(&error)),
    };
    let refs = match snapshot_index_refs(store, keys, &snapshot).await {
        Ok(refs) => refs,
        Err(error) => return Ok(classify(&error)),
    };
    let mut verified = load_verified(state, repository).await?;
    for shard in &refs {
        if verified.contains(shard) {
            continue;
        }
        match verify_shard(store, keys, shard).await {
            Ok(Some(_)) => {
                record_verified(state, repository, &[*shard]).await?;
                verified.insert(*shard);
            }
            Ok(None) => {}
            Err(error) => return Ok(classify(&error)),
        }
    }
    let check = match check_snapshot_resolves(store, keys, &snapshot, &verified, &attested).await {
        Ok(check) => check,
        Err(error) => return Ok(classify(&error)),
    };
    record_verified(state, repository, &check.newly_verified).await?;
    verified.extend(check.newly_verified.iter().copied());

    let candidates: HashSet<BlobId> = refs
        .iter()
        .chain(&check.newly_verified)
        .filter(|id| verified.contains(*id) && !attested.contains(*id))
        .copied()
        .collect();
    // An attested shard's `supersedes` hides other shards from readers and
    // cleanup; opened frames say nothing about whether that list is honest.
    let mut ids = Vec::with_capacity(candidates.len());
    for id in candidates {
        match supersedes_nothing(store, keys, &id).await {
            Ok(true) => ids.push(id),
            Ok(false) => tracing::warn!(
                %job_id,
                shard = %hex32(&id),
                "not attesting a host's shard that supersedes others"
            ),
            Err(error) => return Ok(classify(&error)),
        }
    }
    if !ids.is_empty() {
        if let Err(error) =
            put_attestation(store, keys, &ids, &[], Vec::new(), &mut Entropy::os()).await
        {
            return Ok(Verdict::Retry(error.to_string()));
        }
    }
    Ok(Verdict::Verified)
}

async fn supersedes_nothing<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    id: &BlobId,
) -> lodestore_format::Result<bool> {
    let bytes = store.get(&index_object_key(id)).await?;
    Ok(open_shard(keys.k_idx(), keys.repo_id(), id, &bytes)?
        .supersedes
        .is_empty())
}

pub async fn fail_verification(
    state: &AppState,
    job_id: &str,
    snapshot_id: &str,
    reason: &str,
) -> anyhow::Result<()> {
    let message = format!(
        "The server could not verify the data of this backup: {reason}. Its snapshot may not \
         restore. Run the backup again; if it fails again, check the repository."
    );
    let mut tx = state.db.pool.begin().await?;
    sqlx::query(
        "UPDATE jobs SET state = 'failed', verify_state = 'failed', error_code = $1, \
         error_message = $2 WHERE id = $3",
    )
    .bind(VERIFICATION_FAILED)
    .bind(&message)
    .bind(job_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE snapshots SET unverified_reason = $1 WHERE id = $2 AND job_id = $3")
        .bind(&message)
        .bind(snapshot_id)
        .bind(job_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    tracing::error!(%job_id, %snapshot_id, %reason, "backup verification failed");
    if let Err(error) = append_event(state, job_id, "error", VERIFICATION_FAILED, &message).await {
        tracing::warn!(%error, job_id, "writing a verification log entry failed");
    }
    Ok(())
}

async fn retry_or_give_up(
    state: &AppState,
    job_id: &str,
    target: &Target,
    error: &str,
) -> anyhow::Result<()> {
    let window_ms = i64::try_from(VERIFY_RETRY_WINDOW.as_millis()).unwrap_or(i64::MAX);
    let mut first = first_retry_at(state, job_id).await?;
    if first.is_none() {
        let message = format!(
            "The server could not check this backup yet and will keep trying for 1 hour: {error}"
        );
        if let Err(error) =
            append_event(state, job_id, "warning", "verification_retry", &message).await
        {
            tracing::warn!(%error, job_id, "writing a verification log entry failed");
        }
        first = first_retry_at(state, job_id).await?;
        if first.is_none() {
            // The log is full; the window still has to start on the server's clock.
            record_retry_past_log_limit(state, job_id, &message).await?;
            first = Some(now_ms());
        }
    }
    let started = first.unwrap_or_else(now_ms);
    if now_ms() - started < window_ms {
        tracing::warn!(%job_id, %error, "verification will be retried");
        return set_verify_state(state, job_id, Some("pending")).await;
    }
    let _ = ensure_catalog_row(state, job_id, target).await;
    fail_verification(
        state,
        job_id,
        &target.snapshot_id,
        &format!("the server could not check this backup within 1 hour: {error}"),
    )
    .await
}

async fn first_retry_at(state: &AppState, job_id: &str) -> anyhow::Result<Option<i64>> {
    let row = sqlx::query(
        "SELECT MIN(at) AS first_at FROM job_events WHERE job_id = $1 AND kind = 'verification_retry'",
    )
    .bind(job_id)
    .fetch_one(&state.db.pool)
    .await?;
    Ok(row.try_get("first_at")?)
}

async fn record_retry_past_log_limit(
    state: &AppState,
    job_id: &str,
    message: &str,
) -> anyhow::Result<()> {
    let mut tx = state.db.pool.begin().await?;
    let seq: i64 =
        sqlx::query("UPDATE jobs SET log_seq = log_seq + 1 WHERE id = $1 RETURNING log_seq")
            .bind(job_id)
            .fetch_one(&mut *tx)
            .await?
            .try_get("log_seq")?;
    sqlx::query(
        "INSERT INTO job_events (job_id, seq, at, level, kind, message) \
         VALUES ($1, $2, $3, 'warning', 'verification_retry', $4) ON CONFLICT DO NOTHING",
    )
    .bind(job_id)
    .bind(seq)
    .bind(now_ms())
    .bind(message)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
