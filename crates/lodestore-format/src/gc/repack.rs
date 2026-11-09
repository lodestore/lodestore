//! Moving the live blobs of sparse packs into new packs.

use std::collections::{HashMap, HashSet};

use bytes::Bytes;

use super::Survey;
use super::plan::PackStat;
use crate::attest::put_attestation;
use crate::crypto::{BLOB_TYPE_TREE, BlobId, Entropy, Key32};
use crate::error::{Error, Result};
use crate::index::{
    BlobLocation, SHARD_TARGET_SIZE, hex32, index_object_key, seal_shard, split_records,
};
use crate::keys::ReadKeys;
use crate::pack::{
    PACK_MAX_SIZE, PACK_PREFIX_LEN, PACK_TARGET_SIZE, PackBuilder, pack_object_key, parse_prefix,
};
use crate::pb;
use crate::reader::open_chunk;
use crate::store::{BlobError, BlobStore, put_named, read_bounded};

#[derive(Debug, Clone, Default)]
pub struct RepackOutcome {
    /// The shards that describe `new_packs`; empty when nothing was copied.
    pub shards: Vec<BlobId>,
    pub new_packs: Vec<BlobId>,
    pub replaced: Vec<BlobId>,
    /// Packs left in place, with the reason: a frame or prefix that does not
    /// open, or a size that differs from the listing.
    pub skipped: Vec<(BlobId, String)>,
    pub bytes_read: u64,
    pub bytes_written: u64,
}

/// Rewrites the live blobs of every pack the plan chose for repacking into
/// new packs and describes them with one new shard, which the server attests.
/// Each source pack is held in memory alone, next to one open builder per
/// blob type. Nothing is deleted: the replaced packs are only reported. A
/// pack whose live blobs do not all open is skipped, not replaced, and does
/// not stop the others.
pub async fn repack<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    survey: &Survey,
    entropy: &mut Entropy,
    progress: &(dyn Fn(u64, u64) + Sync),
) -> Result<RepackOutcome> {
    let targets: HashSet<BlobId> = survey.plan.repack.iter().map(|p| p.pack_id).collect();
    let mut located: HashMap<BlobId, Vec<(BlobId, BlobLocation)>> = HashMap::new();
    for (id, blob_type) in survey.live.iter() {
        if let Some(loc) = survey.index.get(id, *blob_type)
            && targets.contains(&loc.pack_id)
        {
            located.entry(loc.pack_id).or_default().push((*id, *loc));
        }
    }

    let mut out = RepackOutcome::default();
    let mut records = Vec::new();
    let mut builders: [Option<PackBuilder>; 2] = [None, None];
    let total = survey.plan.repack.len() as u64;
    'packs: for (done, stat) in survey.plan.repack.iter().enumerate() {
        progress(done as u64, total);
        let mut blobs = located.remove(&stat.pack_id).unwrap_or_default();
        if blobs.is_empty() {
            out.replaced.push(stat.pack_id);
            continue;
        }
        blobs.sort_by_key(|(_, loc)| loc.offset);
        let (bytes, pack_key) = match fetch_pack(store, keys, stat).await? {
            Ok(fetched) => fetched,
            Err(reason) => {
                out.skipped.push((stat.pack_id, reason));
                continue;
            }
        };
        out.bytes_read += stat.size;
        for (id, loc) in blobs {
            let copies = survey.index.locations(&id, loc.blob_type);
            let plain = match open_in_pack(&bytes, &pack_key, keys, &id, &copies, &stat.pack_id) {
                Ok(plain) => plain,
                Err(reason) => {
                    out.skipped.push((stat.pack_id, reason));
                    continue 'packs;
                }
            };
            let slot = usize::from(loc.blob_type == BLOB_TYPE_TREE);
            let builder = match &mut builders[slot] {
                Some(b) => b,
                empty => empty.insert(PackBuilder::new(
                    loc.blob_type,
                    keys.repo_id(),
                    keys.hpke_public_key(),
                    entropy,
                )?),
            };
            builder.add(&id, &plain, entropy)?;
            if builder.size() >= PACK_TARGET_SIZE {
                let full = builders[slot].take().expect("just used");
                seal_pack(store, full, entropy, &mut out, &mut records).await?;
            }
        }
        out.replaced.push(stat.pack_id);
    }
    for builder in builders.into_iter().flatten() {
        seal_pack(store, builder, entropy, &mut out, &mut records).await?;
    }
    if !records.is_empty() {
        for group in split_records(records, SHARD_TARGET_SIZE) {
            let (shard_id, bytes) = seal_shard(keys.k_idx(), keys.repo_id(), group, entropy)?;
            put_named(store, &index_object_key(&shard_id), bytes).await?;
            out.shards.push(shard_id);
        }
        put_attestation(store, keys, &out.shards, &[], Vec::new(), entropy).await?;
    }
    progress(total, total);
    Ok(out)
}

/// The pack and its key, or the reason this pack cannot be rewritten. A pack
/// that is missing, differs in size from the listing or has a prefix that does
/// not open stays where it is; only other store errors stop the cycle.
async fn fetch_pack<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    stat: &PackStat,
) -> Result<std::result::Result<(Bytes, Key32), String>> {
    if stat.size > PACK_MAX_SIZE as u64 {
        return Ok(Err("larger than a pack can be".into()));
    }
    let key = pack_object_key(&stat.pack_id);
    let bytes = match read_bounded(store, &key, Some(stat.size), PACK_MAX_SIZE as u64, "pack").await
    {
        Ok(bytes) => bytes,
        Err(Error::Store(BlobError::NotFound)) => return Ok(Err("missing from the store".into())),
        Err(Error::Corrupt { .. }) => return Ok(Err("larger than a pack can be".into())),
        Err(e) => return Err(e),
    };
    if bytes.len() as u64 != stat.size {
        return Ok(Err("size differs from the listing".into()));
    }
    let Some(prefix) = bytes.get(..PACK_PREFIX_LEN) else {
        return Ok(Err("shorter than its prefix".into()));
    };
    match parse_prefix(prefix).and_then(|p| p.open_key(keys.hpke_private_key(), keys.repo_id())) {
        Ok(key) => Ok(Ok((bytes, key))),
        Err(e @ (Error::Corrupt { .. } | Error::UnsupportedVersion { .. })) => {
            Ok(Err(format!("prefix does not open: {e}")))
        }
        Err(e) => Err(e),
    }
}

/// The plaintext of the first copy of the blob in this pack that opens.
fn open_in_pack(
    bytes: &[u8],
    pack_key: &Key32,
    keys: &ReadKeys,
    id: &BlobId,
    copies: &[BlobLocation],
    pack_id: &BlobId,
) -> std::result::Result<Vec<u8>, String> {
    let mut why = format!("no record of blob {} in it", hex32(id));
    for loc in copies.iter().filter(|l| &l.pack_id == pack_id) {
        let start = loc.offset as usize;
        let Some(frame) = start
            .checked_add(loc.enc_len as usize)
            .and_then(|end| bytes.get(start..end))
        else {
            why = format!("blob {} lies outside it", hex32(id));
            continue;
        };
        match open_chunk(
            pack_key,
            keys.repo_id(),
            loc.blob_type,
            id,
            frame,
            loc.ulen,
            keys.k_id(),
        ) {
            Ok(plain) => return Ok(plain),
            Err(e) => why = e.to_string(),
        }
    }
    Err(why)
}

async fn seal_pack<S: BlobStore>(
    store: &S,
    builder: PackBuilder,
    entropy: &mut Entropy,
    out: &mut RepackOutcome,
    records: &mut Vec<pb::PackRecord>,
) -> Result<()> {
    let done = builder.finish(entropy)?;
    out.bytes_written += done.bytes.len() as u64;
    put_named(store, &pack_object_key(&done.pack_id), done.bytes).await?;
    out.new_packs.push(done.pack_id);
    records.push(done.record);
    Ok(())
}
