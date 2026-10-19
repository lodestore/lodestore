//! Replacing the index shards that name deleted packs, and the many small
//! ones, with a few compact shards.

use std::collections::HashSet;

use super::mark::{LiveSet, ShardSet, load_shards};
use crate::crypto::{BlobId, Entropy};
use crate::error::{Error, Result};
use crate::index::{
    REBUILD_SHARD_PACKS, hex32, index_object_key, listed_packs, seal_shard, seal_shard_superseding,
};
use crate::keys::ReadKeys;
use crate::pb;
use crate::store::{BlobStore, put_named};

pub const SMALL_SHARD_PACKS: usize = 512;

#[derive(Debug, Clone, Default)]
pub struct CompactPlan {
    /// The shards the new ones replace.
    pub rewrite: Vec<BlobId>,
    /// What the new shards describe: the packs of `rewrite` that are not
    /// candidates.
    pub records: Vec<pb::PackRecord>,
}

impl CompactPlan {
    pub fn is_empty(&self) -> bool {
        self.rewrite.is_empty()
    }
}

fn is_candidate(candidates: &HashSet<BlobId>, pack_id: &[u8]) -> bool {
    BlobId::try_from(pack_id).is_ok_and(|id| candidates.contains(&id))
}

fn names_candidate(shard: &pb::IndexShard, candidates: &HashSet<BlobId>) -> bool {
    shard
        .packs
        .iter()
        .any(|p| is_candidate(candidates, &p.pack_id))
}

/// Picks the shards to rewrite: every shard that names a candidate pack and
/// every shard of fewer than [`SMALL_SHARD_PACKS`] packs. Large shards that
/// name no candidate stay as they are. A single small clean shard is left
/// alone too: rewriting it would only change its name. Shards without packs,
/// such as the one that committed the last compaction, are always dropped.
pub fn compact_plan(shards: &ShardSet, candidates: &HashSet<BlobId>) -> CompactPlan {
    let mut plan = CompactPlan::default();
    let mut empty = Vec::new();
    let mut seen = HashSet::new();
    let mut dirty = false;
    for (id, shard) in &shards.shards {
        if shard.packs.is_empty() {
            empty.push(*id);
            continue;
        }
        let has_candidate = names_candidate(shard, candidates);
        if !has_candidate && shard.packs.len() >= SMALL_SHARD_PACKS {
            continue;
        }
        dirty |= has_candidate;
        plan.rewrite.push(*id);
        for record in &shard.packs {
            let keep = !is_candidate(candidates, &record.pack_id);
            if keep && seen.insert(record.pack_id.clone()) {
                plan.records.push(record.clone());
            }
        }
    }
    if plan.rewrite.len() == 1 && !dirty {
        plan = CompactPlan::default();
    }
    plan.rewrite.extend(empty);
    plan.rewrite.sort();
    plan
}

/// Writes the new shards, checks that the index they leave behind once the
/// shards of `plan.rewrite` are gone still places every live blob outside
/// the candidates, and only then writes one empty shard that supersedes
/// `plan.rewrite`. Returns the ids of the shards written. Until that last
/// shard is stored the old shards keep being preferred, so a compaction cut
/// short leaves only harmless duplicates. Old shards are left in place.
pub async fn write_compacted<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    plan: &CompactPlan,
    live: &LiveSet,
    candidates: &HashSet<BlobId>,
    entropy: &mut Entropy,
) -> Result<Vec<BlobId>> {
    let mut written = Vec::new();
    for chunk in plan.records.chunks(REBUILD_SHARD_PACKS) {
        let (id, bytes) = seal_shard(keys.k_idx(), keys.repo_id(), chunk.to_vec(), entropy)?;
        put_named(store, &index_object_key(&id), bytes).await?;
        written.push(id);
    }
    let gone: HashSet<BlobId> = plan.rewrite.iter().copied().collect();
    check_reachable(store, keys, live, candidates, &gone).await?;
    if !written.is_empty() {
        let (id, bytes) = seal_shard_superseding(
            keys.k_idx(),
            keys.repo_id(),
            Vec::new(),
            plan.rewrite.clone(),
            entropy,
        )?;
        put_named(store, &index_object_key(&id), bytes).await?;
        written.push(id);
    }
    Ok(written)
}

/// Reads the index back from the bucket, leaving out the shards of `gone`,
/// and checks that every live blob has a location in a pack that the store
/// lists and that is not going to be deleted.
pub async fn check_reachable<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    live: &LiveSet,
    candidates: &HashSet<BlobId>,
    gone: &HashSet<BlobId>,
) -> Result<()> {
    let mut shards = load_shards(store, keys).await?;
    shards.shards.retain(|(id, _)| !gone.contains(id));
    let mut index = shards.index(candidates)?;
    index.retain_packs(&listed_packs(store).await?);
    for (id, blob_type) in live.iter() {
        if index.get(id, *blob_type).is_none() {
            return Err(Error::corrupt(
                "index",
                format!(
                    "a live blob has no location outside the packs marked for deletion ({})",
                    hex32(id)
                ),
            ));
        }
    }
    Ok(())
}
