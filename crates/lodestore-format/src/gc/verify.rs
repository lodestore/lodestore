//! The second look before anything is deleted.

use std::collections::{BTreeSet, HashMap, HashSet};

use super::mark::{LiveSet, MarkOutcome, ShardSet, load_shards, mark};
use crate::crypto::{BlobId, Key32};
use crate::error::{Error, Result};
use crate::index::{BlobLocation, hex32, listed_packs};
use crate::keys::ReadKeys;
use crate::pack::{PACK_PREFIX_LEN, pack_object_key, parse_entries, parse_prefix};
use crate::pb;
use crate::reader::open_chunk;
use crate::store::{BlobError, BlobStore};

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

/// The candidate packs that hold a live blob according to the raw records of
/// any shard, attested or not, superseded or not: a record the index drops
/// may still be the true one.
fn candidate_holders(
    shards: &ShardSet,
    live: &LiveSet,
    candidates: &HashSet<BlobId>,
    listed: &HashMap<BlobId, u64>,
) -> Result<HashMap<(BlobId, u8), BTreeSet<BlobId>>> {
    let mut out: HashMap<(BlobId, u8), BTreeSet<BlobId>> = HashMap::new();
    for (_, shard) in &shards.shards {
        for record in &shard.packs {
            let Ok(pack_id) = BlobId::try_from(record.pack_id.as_slice()) else {
                continue;
            };
            if !candidates.contains(&pack_id) || listed.get(&pack_id) != Some(&record.pack_size) {
                continue;
            }
            for e in parse_entries(&record.entries)? {
                if live.contains(&e.blob_id, e.blob_type) {
                    out.entry((e.blob_id, e.blob_type))
                        .or_default()
                        .insert(pack_id);
                }
            }
        }
    }
    Ok(out)
}

async fn open_at<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    loc: &BlobLocation,
    id: &BlobId,
    blob_type: u8,
    pack_keys: &mut HashMap<BlobId, Key32>,
) -> Result<()> {
    let name = pack_object_key(&loc.pack_id);
    if !pack_keys.contains_key(&loc.pack_id) {
        let prefix = store.get_range(&name, 0..PACK_PREFIX_LEN as u64).await?;
        let key = parse_prefix(&prefix)?.open_key(keys.hpke_private_key(), keys.repo_id())?;
        pack_keys.insert(loc.pack_id, key);
    }
    let Some(pack_key) = pack_keys.get(&loc.pack_id) else {
        return Err(Error::Crypto);
    };
    let start = u64::from(loc.offset);
    let frame = store
        .get_range(&name, start..start + u64::from(loc.enc_len))
        .await?;
    if frame.len() != loc.enc_len as usize {
        return Err(Error::corrupt(name, "shorter than the index says"));
    }
    open_chunk(
        pack_key,
        keys.repo_id(),
        blob_type,
        id,
        &frame,
        loc.ulen,
        keys.k_id(),
    )
    .map(drop)
}

/// Whether one of `locations` holds a frame that opens as the blob. A copy
/// that does not open or whose pack is gone counts as no copy; any other
/// store error stops the cycle with nothing deleted.
async fn survivor_opens<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    locations: &[BlobLocation],
    id: &BlobId,
    blob_type: u8,
    pack_keys: &mut HashMap<BlobId, Key32>,
) -> Result<bool> {
    for loc in locations {
        match open_at(store, keys, loc, id, blob_type, pack_keys).await {
            Ok(()) => return Ok(true),
            Err(
                Error::Corrupt { .. }
                | Error::Crypto
                | Error::UnsupportedVersion { .. }
                | Error::Store(BlobError::NotFound),
            ) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}

/// Lists the snapshots again and walks every one of them, whenever it was
/// written. A live blob that a candidate pack holds lets the pack go only if
/// a copy outside the candidates opens; otherwise every such pack stays. A
/// blob that no pack holds stops the cycle. Only locations in packs the
/// store lists count.
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

    let holders = candidate_holders(&shards, &live, candidates, &listed)?;
    let mut pack_keys = HashMap::new();
    let mut saved = BTreeSet::new();
    for (id, blob_type) in live.iter() {
        if let Some(held) = holders.get(&(*id, *blob_type)) {
            if !held.is_subset(&saved)
                && !survivor_opens(
                    store,
                    keys,
                    &outside.locations(id, *blob_type),
                    id,
                    *blob_type,
                    &mut pack_keys,
                )
                .await?
            {
                saved.extend(held.iter().copied());
            }
            continue;
        }
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
