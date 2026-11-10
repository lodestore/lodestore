//! Deleting: old index shards first, then the packs, then whatever else the
//! collection left behind. Every step can be repeated.

use std::collections::HashSet;

use super::mark::load_shards;
use crate::crypto::BlobId;
use crate::error::{Error, Result};
use crate::index::index_object_key;
use crate::keys::ReadKeys;
use crate::pack::pack_object_key;
use crate::store::{BlobDelete, BlobError, BlobPurge, BlobStore, PurgeOutcome, RetainedVersion};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepOutcome {
    pub deleted: u64,
    pub bytes_freed: u64,
    /// Versions the store keeps because they are locked, with their keys.
    pub retained: Vec<(String, RetainedVersion)>,
    /// `(key, reason)` of objects that could not be deleted.
    pub failed: Vec<(String, String)>,
}

impl SweepOutcome {
    pub fn merge(&mut self, other: SweepOutcome) {
        self.deleted += other.deleted;
        self.bytes_freed += other.bytes_freed;
        self.retained.extend(other.retained);
        self.failed.extend(other.failed);
    }
}

/// Deletes `key` the ordinary way, so that a versioned bucket hides it from
/// listings at once, then purges every version of it. A locked version is
/// reported as retained. `size` is what is counted as freed when the store
/// does not say.
pub async fn sweep_object<S: BlobDelete + BlobPurge>(
    store: &S,
    key: &str,
    size: u64,
) -> SweepOutcome {
    let removed = match store.delete(key).await {
        Ok(()) => true,
        Err(BlobError::NotFound) => false,
        Err(e) => {
            return SweepOutcome {
                failed: vec![(key.to_owned(), e.to_string())],
                ..SweepOutcome::default()
            };
        }
    };
    tally(key, size, store.purge(key).await, removed)
}

/// Purges every version of `key` without hiding it first: for a key whose
/// current entry is a delete marker already, so that a locked version does
/// not collect one more marker on every pass.
pub async fn purge_object<S: BlobPurge>(store: &S, key: &str, size: u64) -> SweepOutcome {
    tally(key, size, store.purge(key).await, false)
}

/// Only a version that held data counts as deleted: a purge that found
/// nothing but delete markers freed nothing.
fn tally(key: &str, size: u64, purged: PurgeOutcome, removed: bool) -> SweepOutcome {
    let mut out = SweepOutcome::default();
    match purged {
        PurgeOutcome::Purged { versions, bytes } => {
            if versions > 0 {
                out.deleted = 1;
                out.bytes_freed = if bytes > 0 { bytes } else { size };
            }
        }
        PurgeOutcome::NotFound => {
            out.deleted = u64::from(removed);
            out.bytes_freed = if removed { size } else { 0 };
        }
        PurgeOutcome::Retained {
            purged_versions,
            purged_bytes,
            retained,
        } => {
            out.deleted = u64::from(removed || purged_versions > 0);
            out.bytes_freed = purged_bytes;
            out.retained
                .extend(retained.into_iter().map(|v| (key.to_owned(), v)));
        }
        PurgeOutcome::Failed(e) => out.failed.push((key.to_owned(), e.to_string())),
    }
    out
}

// ponytail: objects go one request at a time; run a bounded number at once
// (JoinSet behind a Semaphore) when sweeping a very large bucket gets slow.
pub async fn sweep_shards<S: BlobDelete + BlobPurge>(store: &S, shards: &[BlobId]) -> SweepOutcome {
    let mut out = SweepOutcome::default();
    for id in shards {
        out.merge(sweep_object(store, &index_object_key(id), 0).await);
    }
    out
}

/// Lists the index again and fails if any shard still in the bucket, even a
/// superseded one that current readers ignore, names a pack of `candidates`.
/// Agents that predate `supersedes` read it.
/// Returns the `(key, reason)` of the shards it could not read. Writers that
/// predate the shard limit read those whole and may still reuse a candidate,
/// so the caller decides whether the packs may go.
pub async fn confirm_unreferenced<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    candidates: &HashSet<BlobId>,
) -> Result<Vec<(String, String)>> {
    let shards = load_shards(store, keys).await?;
    for (id, shard) in &shards.shards {
        let named = shard
            .packs
            .iter()
            .any(|p| BlobId::try_from(p.pack_id.as_slice()).is_ok_and(|p| candidates.contains(&p)));
        if named {
            return Err(Error::corrupt(
                index_object_key(id),
                "still names a pack marked for deletion",
            ));
        }
    }
    Ok(shards.skipped)
}

/// Deletes packs given with their sizes.
pub async fn sweep_packs<S: BlobDelete + BlobPurge>(
    store: &S,
    packs: &[(BlobId, u64)],
) -> SweepOutcome {
    let mut out = SweepOutcome::default();
    for (id, size) in packs {
        out.merge(sweep_object(store, &pack_object_key(id), *size).await);
    }
    out
}
