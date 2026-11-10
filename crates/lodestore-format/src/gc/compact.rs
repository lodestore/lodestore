//! Replacing the index shards that name deleted packs, and the many small
//! ones, with a few compact shards.

use std::collections::{HashMap, HashSet};

use prost::Message;

use super::mark::{LiveSet, ShardSet, load_shards};
use crate::attest::put_attestation;
use crate::crypto::{BlobId, Entropy};
use crate::error::{Error, Result};
use crate::index::{
    SHARD_TARGET_SIZE, hex32, index_object_key, listed_packs, seal_shard, seal_shard_superseding,
    split_records,
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
    /// candidates, by the trust of the shard each record came from. A record
    /// found in several shards sits in the most trusted group only.
    /// Records of shards whose frames the server opened.
    pub verified_records: Vec<pb::PackRecord>,
    /// Records of shards that are attested but whose frames were not opened.
    pub attested_records: Vec<pb::PackRecord>,
    /// Records of shards nobody vouches for.
    pub other_records: Vec<pb::PackRecord>,
}

/// The shards a compaction wrote, by what the server may claim about them.
#[derive(Debug, Clone, Default)]
pub struct Compacted {
    pub verified: Vec<BlobId>,
    pub attested: Vec<BlobId>,
    pub other: Vec<BlobId>,
    pub superseding: Option<BlobId>,
}

impl CompactPlan {
    pub fn is_empty(&self) -> bool {
        self.rewrite.is_empty()
    }

    pub fn record_count(&self) -> usize {
        self.verified_records.len() + self.attested_records.len() + self.other_records.len()
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
/// alone too: rewriting it would only change its name. A shard larger than
/// `MAX_SHARD_SIZE` is always rewritten, into shards under the limit. Shards without packs,
/// such as the one that committed the last compaction, are always dropped.
pub fn compact_plan(shards: &ShardSet, candidates: &HashSet<BlobId>) -> CompactPlan {
    let mut plan = CompactPlan::default();
    let mut empty = Vec::new();
    let mut order = Vec::new();
    let mut seen: HashMap<[u8; 32], (u8, pb::PackRecord)> = HashMap::new();
    let mut dirty = false;
    for (id, shard) in &shards.shards {
        if shard.packs.is_empty() {
            empty.push(*id);
            continue;
        }
        let has_candidate = names_candidate(shard, candidates);
        let oversized = shards.oversized.contains(id);
        if !has_candidate && !oversized && shard.packs.len() >= SMALL_SHARD_PACKS {
            continue;
        }
        dirty |= has_candidate || oversized;
        plan.rewrite.push(*id);
        let level = if shards.verified.contains(id) {
            0
        } else {
            u8::from(!shards.attested.contains(id)) + 1
        };
        for record in &shard.packs {
            if is_candidate(candidates, &record.pack_id) {
                continue;
            }
            let key = *blake3::hash(&record.encode_to_vec()).as_bytes();
            match seen.get_mut(&key) {
                Some(held) => held.0 = held.0.min(level),
                None => {
                    order.push(key);
                    seen.insert(key, (level, record.clone()));
                }
            }
        }
    }
    for key in order {
        if let Some((level, record)) = seen.remove(&key) {
            match level {
                0 => plan.verified_records.push(record),
                1 => plan.attested_records.push(record),
                _ => plan.other_records.push(record),
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
/// `plan.rewrite`. Returns the ids of the shards written by group. The
/// verified and attested groups and the superseding shard are attested in one
/// object once the last of them is stored; the other group is not, so no
/// record the server has not vouched for becomes attested. Until that last
/// shard is stored the old shards keep being preferred, so a compaction cut
/// short leaves only harmless duplicates. Old shards are left in place.
pub async fn write_compacted<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    plan: &CompactPlan,
    live: &LiveSet,
    candidates: &HashSet<BlobId>,
    entropy: &mut Entropy,
) -> Result<Compacted> {
    let mut out = Compacted::default();
    for (records, ids) in [
        (&plan.verified_records, &mut out.verified),
        (&plan.attested_records, &mut out.attested),
        (&plan.other_records, &mut out.other),
    ] {
        for group in split_records(records.clone(), SHARD_TARGET_SIZE) {
            let (id, bytes) = seal_shard(keys.k_idx(), keys.repo_id(), group, entropy)?;
            put_named(store, &index_object_key(&id), bytes).await?;
            ids.push(id);
        }
    }
    let gone: HashSet<BlobId> = plan.rewrite.iter().copied().collect();
    check_reachable(store, keys, live, candidates, &gone).await?;
    if plan.record_count() > 0 {
        let (id, bytes) = seal_shard_superseding(
            keys.k_idx(),
            keys.repo_id(),
            Vec::new(),
            plan.rewrite.clone(),
            entropy,
        )?;
        put_named(store, &index_object_key(&id), bytes).await?;
        let trusted: Vec<BlobId> = out
            .verified
            .iter()
            .chain(&out.attested)
            .copied()
            .chain([id])
            .collect();
        put_attestation(store, keys, &trusted, &[], Vec::new(), entropy).await?;
        out.superseding = Some(id);
    }
    Ok(out)
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
