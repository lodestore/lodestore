//! The agent's local copy of the trees of its last snapshot. An agent holds
//! write keys only and cannot read snapshots back, so the parent for an
//! incremental backup comes from here. Every tree is checked against its
//! keyed hash when read, and anything missing or wrong turns into an empty
//! directory: the files below it are read again, never trusted.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use data_encoding::HEXLOWER;
use serde_json::{Value, json};

use crate::Fut;
use crate::chunker::blob_id;
use crate::crypto::{BlobId, Key32, RepoId};
use crate::error::Error;
use crate::index::{hex32, parse_hex32};
use crate::keys::WriteKeys;
use crate::pack::MAX_TREE_ULEN;
use crate::pb;
use crate::snapshot::{ParentTree, TreeSource};
use crate::tree::decode_tree;

const TREES: &str = "trees";
const PARENT: &str = "parent.json";
const ZSTD_LEVEL: i32 = 3;

fn sibling(dir: &Path, suffix: &str) -> PathBuf {
    let mut name: OsString = dir.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn private_dir(path: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(path)
}

fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

/// Collects the trees of one backup in `<dir>.new` and swaps them in for
/// `<dir>` on [`commit`](CacheWriter::commit). Dropping it leaves the
/// previous cache as it was.
pub struct CacheWriter {
    dir: PathBuf,
    new: PathBuf,
    seen: HashSet<BlobId>,
}

impl CacheWriter {
    pub fn create(dir: &Path) -> io::Result<CacheWriter> {
        let new = sibling(dir, ".new");
        let _ = fs::remove_dir_all(&new);
        let _ = fs::remove_dir_all(sibling(dir, ".old"));
        private_dir(&new.join(TREES))?;
        Ok(CacheWriter {
            dir: dir.to_path_buf(),
            new,
            seen: HashSet::new(),
        })
    }

    pub fn put(&mut self, id: &BlobId, tree: &[u8]) -> io::Result<()> {
        if !self.seen.insert(*id) {
            return Ok(());
        }
        let packed = zstd::bulk::compress(tree, ZSTD_LEVEL)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.new.join(TREES).join(hex32(id)))?;
        file.write_all(&packed)
    }

    pub fn commit(
        self,
        repo_id: &RepoId,
        snapshot_id: &BlobId,
        time: &pb::Timestamp,
        root_tree: &BlobId,
    ) -> io::Result<()> {
        let meta = json!({
            "repository_id": HEXLOWER.encode(repo_id),
            "snapshot_id": hex32(snapshot_id),
            "time_secs": time.seconds,
            "time_nanos": time.nanos,
            "root_tree": hex32(root_tree),
        });
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.new.join(PARENT))?;
        file.write_all(meta.to_string().as_bytes())?;
        file.sync_all()?;

        let trees = self.new.join(TREES);
        for entry in fs::read_dir(&trees)? {
            File::open(entry?.path())?.sync_all()?;
        }
        sync_dir(&trees)?;
        sync_dir(&self.new)?;

        // A directory cannot be renamed over a non-empty one; a crash between
        // the two renames loses the cache, which costs one full re-read.
        let old = sibling(&self.dir, ".old");
        let had_old = match fs::rename(&self.dir, &old) {
            Ok(()) => true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(e),
        };
        fs::rename(&self.new, &self.dir)?;
        if had_old {
            let _ = fs::remove_dir_all(&old);
        }
        match self.dir.parent() {
            Some(parent) => sync_dir(parent),
            None => Ok(()),
        }
    }
}

struct Meta {
    snapshot_id: BlobId,
    time: pb::Timestamp,
    root_tree: BlobId,
}

fn read_meta(dir: &Path, repo_id: &RepoId) -> Option<Meta> {
    let value: Value = serde_json::from_slice(&fs::read(dir.join(PARENT)).ok()?).ok()?;
    let id = |key: &str| parse_hex32(value.get(key)?.as_str()?);
    if HEXLOWER
        .decode(value.get("repository_id")?.as_str()?.as_bytes())
        .ok()?
        != repo_id
    {
        return None;
    }
    Some(Meta {
        snapshot_id: id("snapshot_id")?,
        time: pb::Timestamp {
            seconds: value.get("time_secs")?.as_i64()?,
            nanos: u32::try_from(value.get("time_nanos")?.as_u64()?).ok()?,
        },
        root_tree: id("root_tree")?,
    })
}

struct DirSource {
    dir: PathBuf,
    k_id: Key32,
    meta: Meta,
}

impl DirSource {
    fn piece(&self, id: &[u8]) -> Option<Vec<u8>> {
        let id: BlobId = id.try_into().ok()?;
        let packed = fs::read(self.dir.join(TREES).join(hex32(&id))).ok()?;
        let bytes = zstd::bulk::decompress(&packed, MAX_TREE_ULEN).ok()?;
        (blob_id(&self.k_id, &bytes) == id).then_some(bytes)
    }

    fn nodes(&self, ids: &[Vec<u8>]) -> Option<(Vec<pb::Node>, u64)> {
        let mut nodes = Vec::new();
        let mut bytes = 0u64;
        let mut previous: Option<Vec<u8>> = None;
        for id in ids {
            let raw = self.piece(id)?;
            bytes = bytes.saturating_add(raw.len() as u64);
            let piece = decode_tree(&raw, previous.as_deref()).ok()?;
            if let Some(last) = piece.last() {
                previous = Some(last.name.clone());
            }
            nodes.extend(piece);
        }
        Some((nodes, bytes))
    }
}

impl TreeSource for DirSource {
    fn snapshot_root<'a>(
        &'a mut self,
        _id: &'a BlobId,
    ) -> Fut<'a, (pb::Snapshot, Vec<pb::Node>, u64)> {
        Box::pin(async move {
            let (root, bytes) = self
                .nodes(&[self.meta.root_tree.to_vec()])
                .ok_or_else(|| Error::corrupt("tree cache", "the root tree does not verify"))?;
            let snapshot = pb::Snapshot {
                time: Some(self.meta.time),
                root_tree: self.meta.root_tree.to_vec(),
                ..Default::default()
            };
            Ok((snapshot, root, bytes))
        })
    }

    // The cache holds trees this host wrote itself, so the limits of a path are not applied.
    fn tree<'a>(
        &'a mut self,
        ids: &'a [Vec<u8>],
        _above_entries: u64,
        _above_bytes: u64,
    ) -> Fut<'a, (Vec<pb::Node>, u64)> {
        Box::pin(async move { Ok(self.nodes(ids).unwrap_or_default()) })
    }
}

/// The parent a previous run left in `dir`, or `None` when there is none, it
/// is unreadable, or it belongs to another repository.
pub async fn open_parent<'a>(dir: &Path, keys: &WriteKeys) -> Option<ParentTree<'a>> {
    let meta = read_meta(dir, &keys.repo_id)?;
    let id = meta.snapshot_id;
    let source = DirSource {
        dir: dir.to_path_buf(),
        k_id: keys.k_id.clone(),
        meta,
    };
    ParentTree::from_source(Box::new(source), id).await.ok()
}
