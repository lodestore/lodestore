//! Finding the blobs that snapshots in the bucket still need.

use std::collections::HashSet;

use crate::crypto::{BLOB_TYPE_DATA, BLOB_TYPE_TREE, BlobId};
use crate::error::{Error, Result, within_size};
use crate::index::{INDEX_PREFIX, Index, hex32, open_shard, parse_hex32};
use crate::keys::ReadKeys;
use crate::pb;
use crate::reader::{Reader, ReaderCache};
use crate::snapshot::{MAX_SNAPSHOT_SIZE, SNAPSHOT_PREFIX, open_snapshot, snapshot_object_key};
use crate::store::{BlobError, BlobStore};
use crate::tree::decode_tree;

/// Every index shard that opens, as listed in the bucket.
#[derive(Debug, Default)]
pub struct ShardSet {
    pub shards: Vec<(BlobId, pb::IndexShard)>,
    /// `(object key, reason)` of the shards that did not open. Their packs
    /// are unindexed to the plan, and a rebuild finds them again.
    pub skipped: Vec<(String, String)>,
}

pub async fn load_shards<S: BlobStore>(store: &S, keys: &ReadKeys) -> Result<ShardSet> {
    let mut out = ShardSet::default();
    for entry in store.list(INDEX_PREFIX).await? {
        let Some(shard_id) = entry.key.strip_prefix(INDEX_PREFIX).and_then(parse_hex32) else {
            continue;
        };
        // shortcut: a shard is read whole; compaction legitimately writes shards of many
        // GiB, so no size cap fits. Parse shards streaming if planted giant ones matter.
        let bytes = store.get(&entry.key).await?;
        match open_shard(keys.k_idx(), keys.repo_id(), &shard_id, &bytes) {
            Ok(shard) => out.shards.push((shard_id, shard)),
            Err(e @ (Error::Corrupt { .. } | Error::UnsupportedVersion { .. })) => {
                out.skipped.push((entry.key, e.to_string()));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

impl ShardSet {
    /// The index these shards describe, with the blobs of `exclude`d packs
    /// left out.
    pub fn index(&self, exclude: &HashSet<BlobId>) -> Result<Index> {
        let mut index = Index::default();
        index.exclude_packs(exclude.clone());
        for (id, shard) in &self.shards {
            index.add_shard(*id, shard)?;
        }
        Ok(index)
    }
}

// ponytail: the live set is a HashSet next to the Index, about 40 bytes per
// blob; a bitmap over the sorted index records would cut it to a bit per blob.
#[derive(Debug, Default)]
pub struct LiveSet(HashSet<(BlobId, u8)>);

impl LiveSet {
    pub fn contains(&self, id: &BlobId, blob_type: u8) -> bool {
        self.0.contains(&(*id, blob_type))
    }

    /// `false` when the blob was in the set already.
    pub fn insert(&mut self, id: BlobId, blob_type: u8) -> bool {
        self.0.insert((id, blob_type))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &(BlobId, u8)> {
        self.0.iter()
    }
}

#[derive(Debug, Default)]
pub struct MarkOutcome {
    pub live: LiveSet,
    pub snapshots: Vec<BlobId>,
    /// `(object key, reason)` of objects in `snapshots/` that are not
    /// snapshots: anyone who may create objects can plant one, so they must
    /// not stop a collection, and of snapshots deleted while the walk ran.
    pub ignored: Vec<(String, String)>,
}

fn blob_ref(raw: &[u8], what: &str) -> Result<BlobId> {
    raw.try_into()
        .map_err(|_| Error::corrupt(what, "blob id must be 32 bytes"))
}

fn known(index: &Index, key: &str, id: &BlobId, blob_type: u8) -> Result<()> {
    if index.get(id, blob_type).is_some() {
        return Ok(());
    }
    Err(Error::corrupt(
        key,
        format!(
            "references blob {} that no index shard describes",
            hex32(id)
        ),
    ))
}

/// Walks every snapshot found by listing `snapshots/` and records the tree
/// and data blobs they reference. Data is never read, trees are read once.
/// Anything the walk cannot follow stops it with an error: a blob that is
/// not marked would be treated as garbage. A snapshot deleted between the
/// listing and its read is no longer live and is only noted in `ignored`.
pub async fn mark<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    config: &pb::Config,
    index: &Index,
    progress: &(dyn Fn(u64, u64) + Sync),
) -> Result<MarkOutcome> {
    let mut reader = Reader::open_shared(store, keys.duplicate(), config, ReaderCache::new(0))?;
    let mut out = MarkOutcome::default();
    let listed: Vec<_> = store
        .list(SNAPSHOT_PREFIX)
        .await?
        .into_iter()
        .filter_map(|e| {
            let id = e.key.strip_prefix(SNAPSHOT_PREFIX).and_then(parse_hex32)?;
            (snapshot_object_key(&id) == e.key).then_some((e.key, id, e.size))
        })
        .collect();
    let total = listed.len() as u64;
    for (done, (key, id, size)) in listed.into_iter().enumerate() {
        progress(done as u64, total);
        within_size(&key, size, MAX_SNAPSHOT_SIZE, "snapshot")?;
        let bytes = match store.get(&key).await {
            Ok(bytes) => bytes,
            Err(BlobError::NotFound) => {
                out.ignored
                    .push((key, "deleted while the cleanup was reading it".into()));
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        if blake3::hash(&bytes).as_bytes() != &id {
            out.ignored
                .push((key, "object name does not match its content".into()));
            continue;
        }
        let snapshot = open_snapshot(keys.hpke_private_key(), keys.repo_id(), &id, &bytes)?;
        let mut stack = vec![blob_ref(&snapshot.root_tree, &key)?];
        while let Some(tree) = stack.pop() {
            if out.live.contains(&tree, BLOB_TYPE_TREE) {
                continue;
            }
            known(index, &key, &tree, BLOB_TYPE_TREE)?;
            out.live.insert(tree, BLOB_TYPE_TREE);
            let bytes = reader.read_blob(&tree, BLOB_TYPE_TREE).await?;
            for node in decode_tree(&bytes, None)? {
                for raw in &node.content {
                    let chunk = blob_ref(raw, "tree")?;
                    known(index, &key, &chunk, BLOB_TYPE_DATA)?;
                    out.live.insert(chunk, BLOB_TYPE_DATA);
                }
                for raw in &node.subtrees {
                    stack.push(blob_ref(raw, "tree")?);
                }
            }
        }
        out.snapshots.push(id);
    }
    progress(total, total);
    Ok(out)
}
