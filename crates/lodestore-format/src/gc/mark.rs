//! Finding the blobs that snapshots in the bucket still need.

use std::collections::HashSet;

use crate::attest::read_attestations;
use crate::check::is_data_error;
use crate::crypto::{BLOB_TYPE_DATA, BLOB_TYPE_TREE, BlobId};
use crate::error::{Error, Result, within_size};
use crate::index::{
    INDEX_PREFIX, Index, MAX_SHARD_READ_SIZE, MAX_SHARD_SIZE, SupersedesTrust, check_shard,
    fetch_shard_bytes, hex32, open_shard, parse_hex32,
};
use crate::keys::ReadKeys;
use crate::pb;
use crate::reader::{Reader, ReaderCache};
use crate::snapshot::{MAX_SNAPSHOT_SIZE, SNAPSHOT_PREFIX, open_snapshot, snapshot_object_key};
use crate::store::{BlobError, BlobStore};
use crate::tree::decode_tree;

/// Every index shard that opens and whose records are well formed, as
/// listed in the bucket.
#[derive(Debug, Default)]
pub struct ShardSet {
    pub shards: Vec<(BlobId, pb::IndexShard)>,
    /// `(object key, reason)` of the shards that did not open or hold a
    /// malformed record. Their packs are unindexed to the plan, and a
    /// rebuild finds them again.
    pub skipped: Vec<(String, String)>,
    /// Shards the server vouches for; only their `supersedes` count.
    pub attested: HashSet<BlobId>,
    /// Shards whose frames the server opened. The format does not know the
    /// server's database; `load_shards` leaves it empty and the server fills it.
    pub verified: HashSet<BlobId>,
    /// Attested shards larger than `MAX_SHARD_SIZE`, written before the
    /// limit; compaction splits them.
    pub oversized: HashSet<BlobId>,
}

pub async fn load_shards<S: BlobStore>(store: &S, keys: &ReadKeys) -> Result<ShardSet> {
    let mut out = ShardSet {
        attested: read_attestations(store, keys).await?.shards,
        ..ShardSet::default()
    };
    for entry in store.list(INDEX_PREFIX).await? {
        let Some(shard_id) = entry.key.strip_prefix(INDEX_PREFIX).and_then(parse_hex32) else {
            continue;
        };
        let max = if out.attested.contains(&shard_id) {
            MAX_SHARD_READ_SIZE
        } else {
            MAX_SHARD_SIZE
        };
        let opened = match fetch_shard_bytes(store, &entry.key, Some(entry.size), max).await {
            Ok(Some(bytes)) => open_shard(keys.k_idx(), keys.repo_id(), &shard_id, &bytes)
                .and_then(|shard| check_shard(shard_id, &shard).map(|()| shard)),
            Ok(None) => continue,
            Err(e) => Err(e),
        };
        match opened {
            Ok(shard) => {
                if entry.size > MAX_SHARD_SIZE {
                    out.oversized.insert(shard_id);
                }
                out.shards.push((shard_id, shard));
            }
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
        index.set_supersedes_trust(SupersedesTrust::Only(self.attested.clone()));
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

    /// `false` when the blob was not in the set.
    pub fn remove(&mut self, id: &BlobId, blob_type: u8) -> bool {
        self.0.remove(&(*id, blob_type))
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
    /// Snapshots the walk could not follow while `MarkPolicy::quarantine` was
    /// set. What they need is not known, so nothing may be deleted until an
    /// administrator has dealt with each.
    pub quarantined: Vec<Quarantined>,
}

/// What `mark_with` does with a snapshot it cannot follow. The format does not
/// know the server's database: the server hands over its decisions as sets.
#[derive(Debug, Default, Clone)]
pub struct MarkPolicy {
    /// Record a snapshot that does not open or whose trees cannot be walked
    /// in `MarkOutcome::quarantined` and go on. Off, the first such snapshot
    /// stops the walk with its error. Store errors always stop it.
    pub quarantine: bool,
    /// Snapshots an administrator has excluded: still walked, and live if the
    /// walk succeeds; if it fails they are `ignored`, not `quarantined`.
    pub excluded: HashSet<BlobId>,
    /// Snapshots a job points at. Their failure is `quarantined` with
    /// `bound = true`, and the exclusion does not apply to them.
    pub bound: HashSet<BlobId>,
}

#[derive(Debug, Clone)]
pub struct Quarantined {
    pub key: String,
    pub id: BlobId,
    pub reason: String,
    pub bound: bool,
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

/// What reading one listed snapshot came to, when it did not fail.
enum Walked {
    Done,
    Gone,
    WrongName,
}

/// Reads the snapshot and marks everything it references. The trees newly
/// added to `live` are listed in `added`, also when the walk fails midway.
async fn walk_snapshot<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    reader: &mut Reader<&S>,
    index: &Index,
    live: &mut LiveSet,
    (key, id, size): &(String, BlobId, u64),
    added: &mut Vec<BlobId>,
) -> Result<Walked> {
    within_size(key, *size, MAX_SNAPSHOT_SIZE, "snapshot")?;
    let bytes = match store.get(key).await {
        Ok(bytes) => bytes,
        Err(BlobError::NotFound) => return Ok(Walked::Gone),
        Err(e) => return Err(e.into()),
    };
    if blake3::hash(&bytes).as_bytes() != id {
        return Ok(Walked::WrongName);
    }
    let snapshot = open_snapshot(keys.hpke_private_key(), keys.repo_id(), id, &bytes)?;
    let mut stack = vec![blob_ref(&snapshot.root_tree, key)?];
    while let Some(tree) = stack.pop() {
        if live.contains(&tree, BLOB_TYPE_TREE) {
            continue;
        }
        known(index, key, &tree, BLOB_TYPE_TREE)?;
        live.insert(tree, BLOB_TYPE_TREE);
        added.push(tree);
        let bytes = reader.read_blob(&tree, BLOB_TYPE_TREE).await?;
        for node in decode_tree(&bytes, None)? {
            for raw in &node.content {
                let chunk = blob_ref(raw, "tree")?;
                known(index, key, &chunk, BLOB_TYPE_DATA)?;
                live.insert(chunk, BLOB_TYPE_DATA);
            }
            for raw in &node.subtrees {
                stack.push(blob_ref(raw, "tree")?);
            }
        }
    }
    Ok(Walked::Done)
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
    mark_with(store, keys, config, index, &MarkPolicy::default(), progress).await
}

/// [`mark`] with a policy for the snapshots it cannot follow. The trees that a
/// failed walk added to the live set are taken out again, so that another
/// snapshot sharing them walks them itself and fails the same way. Data
/// blobs it marked stay live: more live than needed is the safe side.
pub async fn mark_with<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    config: &pb::Config,
    index: &Index,
    policy: &MarkPolicy,
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
    for (done, entry) in listed.into_iter().enumerate() {
        progress(done as u64, total);
        let mut added = Vec::new();
        let walked = walk_snapshot(
            store,
            keys,
            &mut reader,
            index,
            &mut out.live,
            &entry,
            &mut added,
        )
        .await;
        let (key, id, _) = entry;
        let bound = policy.bound.contains(&id);
        let reason = match walked {
            Ok(Walked::Done) => {
                out.snapshots.push(id);
                continue;
            }
            Ok(Walked::Gone) => {
                out.ignored
                    .push((key, "deleted while the cleanup was reading it".into()));
                continue;
            }
            Ok(Walked::WrongName) => {
                let reason = "object name does not match its content".to_string();
                if policy.quarantine && bound {
                    reason
                } else {
                    out.ignored.push((key, reason));
                    continue;
                }
            }
            Err(e) if policy.quarantine && is_data_error(&e) => {
                for tree in &added {
                    out.live.remove(tree, BLOB_TYPE_TREE);
                }
                if !bound && policy.excluded.contains(&id) {
                    out.ignored
                        .push((key, format!("excluded by an administrator: {e}")));
                    continue;
                }
                e.to_string()
            }
            Err(e) => return Err(e),
        };
        out.quarantined.push(Quarantined {
            key,
            id,
            reason,
            bound,
        });
    }
    progress(total, total);
    Ok(out)
}
