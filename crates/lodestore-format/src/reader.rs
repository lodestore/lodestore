//! Reading a repository: snapshot listing, blob reads and restore.
//!
//! Everything read from the store is untrusted. Objects are checked against
//! their names, frames against their authentication tags and declared sizes,
//! and plaintext against its keyed hash before it is used.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use crate::Fut;
use crate::attest::{read_attestations, read_attestations_complete};
use crate::check::is_data_error;
use crate::chunker::blob_id;
use crate::config;
use crate::crypto::{BLOB_TYPE_DATA, BLOB_TYPE_TREE, BlobId, Key32, RepoId, open_frame};
use crate::error::{Error, Result};
use crate::fs::filter::PathMatcher;
use crate::fs::restore::{RestoreSettings, Restorer, check_extents};
use crate::index::{
    BlobLocation, INDEX_PREFIX, Index, MAX_SHARD_READ_SIZE, MAX_SHARD_SIZE, SupersedesTrust, fetch_shard_bytes,
    hex32, index_object_key, open_shard, parse_hex32,
};
use crate::keys::ReadKeys;
use crate::pack::{PACK_PREFIX_LEN, decompress, open_prefix, pack_object_key};
use crate::pb;
use crate::provenance::{LatestPick, ProvenanceSet, pick_latest};
use crate::selection::Selection;
use crate::snapshot::{
    MAX_SNAPSHOT_SIZE, SnapshotEntry, SnapshotList, TreeSource, list_snapshots, open_snapshot,
    snapshot_object_key,
};
use crate::store::{BlobError, BlobStore, read_bounded};
use crate::tree::{
    Action, SelectedWalk, Step, TreeBudget, TreeCursor, decode_tree, selection_totals,
    validate_subtree,
};

pub use crate::fs::restore::{Overwrite, RestoreFailure, Unapplied};

#[derive(Debug, Clone, Default)]
pub struct RestoreOptions {
    /// Absolute path inside the snapshot; empty restores the whole snapshot.
    /// A directory's entries land directly in the target, a file lands in it
    /// under its own name.
    pub path: Vec<u8>,
    /// What to do with files already in the target.
    pub overwrite: Overwrite,
    /// Restore only what matches, with the directories above it. Patterns
    /// are those of `--exclude`, matched against absolute snapshot paths.
    pub include: Option<PathMatcher>,
    /// Read every restored file back and compare its chunk ids.
    pub verify: bool,
    /// Decide and count without creating or changing anything.
    pub dry_run: bool,
    /// Leave owners, setuid/setgid bits and file capabilities of the
    /// snapshot out.
    pub skip_owners: bool,
}

/// Splits `<snapshot>[:<path>]`. The path is empty when none is given.
pub fn parse_snapshot_path(spec: &str) -> (&str, &[u8]) {
    match spec.split_once(':') {
        Some((snapshot, path)) => (snapshot, path.as_bytes()),
        None => (spec, &[]),
    }
}

/// The components of an absolute snapshot path; `.`/`..` and NUL are refused.
fn components(path: &[u8]) -> Result<Vec<&[u8]>> {
    let mut out = Vec::new();
    for part in path.split(|&b| b == b'/').filter(|c| !c.is_empty()) {
        if part == b"." || part == b".." || part.contains(&0) {
            return Err(Error::InvalidInput(format!(
                "{:?} is not a snapshot path",
                String::from_utf8_lossy(path)
            )));
        }
        out.push(part);
    }
    Ok(out)
}

/// Where a snapshot path points.
struct Located {
    /// `None` for the top of the snapshot.
    node: Option<pb::Node>,
    /// Absolute path of `node`; empty for the top.
    path: Vec<u8>,
    /// The top-level entries, set for the top only.
    top: Vec<pb::Node>,
}

#[derive(Debug, Clone)]
pub struct RestoreProgress {
    pub files_done: u64,
    pub bytes_done: u64,
    pub current: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct RestoreReport {
    /// Files written or hard-linked, and their total size.
    pub files: u64,
    pub bytes: u64,
    /// Files left alone because of `--overwrite`.
    pub skipped: u64,
    pub verified: u64,
    /// Dry run: what a real run would write and leave alone.
    pub would_write: u64,
    pub would_skip: u64,
    pub unapplied: Vec<Unapplied>,
    /// Entries that could not be restored.
    pub errors: Vec<RestoreFailure>,
}

/// What a selection would restore.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SelectionTotals {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
}

/// Opens one chunk fetched by whoever holds the pack key: authenticates the
/// frame against the repository, blob type and id, decompresses it into at
/// most `ulen` bytes and checks that the plaintext hashes back to `id` under
/// `k_id`. The server and the agent read chunks through this one function.
pub fn open_chunk(
    pack_key: &Key32,
    repo_id: &RepoId,
    blob_type: u8,
    id: &BlobId,
    frame: &[u8],
    ulen: u32,
    k_id: &[u8; 32],
) -> Result<Vec<u8>> {
    let name = || format!("blob {}", hex32(id));
    let (codec, payload) = open_frame(pack_key, repo_id, blob_type, id, frame)
        .map_err(|_| Error::corrupt(name(), "authentication failed"))?;
    let plain = decompress(codec, payload, ulen as usize, blob_type)?;
    if &blob_id(k_id, &plain) != id {
        return Err(Error::corrupt(name(), "content does not match its id"));
    }
    Ok(plain)
}

/// Tree bytes a [`ReaderCache`] keeps unless told otherwise.
pub const DEFAULT_TREE_BUDGET: usize = 128 << 20;

const RELIST_AFTER: Duration = Duration::from_secs(30);

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

struct TreeBytes {
    bytes: HashMap<BlobId, Arc<Vec<u8>>>,
    order: VecDeque<BlobId>,
    used: usize,
    budget: usize,
}

impl TreeBytes {
    fn insert(&mut self, id: BlobId, bytes: Arc<Vec<u8>>) {
        if bytes.len() > self.budget || self.bytes.contains_key(&id) {
            return;
        }
        while self.used + bytes.len() > self.budget {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some(gone) = self.bytes.remove(&old) {
                self.used -= gone.len();
            }
        }
        self.used += bytes.len();
        self.order.push_back(id);
        self.bytes.insert(id, bytes);
    }
}

/// What the readers of one repository can share: the index, the keys of the
/// packs read so far and the decrypted tree blobs. Nothing in it can go stale
/// except the list of index shards, which a snapshot names itself and which is
/// listed again after [`RELIST_AFTER`] when a blob is not found.
// ponytail: trees leave in the order they came in, not the order they were
// last used; an LRU would keep a hot directory longer if that ever shows in a profile.
pub struct ReaderCache {
    repo_id: OnceLock<RepoId>,
    index: tokio::sync::RwLock<Index>,
    load: tokio::sync::Mutex<()>,
    trust_set: tokio::sync::OnceCell<HashSet<BlobId>>,
    listed_at: Mutex<Option<Instant>>,
    pack_keys: Mutex<HashMap<BlobId, Key32>>,
    trees: Mutex<TreeBytes>,
}

impl ReaderCache {
    /// An empty cache holding at most `tree_budget_bytes` of tree blobs.
    pub fn new(tree_budget_bytes: usize) -> Arc<Self> {
        Arc::new(ReaderCache {
            repo_id: OnceLock::new(),
            index: tokio::sync::RwLock::default(),
            load: tokio::sync::Mutex::new(()),
            trust_set: tokio::sync::OnceCell::new(),
            listed_at: Mutex::new(None),
            pack_keys: Mutex::default(),
            trees: Mutex::new(TreeBytes {
                bytes: HashMap::new(),
                order: VecDeque::new(),
                used: 0,
                budget: tree_budget_bytes,
            }),
        })
    }

    fn may_list(&self) -> bool {
        locked(&self.listed_at).is_none_or(|at| at.elapsed() >= RELIST_AFTER)
    }
}

impl<S: BlobStore> TreeSource for Reader<S> {
    fn snapshot_root<'a>(&'a mut self, id: &'a BlobId) -> Fut<'a, (pb::Snapshot, Vec<pb::Node>)> {
        Box::pin(self.open_root(id))
    }

    fn tree<'a>(&'a mut self, ids: &'a [Vec<u8>]) -> Fut<'a, Vec<pb::Node>> {
        Box::pin(self.load_tree(ids))
    }
}

pub struct Reader<S: BlobStore> {
    store: S,
    keys: ReadKeys,
    cache: Arc<ReaderCache>,
    walk: TreeBudget,
}

impl<S: BlobStore> Reader<S> {
    /// A reader with a cache of its own.
    pub fn open(store: S, keys: ReadKeys, config: &pb::Config) -> Result<Self> {
        Self::open_shared(store, keys, config, ReaderCache::new(DEFAULT_TREE_BUDGET))
    }

    /// A reader that shares `cache` with the others of the same repository.
    pub fn open_shared(
        store: S,
        keys: ReadKeys,
        config: &pb::Config,
        cache: Arc<ReaderCache>,
    ) -> Result<Self> {
        config::check_read_version(config)?;
        if config::repo_id_of(config)? != *keys.repo_id() {
            return Err(Error::InvalidInput(
                "the keys belong to a different repository".into(),
            ));
        }
        if cache.repo_id.get_or_init(|| *keys.repo_id()) != keys.repo_id() {
            return Err(Error::InvalidInput(
                "the cache belongs to a different repository".into(),
            ));
        }
        Ok(Reader {
            store,
            keys,
            cache,
            walk: TreeBudget::default(),
        })
    }

    /// Replaces the allowance of one operation; the next one starts with it.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn set_walk_limits(&mut self, nodes: u64, bytes: u64) {
        self.walk = TreeBudget::new(nodes, bytes);
    }

    pub async fn list_snapshots(&self) -> Result<SnapshotList> {
        list_snapshots(&self.store, &self.keys).await
    }

    /// What the server and the password holder have signed. A read that a
    /// cleanup cycle keeps changing is an error, never a partial set.
    pub async fn provenance(&self) -> Result<ProvenanceSet> {
        let set = read_attestations_complete(&self.store, &self.keys).await?;
        Ok(ProvenanceSet::from_attested(&set, &self.keys))
    }

    /// The `latest` snapshot with the warnings that go with the choice.
    /// `expect_upgraded` is true for a caller that knows the repository
    /// carried server signatures before.
    pub async fn resolve_latest(&self, expect_upgraded: bool) -> Result<LatestPick> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|d| i64::try_from(d.as_millis()).ok())
            .unwrap_or(0);
        let entries = self.list_snapshots().await?.entries;
        pick_latest(entries, &self.provenance().await?, now_ms, expect_upgraded)
    }

    /// Finds a snapshot by `latest`, its full id or a unique hex prefix.
    /// `latest` here assumes the caller knows nothing of an earlier upgrade;
    /// a caller that keeps that knowledge calls [`Reader::resolve_latest`].
    pub async fn resolve_snapshot(&self, spec: &str) -> Result<SnapshotEntry> {
        if spec == "latest" {
            return Ok(self.resolve_latest(false).await?.entry);
        }
        let spec = spec.to_ascii_lowercase();
        if spec.is_empty() || !spec.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::InvalidInput(format!(
                "{spec:?} is not a snapshot id"
            )));
        }
        let mut hits: Vec<SnapshotEntry> = self
            .list_snapshots()
            .await?
            .entries
            .into_iter()
            .filter(|e| hex32(&e.id).starts_with(&spec))
            .collect();
        match hits.len() {
            0 => Err(Error::InvalidInput(format!("no snapshot matches {spec}"))),
            1 => Ok(hits.remove(0)),
            n => Err(Error::InvalidInput(format!(
                "{spec} is ambiguous, {n} snapshots match"
            ))),
        }
    }

    pub fn keys(&self) -> &ReadKeys {
        &self.keys
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    /// The snapshot `id`, read at most [`MAX_SNAPSHOT_SIZE`] bytes.
    pub async fn snapshot(&self, id: &BlobId) -> Result<pb::Snapshot> {
        let bytes = read_bounded(
            &self.store,
            &snapshot_object_key(id),
            None,
            MAX_SNAPSHOT_SIZE,
            "snapshot",
        )
        .await?;
        open_snapshot(
            self.keys.hpke_private_key(),
            self.keys.repo_id(),
            id,
            &bytes,
        )
    }

    /// The key that opens the frames of a pack, read from the pack's prefix.
    pub async fn pack_key(&mut self, pack_id: &BlobId) -> Result<Key32> {
        let known = locked(&self.cache.pack_keys).get(pack_id).cloned();
        if let Some(key) = known {
            return Ok(key);
        }
        let prefix = self
            .store
            .get_range(&pack_object_key(pack_id), 0..PACK_PREFIX_LEN as u64)
            .await?;
        let key = open_prefix(self.keys.hpke_private_key(), self.keys.repo_id(), &prefix)?;
        locked(&self.cache.pack_keys).insert(*pack_id, key.clone());
        Ok(key)
    }

    /// Makes sure one index shard is in the shared index. Reading it from the
    /// store happens outside the index lock; only one shard is fetched at a
    /// time, and a shard another reader fetched meanwhile is not fetched again.
    /// `false` when the store does not have it.
    async fn load_shard(&self, shard_id: BlobId, listed: Option<u64>) -> Result<bool> {
        if self.cache.index.read().await.has_shard(&shard_id) {
            return Ok(true);
        }
        let _one_at_a_time = self.cache.load.lock().await;
        if self.cache.index.read().await.has_shard(&shard_id) {
            return Ok(true);
        }
        let attested = self
            .cache
            .trust_set
            .get_or_init(|| self.trust_attested())
            .await
            .contains(&shard_id);
        let key = index_object_key(&shard_id);
        let max = if attested {
            MAX_SHARD_READ_SIZE
        } else {
            MAX_SHARD_SIZE
        };
        match fetch_shard_bytes(&self.store, &key, listed, max).await? {
            Some(bytes) => {
                let shard = open_shard(self.keys.k_idx(), self.keys.repo_id(), &shard_id, &bytes)?;
                self.cache.index.write().await.add_shard(shard_id, &shard)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Only shards the server attested may supersede others. When the
    /// attestations cannot be read nothing is trusted: that only costs the
    /// preference for a newer copy and the larger size a signed shard may have.
    async fn trust_attested(&self) -> HashSet<BlobId> {
        let attested = match read_attestations(&self.store, &self.keys).await {
            Ok(set) => set.shards,
            Err(_) => HashSet::new(),
        };
        self.cache
            .index
            .write()
            .await
            .set_supersedes_trust(SupersedesTrust::Only(attested.clone()));
        attested
    }

    /// Lists the shards in the store and loads those that open; one that does
    /// not costs deduplication only, and a rebuild finds its packs again.
    async fn load_all_shards(&self) -> Result<()> {
        for entry in self.store.list(INDEX_PREFIX).await? {
            let Some(shard_id) = entry.key.strip_prefix(INDEX_PREFIX).and_then(parse_hex32) else {
                continue;
            };
            match self.load_shard(shard_id, Some(entry.size)).await {
                Ok(_) | Err(Error::Corrupt { .. }) => {}
                Err(e) => return Err(e),
            }
        }
        *locked(&self.cache.listed_at) = Some(Instant::now());
        Ok(())
    }

    /// Every location the index knows for a blob, the preferred one first. The
    /// index named by the snapshot is loaded already; a blob missing from it
    /// triggers one listing of all shards.
    async fn locate_blob_all(&mut self, id: &BlobId, blob_type: u8) -> Result<Vec<BlobLocation>> {
        let mut locs = self.cache.index.read().await.locations(id, blob_type);
        if locs.is_empty() && self.cache.may_list() {
            self.load_all_shards().await?;
            locs = self.cache.index.read().await.locations(id, blob_type);
        }
        if locs.is_empty() {
            return Err(Error::corrupt(
                format!("blob {}", hex32(id)),
                "not in the index; if index/ objects are missing, run `lode repo rebuild-index`",
            ));
        }
        Ok(locs)
    }

    async fn locate_blob(&mut self, id: &BlobId, blob_type: u8) -> Result<BlobLocation> {
        Ok(self.locate_blob_all(id, blob_type).await?.remove(0))
    }

    async fn read_at(&mut self, loc: &BlobLocation, id: &BlobId, blob_type: u8) -> Result<Vec<u8>> {
        let name = || format!("blob {}", hex32(id));
        let missing = |e: Error| match e {
            Error::Store(BlobError::NotFound) => {
                Error::corrupt(name(), "its pack is missing from the store")
            }
            e => e,
        };
        let pack_key = self.pack_key(&loc.pack_id).await.map_err(missing)?;
        let start = u64::from(loc.offset);
        let frame = self
            .store
            .get_range(
                &pack_object_key(&loc.pack_id),
                start..start + u64::from(loc.enc_len),
            )
            .await
            .map_err(|e| missing(e.into()))?;
        if frame.len() != loc.enc_len as usize {
            return Err(Error::corrupt(
                name(),
                "pack is shorter than the index says",
            ));
        }
        open_chunk(
            &pack_key,
            self.keys.repo_id(),
            blob_type,
            id,
            &frame,
            loc.ulen,
            self.keys.k_id(),
        )
    }

    /// Reads, decrypts, decompresses and verifies one blob. A location that
    /// does not open (a shard from a write-key holder can name wrong offsets,
    /// or a pack of a version this build does not know) is followed by the
    /// next known copy; when none works the shards are listed once more,
    /// since the real record may sit in one not loaded yet. Store errors
    /// other than a missing pack are returned at once.
    pub async fn read_blob(&mut self, id: &BlobId, blob_type: u8) -> Result<Vec<u8>> {
        let locs = self.locate_blob_all(id, blob_type).await?;
        let mut first_err = None;
        for loc in &locs {
            match self.read_at(loc, id, blob_type).await {
                Ok(bytes) => return Ok(bytes),
                Err(e) if is_data_error(&e) => {
                    first_err.get_or_insert(e);
                }
                Err(e) => return Err(e),
            }
        }
        if self.cache.may_list() {
            self.load_all_shards().await?;
            let fresh = self.cache.index.read().await.locations(id, blob_type);
            for loc in fresh.iter().filter(|l| !locs.contains(l)) {
                match self.read_at(loc, id, blob_type).await {
                    Ok(bytes) => return Ok(bytes),
                    Err(e) if is_data_error(&e) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        Err(first_err
            .unwrap_or_else(|| Error::corrupt(format!("blob {}", hex32(id)), "unreadable")))
    }

    /// Where the data chunks of a file are stored, in content order.
    pub async fn chunk_locations(&mut self, node: &pb::Node) -> Result<Vec<BlobLocation>> {
        let mut out = Vec::with_capacity(node.content.len());
        for raw in &node.content {
            let id: BlobId = raw
                .as_slice()
                .try_into()
                .map_err(|_| Error::corrupt("tree", "content id must be 32 bytes"))?;
            out.push(self.locate_blob(&id, BLOB_TYPE_DATA).await?);
        }
        Ok(out)
    }

    /// A tree blob, from the cache when it is there. A blob never changes
    /// under its id, so what the cache holds was verified when it was read.
    async fn tree_bytes(&mut self, id: &BlobId) -> Result<Arc<Vec<u8>>> {
        let known = locked(&self.cache.trees).bytes.get(id).cloned();
        if let Some(bytes) = known {
            return Ok(bytes);
        }
        let bytes = Arc::new(self.read_blob(id, BLOB_TYPE_TREE).await?);
        locked(&self.cache.trees).insert(*id, Arc::clone(&bytes));
        Ok(bytes)
    }

    /// Loads one directory: its tree blobs read back as one ordered run.
    pub(crate) async fn load_tree(&mut self, ids: &[Vec<u8>]) -> Result<Vec<pb::Node>> {
        let mut nodes = Vec::new();
        let mut previous: Option<Vec<u8>> = None;
        for raw in ids {
            let id: BlobId = raw
                .as_slice()
                .try_into()
                .map_err(|_| Error::corrupt("tree", "tree id must be 32 bytes"))?;
            let bytes = self.tree_bytes(&id).await?;
            self.walk.charge_bytes(bytes.len() as u64)?;
            let piece = decode_tree(&bytes, previous.as_deref())?;
            self.walk.charge_nodes(piece.len() as u64)?;
            if let Some(last) = piece.last() {
                previous = Some(last.name.clone());
            }
            nodes.extend(piece);
        }
        Ok(nodes)
    }

    /// Loads a snapshot and the index shards it names; returns it with its
    /// top-level nodes.
    pub(crate) async fn open_root(&mut self, id: &BlobId) -> Result<(pb::Snapshot, Vec<pb::Node>)> {
        self.walk.reset();
        let snapshot = self.snapshot(id).await?;
        for raw in &snapshot.index_refs {
            let shard_id: BlobId = raw
                .as_slice()
                .try_into()
                .map_err(|_| Error::corrupt("snapshot", "index reference must be 32 bytes"))?;
            self.load_shard(shard_id, None).await?;
        }
        let root = self
            .load_tree(std::slice::from_ref(&snapshot.root_tree))
            .await?;
        Ok((snapshot, root))
    }

    /// Loads a snapshot and the index shards it names; returns its top-level nodes.
    async fn open_tree(&mut self, snapshot_id: &BlobId) -> Result<Vec<pb::Node>> {
        let (_, root) = self.open_root(snapshot_id).await?;
        Ok(root)
    }

    /// Follows `path` from the top of the snapshot.
    async fn locate(&mut self, top: Vec<pb::Node>, path: &[u8]) -> Result<Located> {
        let parts = components(path)?;
        let mut nodes = top;
        let mut abs = Vec::new();
        let mut found = None;
        for (i, part) in parts.iter().enumerate() {
            let missing = || {
                Error::InvalidInput(format!(
                    "{:?} is not in the snapshot",
                    String::from_utf8_lossy(path)
                ))
            };
            let at = nodes
                .binary_search_by(|n| n.name.as_slice().cmp(part))
                .map_err(|_| missing())?;
            let node = nodes.swap_remove(at);
            abs.push(b'/');
            abs.extend_from_slice(part);
            if i + 1 == parts.len() {
                found = Some(node);
            } else if node.r#type == pb::NodeType::Dir as i32 {
                nodes = self.load_tree(&node.subtrees).await?;
            } else {
                return Err(missing());
            }
        }
        Ok(Located {
            node: found,
            path: abs,
            top: nodes,
        })
    }

    /// The directory entries a path stands for, and the absolute path of the
    /// directory holding them: a directory's own entries, or the file alone.
    async fn entries_at(
        &mut self,
        snapshot_id: &BlobId,
        path: &[u8],
    ) -> Result<(Vec<pb::Node>, Vec<u8>)> {
        let top = self.open_tree(snapshot_id).await?;
        let at = self.locate(top, path).await?;
        match at.node {
            None => Ok((at.top, Vec::new())),
            Some(node) if node.r#type == pb::NodeType::Dir as i32 => {
                Ok((self.load_tree(&node.subtrees).await?, at.path))
            }
            Some(node) => {
                let parent = at.path[..at.path.len() - node.name.len() - 1].to_vec();
                Ok((vec![node], parent))
            }
        }
    }

    /// The entries of the directory at `path`; an empty path or `/` is the
    /// top of the snapshot.
    pub async fn list_dir(&mut self, snapshot_id: &BlobId, path: &[u8]) -> Result<Vec<pb::Node>> {
        let top = self.open_tree(snapshot_id).await?;
        let at = self.locate(top, path).await?;
        match at.node {
            None => Ok(at.top),
            Some(node) if node.r#type == pb::NodeType::Dir as i32 => {
                self.load_tree(&node.subtrees).await
            }
            Some(_) => Err(Error::InvalidInput(format!(
                "{:?} is not a directory",
                String::from_utf8_lossy(path)
            ))),
        }
    }

    /// Counts what a selection would restore. Every tree and file it touches
    /// is checked as a restore checks them, so a bad one fails here.
    pub async fn selection_totals(
        &mut self,
        snapshot_id: &BlobId,
        selection: &Selection,
    ) -> Result<SelectionTotals> {
        selection_totals(self, snapshot_id, selection).await
    }

    /// Calls `visit` with the absolute path and node of `path` itself and then
    /// of everything below it, in tree order. The top of the snapshot has no
    /// node of its own and is not visited.
    pub async fn walk_subtree(
        &mut self,
        snapshot_id: &BlobId,
        path: &[u8],
        visit: &mut dyn FnMut(&[u8], &pb::Node) -> Result<()>,
    ) -> Result<()> {
        let top = self.open_tree(snapshot_id).await?;
        let at = self.locate(top, path).await?;
        let (nodes, base) = match at.node {
            None => (at.top, Vec::new()),
            Some(node) => {
                visit(&at.path, &node)?;
                if node.r#type != pb::NodeType::Dir as i32 {
                    return Ok(());
                }
                (self.load_tree(&node.subtrees).await?, at.path)
            }
        };
        let mut cursor = TreeCursor::new(nodes, base);
        while let Some(step) = cursor.next() {
            let Step::Node(node) = step else { continue };
            visit(&cursor.path_of(&node.name), &node)?;
            if node.r#type == pb::NodeType::Dir as i32 {
                cursor.descend(self, &node).await?;
            }
        }
        Ok(())
    }

    /// Checks that the chunks of a regular file add up to the size it claims.
    /// The sizes come from the index; when they do not add up, the chunks are
    /// read, since a shard from a write-key holder can list a copy with a
    /// wrong size ahead of the real one.
    pub(crate) async fn check_file(&mut self, node: &pb::Node) -> Result<Vec<(u64, u64)>> {
        let extents = check_extents(node)?;
        let expected = if extents.is_empty() {
            node.size
        } else {
            extents.iter().map(|e| e.1).sum()
        };
        let mut total = self.content_len(node, false).await?;
        if total != expected {
            total = self.content_len(node, true).await?;
        }
        if total != expected {
            return Err(Error::corrupt(
                String::from_utf8_lossy(&node.name),
                format!("its chunks hold {total} bytes, the node says {expected}"),
            ));
        }
        Ok(extents)
    }

    /// The size of a file's chunks, from the index or, with `read`, from the
    /// chunks themselves.
    async fn content_len(&mut self, node: &pb::Node, read: bool) -> Result<u64> {
        let mut total = 0u64;
        for raw in &node.content {
            let id: BlobId = raw
                .as_slice()
                .try_into()
                .map_err(|_| Error::corrupt("tree", "content id must be 32 bytes"))?;
            let len = if read {
                self.read_blob(&id, BLOB_TYPE_DATA).await?.len() as u64
            } else {
                u64::from(self.locate_blob(&id, BLOB_TYPE_DATA).await?.ulen)
            };
            total = total.saturating_add(len);
        }
        Ok(total)
    }

    /// Writes the content of one regular file to `out`; holes of a sparse
    /// file are written as zeros.
    pub async fn dump_file(
        &mut self,
        snapshot_id: &BlobId,
        path: &[u8],
        out: &mut dyn Write,
    ) -> Result<()> {
        let top = self.open_tree(snapshot_id).await?;
        let at = self.locate(top, path).await?;
        let node = match at.node {
            Some(n) if n.r#type == pb::NodeType::File as i32 => n,
            _ => {
                return Err(Error::InvalidInput(format!(
                    "{:?} is not a regular file",
                    String::from_utf8_lossy(path)
                )));
            }
        };
        let extents = self.check_file(&node).await?;
        let (mut pos, mut extent, mut extent_pos) = (0u64, 0usize, 0u64);
        for raw in &node.content {
            let id: BlobId = raw.as_slice().try_into().expect("length checked");
            let data = self.read_blob(&id, BLOB_TYPE_DATA).await?;
            if extents.is_empty() {
                out.write_all(&data)?;
                continue;
            }
            let mut rest = data.as_slice();
            while !rest.is_empty() {
                let Some(&(offset, len)) = extents.get(extent) else {
                    return Err(Error::corrupt(
                        String::from_utf8_lossy(&node.name),
                        "content is longer than its data extents",
                    ));
                };
                if extent_pos == len {
                    extent += 1;
                    extent_pos = 0;
                    continue;
                }
                write_zeros(out, (offset + extent_pos).saturating_sub(pos))?;
                pos = pos.max(offset + extent_pos);
                let n = usize::try_from(len - extent_pos).map_or(rest.len(), |r| r.min(rest.len()));
                out.write_all(&rest[..n])?;
                pos += n as u64;
                extent_pos += n as u64;
                rest = &rest[n..];
            }
        }
        if !extents.is_empty() {
            write_zeros(out, node.size.saturating_sub(pos))?;
        }
        Ok(())
    }

    /// Restores a snapshot to `<target>/<absolute path>`. The trees to be used
    /// are all read and checked first; a bad one fails the restore before the
    /// target is touched. After that, a file whose data does not check out is
    /// left out and listed in `errors`, the rest is restored, and metadata that
    /// cannot be applied is listed in `unapplied`. Failures to create things
    /// abort.
    pub async fn restore(
        &mut self,
        snapshot_id: &BlobId,
        target: &Path,
        options: &RestoreOptions,
        progress: &dyn Fn(&RestoreProgress),
    ) -> Result<RestoreReport> {
        let (nodes, base) = self.entries_at(snapshot_id, &options.path).await?;
        // Nothing is written until every tree that will be used has passed.
        validate_subtree(self, nodes.clone(), base.clone(), options.include.as_ref()).await?;
        let settings = RestoreSettings {
            overwrite: options.overwrite,
            verify: options.verify && !options.dry_run,
            dry_run: options.dry_run,
            k_id: Key32::new(*self.keys.k_id()),
            seed: self.keys.cdc_seed(),
            restore_owners: !options.skip_owners,
            keep_privileges: true,
        };
        self.walk.reset();
        let mut restorer = Restorer::open(target, settings)?;
        let mut walk = SelectedWalk::new(TreeCursor::new(nodes, base), options.include.as_ref());
        while let Some(action) = walk.next(self).await? {
            let node = match action {
                Action::Enter { node, .. } => {
                    restorer.enter_dir(&node)?;
                    continue;
                }
                Action::Leave => {
                    restorer.leave_dir();
                    continue;
                }
                Action::Entry(node) => node,
            };
            if node.r#type != pb::NodeType::File as i32 {
                restorer.create_other(&node)?;
                continue;
            }
            match self.restore_file(&mut restorer, &node).await {
                Ok(()) => {}
                // One bad file does not stop the others.
                Err(e @ (Error::Corrupt { .. } | Error::Crypto)) => {
                    restorer.record_failure(&node.name, e.to_string());
                }
                Err(e) => return Err(e),
            }
            let stats = restorer.stats();
            progress(&RestoreProgress {
                files_done: stats.restored + stats.skipped + stats.would_write + stats.would_skip,
                bytes_done: stats.bytes,
                current: walk.path_of(&node.name),
            });
        }
        let stats = restorer.take_stats();
        Ok(RestoreReport {
            files: stats.restored,
            bytes: stats.bytes,
            skipped: stats.skipped,
            verified: stats.verified,
            would_write: stats.would_write,
            would_skip: stats.would_skip,
            unapplied: restorer.take_unapplied(),
            errors: restorer.take_failures(),
        })
    }

    /// Writes one regular file (or links it to an earlier member of its
    /// hardlink group). A failure leaves no file under the final name.
    async fn restore_file(&mut self, restorer: &mut Restorer, node: &pb::Node) -> Result<()> {
        if restorer.link_if_seen(node)? {
            return Ok(());
        }
        let Some(mut sink) = restorer.begin_file(node)? else {
            return Ok(());
        };
        for raw in &node.content {
            let id: BlobId = raw
                .as_slice()
                .try_into()
                .map_err(|_| Error::corrupt("tree", "content id must be 32 bytes"))?;
            let data = self.read_blob(&id, BLOB_TYPE_DATA).await?;
            sink.write(&data)?;
        }
        restorer.finish_file(sink, node)
    }
}

fn write_zeros(out: &mut dyn Write, mut n: u64) -> Result<()> {
    static ZEROS: [u8; 64 << 10] = [0; 64 << 10];
    while n > 0 {
        let chunk = usize::try_from(n).map_or(ZEROS.len(), |n| n.min(ZEROS.len()));
        out.write_all(&ZEROS[..chunk])?;
        n -= chunk as u64;
    }
    Ok(())
}
