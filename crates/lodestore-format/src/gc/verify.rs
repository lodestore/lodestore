//! The second look before anything is deleted.

use std::collections::{BTreeSet, HashSet};

use super::mark::{LiveSet, MarkOutcome, ShardSet, load_shards, mark};
use crate::crypto::BlobId;
use crate::error::{Error, Result};
use crate::index::{hex32, listed_packs};
use crate::keys::ReadKeys;
use crate::pb;
use crate::store::BlobStore;

#[derive(Debug)]
pub struct Verified {
    /// The candidates that may still be deleted.
    pub candidates: HashSet<BlobId>,
    /// Candidates that a snapshot present now needs and that were taken off
    /// the list.
    pub resurrected: Vec<BlobId>,
    pub live: LiveSet,
    pub shards: ShardSet,
}

/// Lists the snapshots again and walks every one of them, whenever it was
/// written. A blob that only a candidate pack holds keeps that pack; a blob
/// that no pack holds stops the cycle. Only locations in packs the store
/// lists count.
pub async fn verify<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    config: &pb::Config,
    candidates: &HashSet<BlobId>,
    progress: &(dyn Fn(u64, u64) + Sync),
) -> Result<Verified> {
    let shards = load_shards(store, keys).await?;
    let listed = listed_packs(store).await?;
    let mut full = shards.index(&HashSet::new())?;
    full.retain_packs(&listed);
    let mut outside = shards.index(candidates)?;
    outside.retain_packs(&listed);
    let MarkOutcome { live, .. } = mark(store, keys, config, &full, progress).await?;

    let mut saved = BTreeSet::new();
    for (id, blob_type) in live.iter() {
        if outside.get(id, *blob_type).is_some() {
            continue;
        }
        match full.get(id, *blob_type) {
            Some(loc) => {
                saved.insert(loc.pack_id);
            }
            None => {
                return Err(Error::corrupt(
                    format!("blob {}", hex32(id)),
                    "is needed by a snapshot but no pack holds it",
                ));
            }
        }
    }
    let mut candidates = candidates.clone();
    for pack in &saved {
        candidates.remove(pack);
    }
    Ok(Verified {
        candidates,
        resurrected: saved.into_iter().collect(),
        live,
        shards,
    })
}
