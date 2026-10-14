//! Reading a repository: snapshot listing, blob reads and restore.
//!
//! Everything read from the store is untrusted. Objects are checked against
//! their names, frames against their authentication tags and declared sizes,
//! and plaintext against its keyed hash before it is used.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use crate::Fut;
use crate::chunker::blob_id;
use crate::config;
use crate::crypto::{BLOB_TYPE_DATA, BLOB_TYPE_TREE, BlobId, Key32, open_frame};
use crate::error::{Error, Result};
use crate::fs::filter::PathMatcher;
use crate::fs::restore::{Restorer, Settings, check_extents};
use crate::index::{BlobLocation, Index, hex32};
use crate::keys::ReadKeys;
use crate::pack::{PACK_PREFIX_LEN, decompress, open_prefix, pack_object_key};
use crate::pb;
use crate::snapshot::{
    SnapshotEntry, SnapshotList, TreeSource, list_snapshots, open_snapshot, snapshot_object_key,
};
use crate::store::{BlobError, BlobStore};
use crate::tree::{Action, SelectedWalk, Step, TreeCursor, decode_tree, validate_subtree};

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

impl<S: BlobStore> TreeSource for Reader<S> {
    fn snapshot_root<'a>(&'a mut self, id: &'a BlobId) -> Fut<'a, (pb::Snapshot, Vec<pb::Node>)> {
        Box::pin(async move {
            let snapshot = self.load_snapshot(id).await?;
            self.index
                .load_refs(
                    &self.store,
                    self.keys.k_idx(),
                    self.keys.repo_id(),
                    &snapshot.index_refs,
                )
                .await?;
            let root = self
                .load_tree(std::slice::from_ref(&snapshot.root_tree))
                .await?;
            Ok((snapshot, root))
        })
    }

    fn tree<'a>(&'a mut self, ids: &'a [Vec<u8>]) -> Fut<'a, Vec<pb::Node>> {
        Box::pin(self.load_tree(ids))
    }
}

pub struct Reader<S: BlobStore> {
    store: S,
    keys: ReadKeys,
    index: Index,
    listed: bool,
    pack_keys: HashMap<BlobId, Key32>,
}

impl<S: BlobStore> Reader<S> {
    pub fn open(store: S, keys: ReadKeys, config: &pb::Config) -> Result<Self> {
        config::check_read_version(config)?;
        if config::repo_id_of(config)? != *keys.repo_id() {
            return Err(Error::InvalidInput(
                "the keys belong to a different repository".into(),
            ));
        }
        Ok(Reader {
            store,
            keys,
            index: Index::default(),
            listed: false,
            pack_keys: HashMap::new(),
        })
    }

    pub async fn list_snapshots(&self) -> Result<SnapshotList> {
        list_snapshots(&self.store, &self.keys).await
    }

    /// Finds a snapshot by `latest`, its full id or a unique hex prefix.
    pub async fn resolve_snapshot(&self, spec: &str) -> Result<SnapshotEntry> {
        if spec == "latest" {
            return self
                .list_snapshots()
                .await?
                .entries
                .pop()
                .ok_or_else(|| Error::InvalidInput("the repository has no snapshots".into()));
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

    async fn load_snapshot(&self, id: &BlobId) -> Result<pb::Snapshot> {
        let bytes = self.store.get(&snapshot_object_key(id)).await?;
        open_snapshot(
            self.keys.hpke_private_key(),
            self.keys.repo_id(),
            id,
            &bytes,
        )
    }

    async fn pack_key(&mut self, pack_id: &BlobId) -> Result<Key32> {
        if let Some(key) = self.pack_keys.get(pack_id) {
            return Ok(key.clone());
        }
        let prefix = self
            .store
            .get_range(&pack_object_key(pack_id), 0..PACK_PREFIX_LEN as u64)
            .await?;
        let key = open_prefix(self.keys.hpke_private_key(), self.keys.repo_id(), &prefix)?;
        self.pack_keys.insert(*pack_id, key.clone());
        Ok(key)
    }

    /// Where the index says a blob is. The index named by the snapshot is
    /// loaded already; a blob missing from it triggers one listing of all shards.
    async fn locate_blob(&mut self, id: &BlobId, blob_type: u8) -> Result<BlobLocation> {
        let name = || format!("blob {}", hex32(id));
        let mut loc = self.index.get(id, blob_type).copied();
        if loc.is_none() && !self.listed {
            self.index
                .load_all(&self.store, self.keys.k_idx(), self.keys.repo_id())
                .await?;
            self.listed = true;
            loc = self.index.get(id, blob_type).copied();
        }
        loc.ok_or_else(|| {
            Error::corrupt(
                name(),
                "not in the index; if index/ objects are missing, run `lode repo rebuild-index`",
            )
        })
    }

    /// Reads, decrypts, decompresses and verifies one blob.
    pub async fn read_blob(&mut self, id: &BlobId, blob_type: u8) -> Result<Vec<u8>> {
        let name = || format!("blob {}", hex32(id));
        let loc = self.locate_blob(id, blob_type).await?;

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
        let (codec, payload) = open_frame(&pack_key, self.keys.repo_id(), blob_type, id, &frame)
            .map_err(|_| Error::corrupt(name(), "authentication failed"))?;
        let plain = decompress(codec, payload, loc.ulen as usize, blob_type)?;
        if &blob_id(self.keys.k_id(), &plain) != id {
            return Err(Error::corrupt(name(), "content does not match its id"));
        }
        Ok(plain)
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
            let bytes = self.read_blob(&id, BLOB_TYPE_TREE).await?;
            let piece = decode_tree(&bytes, previous.as_deref())?;
            if let Some(last) = piece.last() {
                previous = Some(last.name.clone());
            }
            nodes.extend(piece);
        }
        Ok(nodes)
    }

    /// Loads a snapshot and the index shards it names; returns its top-level nodes.
    async fn open_tree(&mut self, snapshot_id: &BlobId) -> Result<Vec<pb::Node>> {
        let (_, root) = self.snapshot_root(snapshot_id).await?;
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
    pub(crate) async fn check_file(&mut self, node: &pb::Node) -> Result<Vec<(u64, u64)>> {
        let extents = check_extents(node)?;
        let mut total = 0u64;
        for raw in &node.content {
            let id: BlobId = raw
                .as_slice()
                .try_into()
                .map_err(|_| Error::corrupt("tree", "content id must be 32 bytes"))?;
            total =
                total.saturating_add(u64::from(self.locate_blob(&id, BLOB_TYPE_DATA).await?.ulen));
        }
        let expected = if extents.is_empty() {
            node.size
        } else {
            extents.iter().map(|e| e.1).sum()
        };
        if total != expected {
            return Err(Error::corrupt(
                String::from_utf8_lossy(&node.name),
                format!("its chunks hold {total} bytes, the node says {expected}"),
            ));
        }
        Ok(extents)
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
        let settings = Settings {
            overwrite: options.overwrite,
            verify: options.verify && !options.dry_run,
            dry_run: options.dry_run,
            k_id: *self.keys.k_id(),
            seed: self.keys.cdc_seed(),
        };
        let mut restorer = Restorer::open(target, settings)?;
        let mut walk = SelectedWalk::new(TreeCursor::new(nodes, base), options.include.as_ref());
        while let Some(action) = walk.next(self).await? {
            let node = match action {
                Action::Enter(dir) => {
                    restorer.enter_dir(&dir)?;
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
