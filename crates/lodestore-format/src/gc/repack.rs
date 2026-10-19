//! Moving the live blobs of sparse packs into new packs.

use std::collections::{HashMap, HashSet};

use super::Survey;
use crate::crypto::{BLOB_TYPE_TREE, BlobId, Entropy};
use crate::error::{Error, Result};
use crate::index::{BlobLocation, hex32, index_object_key, seal_shard};
use crate::keys::ReadKeys;
use crate::pack::{PACK_PREFIX_LEN, PACK_TARGET_SIZE, PackBuilder, pack_object_key, parse_prefix};
use crate::pb;
use crate::reader::open_chunk;
use crate::store::{BlobStore, put_named};

#[derive(Debug, Clone, Default)]
pub struct RepackOutcome {
    /// The shard that describes `new_packs`; `None` when nothing was copied.
    pub shard: Option<BlobId>,
    pub new_packs: Vec<BlobId>,
    pub replaced: Vec<BlobId>,
    pub bytes_read: u64,
    pub bytes_written: u64,
}

/// Rewrites the live blobs of every pack the plan chose for repacking into
/// new packs and describes them with one new shard. Each source pack is held
/// in memory alone, next to one open builder per blob type. Nothing is
/// deleted: the replaced packs are only reported.
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
    for (done, stat) in survey.plan.repack.iter().enumerate() {
        progress(done as u64, total);
        out.replaced.push(stat.pack_id);
        let mut blobs = located.remove(&stat.pack_id).unwrap_or_default();
        if blobs.is_empty() {
            continue;
        }
        blobs.sort_by_key(|(_, loc)| loc.offset);
        let name = pack_object_key(&stat.pack_id);
        let bytes = store.get(&name).await?;
        if bytes.len() as u64 != stat.size || bytes.len() < PACK_PREFIX_LEN {
            return Err(Error::corrupt(name, "size differs from the listing"));
        }
        out.bytes_read += stat.size;
        let pack_key = parse_prefix(&bytes[..PACK_PREFIX_LEN])?
            .open_key(keys.hpke_private_key(), keys.repo_id())?;
        for (id, loc) in blobs {
            let start = loc.offset as usize;
            let frame = bytes
                .get(start..start + loc.enc_len as usize)
                .ok_or_else(|| {
                    Error::corrupt(name.clone(), format!("blob {} lies outside it", hex32(&id)))
                })?;
            let plain = open_chunk(
                &pack_key,
                keys.repo_id(),
                loc.blob_type,
                &id,
                frame,
                loc.ulen,
                keys.k_id(),
            )?;
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
    }
    for builder in builders.into_iter().flatten() {
        seal_pack(store, builder, entropy, &mut out, &mut records).await?;
    }
    if !records.is_empty() {
        let (shard_id, bytes) = seal_shard(keys.k_idx(), keys.repo_id(), records, entropy)?;
        put_named(store, &index_object_key(&shard_id), bytes).await?;
        out.shard = Some(shard_id);
    }
    progress(total, total);
    Ok(out)
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
