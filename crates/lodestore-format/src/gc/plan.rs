//! Choosing the packs a collection will delete or repack.

use std::cmp::Ordering;
use std::collections::HashMap;

use super::mark::{LiveSet, ShardSet};
use crate::crypto::BlobId;
use crate::error::{Error, Result};
use crate::index::{Index, hex32, parse_hex32};
use crate::pack::{ENTRY_LEN, PACK_FIXED_LEN, pack_object_key, parse_entries};
use crate::store::ListEntry;

pub const MAX_CANDIDATES: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackStat {
    pub pack_id: BlobId,
    pub size: u64,
    /// Bytes of the blobs the index places in this pack that a snapshot needs.
    pub live_bytes: u64,
    pub blob_type: u8,
    pub modified_ms: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct PlanOptions {
    pub now_ms: i64,
    /// Packs written more recently than this are left alone.
    pub min_age_ms: i64,
    /// Packs with a smaller share of live bytes, in percent, are repacked.
    pub repack_below_pct: u8,
    /// The packs chosen for repacking add up to at most this many bytes.
    pub max_repack_bytes: u64,
    pub max_candidates: usize,
}

#[derive(Debug, Clone, Default)]
pub struct GcPlan {
    pub delete: Vec<PackStat>,
    pub repack: Vec<PackStat>,
    pub packs_total: u64,
    pub bytes_total: u64,
    /// `(object key, size)` of packs no index shard mentions.
    pub unindexed: Vec<(String, u64)>,
    /// Packs that qualified but are too young to touch.
    pub young: u64,
}

impl GcPlan {
    pub fn bytes_freed(&self) -> u64 {
        let dead = |p: &PackStat| p.size.saturating_sub(p.live_bytes);
        self.delete.iter().map(|p| p.size).sum::<u64>() + self.repack.iter().map(dead).sum::<u64>()
    }

    /// Packs to download plus live bytes to upload again.
    pub fn repack_transfer_bytes(&self) -> u64 {
        self.repack.iter().map(|p| p.size + p.live_bytes).sum()
    }
}

fn pack_missing(pack_id: &BlobId) -> Error {
    Error::corrupt(
        pack_object_key(pack_id),
        "is named by the index but missing from the store",
    )
}

/// Compares `a.live / a.size` with `b.live / b.size` without dividing.
fn by_live_share(a: &PackStat, b: &PackStat) -> Ordering {
    let lhs = u128::from(a.live_bytes) * u128::from(b.size);
    let rhs = u128::from(b.live_bytes) * u128::from(a.size);
    lhs.cmp(&rhs).then(a.pack_id.cmp(&b.pack_id))
}

/// Sorts the packs of the bucket into delete / repack / leave alone. A blob
/// counts as live in the pack the index places it in; copies in other packs
/// are dead bytes of those packs.
pub fn plan(
    shards: &ShardSet,
    index: &Index,
    live: &LiveSet,
    packs_listing: &[ListEntry],
    opts: &PlanOptions,
) -> Result<GcPlan> {
    let mut listed = HashMap::new();
    for entry in packs_listing {
        let id = entry.key.rsplit('/').next().and_then(parse_hex32);
        if let Some(id) = id.filter(|id| pack_object_key(id) == entry.key) {
            listed.insert(id, entry);
        }
    }

    let mut known: HashMap<BlobId, (u8, usize)> = HashMap::new();
    for (shard_id, shard) in &shards.shards {
        for record in &shard.packs {
            let bad = |reason: &str| {
                Error::corrupt(crate::index::index_object_key(shard_id), reason.to_owned())
            };
            let pack_id: BlobId = record
                .pack_id
                .as_slice()
                .try_into()
                .map_err(|_| bad("pack id must be 32 bytes"))?;
            if known.contains_key(&pack_id) {
                continue;
            }
            let entries = parse_entries(&record.entries)?;
            let blob_type = entries.first().map_or(0, |e| e.blob_type);
            known.insert(pack_id, (blob_type, entries.len()));
        }
    }

    let mut live_bytes: HashMap<BlobId, u64> = HashMap::new();
    for (id, blob_type) in live.iter() {
        let chosen = index.get(id, *blob_type).ok_or_else(|| {
            Error::corrupt(
                format!("blob {}", hex32(id)),
                "is live but no index shard describes it",
            )
        })?;
        let loc = if listed.contains_key(&chosen.pack_id) {
            *chosen
        } else {
            // A shard from a write-key holder can name a pack that is not
            // there ahead of the real one; only a blob with no listed copy
            // is missing.
            index
                .locations(id, *blob_type)
                .into_iter()
                .find(|l| listed.contains_key(&l.pack_id))
                .ok_or_else(|| pack_missing(&chosen.pack_id))?
        };
        *live_bytes.entry(loc.pack_id).or_default() += u64::from(loc.enc_len);
    }

    let mut out = GcPlan {
        packs_total: listed.len() as u64,
        bytes_total: listed.values().map(|e| e.size).sum(),
        ..Default::default()
    };
    for (id, entry) in &listed {
        if !known.contains_key(id) {
            out.unindexed.push((entry.key.clone(), entry.size));
        }
    }
    out.unindexed.sort();

    let mut dead = Vec::new();
    let mut sparse = Vec::new();
    for (pack_id, (blob_type, entry_count)) in &known {
        let Some(entry) = listed.get(pack_id) else {
            continue;
        };
        let stat = PackStat {
            pack_id: *pack_id,
            size: entry.size,
            live_bytes: live_bytes.get(pack_id).copied().unwrap_or(0),
            blob_type: *blob_type,
            modified_ms: entry.modified_ms,
        };
        let framed = stat
            .size
            .saturating_sub((PACK_FIXED_LEN + entry_count * ENTRY_LEN) as u64);
        let candidate = if stat.live_bytes == 0 {
            &mut dead
        } else if u128::from(stat.live_bytes) * 100
            < u128::from(opts.repack_below_pct) * u128::from(framed)
        {
            &mut sparse
        } else {
            continue;
        };
        let old_enough = stat
            .modified_ms
            .is_some_and(|m| opts.now_ms.saturating_sub(m) >= opts.min_age_ms);
        if old_enough {
            candidate.push(stat);
        } else {
            out.young += 1;
        }
    }

    let cap = opts.max_candidates.min(MAX_CANDIDATES);
    dead.sort_by(|a, b| b.size.cmp(&a.size).then(a.pack_id.cmp(&b.pack_id)));
    dead.truncate(cap);
    sparse.sort_by(by_live_share);
    let mut budget = opts.max_repack_bytes;
    for stat in sparse {
        if dead.len() + out.repack.len() >= cap {
            break;
        }
        if stat.size <= budget {
            budget -= stat.size;
            out.repack.push(stat);
        }
    }
    out.delete = dead;
    Ok(out)
}
