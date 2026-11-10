//! A restore must refuse snapshots a writer-keys holder could forge, and
//! survive a hostile target, without changing anything it was not meant to.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};

use lodestore_format::check::FindingKind;
use lodestore_format::check::structure::{
    StructureReport, check_structure, check_structure_with_budget, check_structure_with_dir_limits,
};
use lodestore_format::crypto::{BLOB_TYPE_DATA, BLOB_TYPE_TREE, BlobId, Entropy};
use lodestore_format::error::Error;
use lodestore_format::keys::KdfCost;
use lodestore_format::pack::{CODEC_RAW, CODEC_ZSTD, PACK_PREFIX_LEN};
use lodestore_format::pb;
use lodestore_format::reader::{Overwrite, Reader, RestoreOptions, RestoreReport};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::selection::Selection;
use lodestore_format::snapshot::{ParentTree, seal_snapshot, snapshot_object_key};
use lodestore_format::store::{BlobStore, LocalFsStore, put_named};
use lodestore_format::tree::{
    MAX_DEPTH, MAX_DIR_ENTRIES, MAX_DIR_TREE_BYTES, MAX_HELD_ENTRIES, MAX_HELD_TREE_BYTES,
    MAX_WALK_NODES, MAX_WALK_TREE_BYTES, TreeBudget, encode_tree,
};
use lodestore_format::writer::Writer;

mod adv_support;
#[path = "adv_support/planted.rs"]
mod planted;
mod tree_support;

use adv_support::{attacker_keys, open_all_shards, plant_shard, plant_shard_where};
use lodestore_format::pack::{ENTRY_LEN, pack_object_key, parse_entries};
use planted::RangedOnly;

const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

struct Fx {
    _tmp: tempfile::TempDir,
    repo_dir: PathBuf,
    root: PathBuf,
    target: PathBuf,
    writer: Writer<LocalFsStore>,
}

impl Fx {
    async fn new() -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let repo_dir = root.join("repo");
        fs::create_dir(&repo_dir).unwrap();
        let store = LocalFsStore::new(repo_dir.clone());
        let mut entropy = Entropy::os();
        let options = InitOptions {
            kdf: LOW_KDF,
            ..Default::default()
        };
        PendingInit::generate(&mut entropy, options)
            .unwrap()
            .commit(&store, b"pw", &mut entropy)
            .await
            .unwrap();
        let repo = Repository::open(&store, b"pw").await.unwrap();
        let keys = repo.read_keys().write_keys(repo.config_hash());
        let writer = Writer::new(store, keys, repo.config()).await.unwrap();

        let target = root.join("out");
        fs::create_dir_all(target.join("a")).unwrap();
        fs::write(target.join("keep.txt"), b"keep").unwrap();
        Fx {
            _tmp: tmp,
            repo_dir,
            root,
            target,
            writer,
        }
    }

    fn data(&mut self, bytes: &[u8]) -> BlobId {
        let len = u32::try_from(bytes.len()).unwrap();
        self.writer
            .put_blob_raw(BLOB_TYPE_DATA, CODEC_RAW, bytes, len)
            .unwrap()
    }

    fn tree(&mut self, nodes: Vec<pb::Node>) -> BlobId {
        self.writer.put_tree_raw(&encode_tree(nodes)).unwrap()
    }

    async fn snapshot(&mut self, top: Vec<pb::Node>) -> BlobId {
        let root = self.tree(top);
        self.writer
            .commit_snapshot_raw(root, vec![b"/".to_vec()])
            .await
            .unwrap()
    }

    /// A snapshot whose first directory is fine and whose second holds
    /// `bad`, so a restore that validated as it went would already have
    /// written the first one.
    async fn hostile(&mut self, bad: Vec<pb::Node>) -> BlobId {
        let ok = self.data(b"fine");
        let first = self.tree(vec![file("ok.txt", &[ok], 4)]);
        let second = self.tree(bad);
        self.snapshot(vec![dir("a", first), dir("z", second)]).await
    }

    async fn reader(&self) -> Reader<LocalFsStore> {
        let store = LocalFsStore::new(self.repo_dir.clone());
        let repo = Repository::open(&store, b"pw").await.unwrap();
        Reader::open(store, repo.read_keys().duplicate(), repo.config()).unwrap()
    }

    async fn restore(
        &self,
        snapshot: &BlobId,
        options: &RestoreOptions,
    ) -> Result<RestoreReport, Error> {
        let mut reader = self.reader().await;
        reader
            .restore(snapshot, &self.target, options, &|_| {})
            .await
    }

    fn listing(&self) -> BTreeMap<PathBuf, (bool, u64)> {
        listing(&self.target)
    }

    /// The structure check reports the snapshot, as the restore refuses it.
    async fn assert_check_finds(&self, snapshot: &BlobId) {
        let store = LocalFsStore::new(self.repo_dir.clone());
        let repo = Repository::open(&store, b"pw").await.unwrap();
        let report = check_structure(&store, repo.read_keys(), repo.config(), &|_, _, _| {})
            .await
            .unwrap();
        let key = snapshot_object_key(snapshot);
        assert!(
            report.findings.iter().any(|f| f.key == key),
            "no finding for {key}: {:?}",
            report.findings
        );
    }

    /// The structure check with the default limits.
    async fn check(&self) -> StructureReport {
        let store = LocalFsStore::new(self.repo_dir.clone());
        let repo = Repository::open(&store, b"pw").await.unwrap();
        check_structure(&store, repo.read_keys(), repo.config(), &|_, _, _| {})
            .await
            .unwrap()
    }

    /// The structure check with a budget of the test's.
    async fn check_budget(&self, budget: TreeBudget) -> StructureReport {
        let store = LocalFsStore::new(self.repo_dir.clone());
        let repo = Repository::open(&store, b"pw").await.unwrap();
        check_structure_with_budget(
            &store,
            repo.read_keys(),
            repo.config(),
            &|_, _, _| {},
            budget,
        )
        .await
        .unwrap()
    }

    /// The structure check with smaller directory limits.
    async fn check_limited(&self, dir: u64, held: u64) -> StructureReport {
        let store = LocalFsStore::new(self.repo_dir.clone());
        let repo = Repository::open(&store, b"pw").await.unwrap();
        check_structure_with_dir_limits(
            &store,
            repo.read_keys(),
            repo.config(),
            &|_, _, _| {},
            dir,
            held,
        )
        .await
        .unwrap()
    }

    /// The restore is refused as corrupt and the target is exactly as before.
    async fn assert_refused(&self, snapshot: &BlobId) {
        let before = self.listing();
        let err = self
            .restore(snapshot, &RestoreOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Corrupt { .. }), "{err:?}");
        assert_eq!(self.listing(), before);
    }
}

fn listing(dir: &Path) -> BTreeMap<PathBuf, (bool, u64)> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, (bool, u64)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            out.insert(
                path.strip_prefix(base).unwrap().to_path_buf(),
                (meta.is_dir(), if meta.is_dir() { 0 } else { meta.len() }),
            );
            if meta.is_dir() {
                walk(base, &path, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

fn file(name: &str, content: &[BlobId], size: u64) -> pb::Node {
    pb::Node {
        name: name.as_bytes().to_vec(),
        r#type: pb::NodeType::File as i32,
        mode: 0o644,
        size,
        content: content.iter().map(|c| c.to_vec()).collect(),
        ..Default::default()
    }
}

fn dir(name: &str, tree: BlobId) -> pb::Node {
    pb::Node {
        name: name.as_bytes().to_vec(),
        r#type: pb::NodeType::Dir as i32,
        mode: 0o755,
        subtrees: vec![tree.to_vec()],
        ..Default::default()
    }
}

fn leftovers(dir: &Path) -> Vec<PathBuf> {
    listing(dir)
        .into_keys()
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".lode-restore-")
        })
        .collect()
}

async fn refuses_name(name: &[u8]) {
    let mut fx = Fx::new().await;
    let mut bad = file("x", &[], 0);
    bad.name = name.to_vec();
    let snap = fx.hostile(vec![bad]).await;
    fx.assert_refused(&snap).await;
    fx.assert_check_finds(&snap).await;
}

#[tokio::test]
async fn restore_hostile_dotdot_name() {
    refuses_name(b"..").await;
}

#[tokio::test]
async fn restore_hostile_dot_name() {
    refuses_name(b".").await;
}

#[tokio::test]
async fn restore_hostile_slash_name() {
    refuses_name(b"x/y").await;
    refuses_name(b"../escape").await;
}

#[tokio::test]
async fn restore_hostile_nul_name() {
    refuses_name(b"x\0y").await;
}

#[tokio::test]
async fn restore_hostile_empty_name() {
    refuses_name(b"").await;
}

#[tokio::test]
async fn restore_hostile_duplicate_names() {
    let mut fx = Fx::new().await;
    let snap = fx
        .hostile(vec![file("same", &[], 0), file("same", &[], 0)])
        .await;
    fx.assert_refused(&snap).await;
    fx.assert_check_finds(&snap).await;
}

#[tokio::test]
async fn restore_hostile_unsorted() {
    let mut fx = Fx::new().await;
    let snap = fx.hostile(vec![file("b", &[], 0), file("a", &[], 0)]).await;
    fx.assert_refused(&snap).await;
    fx.assert_check_finds(&snap).await;
}

#[tokio::test]
async fn restore_hostile_unsorted_across_pieces() {
    let mut fx = Fx::new().await;
    let first = fx.tree(vec![file("b", &[], 0)]);
    let second = fx.tree(vec![file("a", &[], 0)]);
    let mut split = dir("z", first);
    split.subtrees = vec![first.to_vec(), second.to_vec()];
    let snap = fx.snapshot(vec![split]).await;
    fx.assert_refused(&snap).await;
    fx.assert_check_finds(&snap).await;
}

#[tokio::test]
async fn restore_hostile_deep_tree() {
    let mut fx = Fx::new().await;
    let mut inner = fx.tree(vec![file("f", &[], 0)]);
    for _ in 0..5000 {
        inner = fx.tree(vec![dir("d", inner)]);
    }
    let snap = fx.snapshot(vec![dir("d", inner)]).await;
    let before = fx.listing();
    let err = fx
        .restore(&snap, &RestoreOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Corrupt { reason, .. } if reason.contains("nested")),
        "{err:?}"
    );
    assert_eq!(fx.listing(), before);
    fx.assert_check_finds(&snap).await;
}

/// A valid DAG in which every directory lists the next one sixteen times, so
/// the single file at the bottom is reached 16^4 times.
async fn fan_out(fx: &mut Fx) -> BlobId {
    let content = fx.data(b"fine");
    let mut inner = fx.tree(vec![file("f", &[content], 4)]);
    for _ in 0..3 {
        let entries = (0..16).map(|i| dir(&format!("d{i:02}"), inner)).collect();
        inner = fx.tree(entries);
    }
    let top = (0..16).map(|i| dir(&format!("d{i:02}"), inner)).collect();
    fx.snapshot(top).await
}

#[tokio::test]
async fn restore_hostile_fan_out_stops_by_budget() {
    let mut fx = Fx::new().await;
    let snap = fan_out(&mut fx).await;
    let before = fx.listing();

    let mut reader = fx.reader().await;
    reader.set_walk_limits(10_000, u64::MAX);
    let err = reader
        .restore(&snap, &fx.target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::WalkLimit {
                what: "entries",
                limit: 10_000
            }
        ),
        "{err:?}"
    );
    assert_eq!(fx.listing(), before);

    let err = reader
        .walk_subtree(&snap, b"/", &mut |_, _| Ok(()))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::WalkLimit {
                what: "entries",
                ..
            }
        ),
        "{err:?}"
    );

    assert_eq!(reader.list_dir(&snap, b"/").await.unwrap().len(), 16);
}

#[tokio::test]
async fn restore_hostile_fan_out_bytes_budget() {
    let mut fx = Fx::new().await;
    let snap = fan_out(&mut fx).await;
    let before = fx.listing();

    let mut reader = fx.reader().await;
    reader.set_walk_limits(u64::MAX, 4096);
    let err = reader
        .restore(&snap, &fx.target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::WalkLimit {
                what: "bytes of directory data",
                limit: 4096
            }
        ),
        "{err:?}"
    );
    assert_eq!(fx.listing(), before);
}

#[tokio::test]
async fn walk_budget_is_per_operation() {
    let mut fx = Fx::new().await;
    let snap = fan_out(&mut fx).await;

    let mut reader = fx.reader().await;
    reader.set_walk_limits(20, u64::MAX);
    assert_eq!(reader.list_dir(&snap, b"/").await.unwrap().len(), 16);
    assert_eq!(reader.list_dir(&snap, b"/").await.unwrap().len(), 16);
    let err = reader.list_dir(&snap, b"/d00").await.unwrap_err();
    assert!(matches!(err, Error::WalkLimit { .. }), "{err:?}");
}

#[test]
fn walk_budget_defaults() {
    assert_eq!(MAX_WALK_NODES, 1 << 30);
    assert_eq!(MAX_WALK_TREE_BYTES, 512 << 30);
    assert_eq!(MAX_DIR_ENTRIES, 1 << 23);
    assert_eq!(MAX_HELD_ENTRIES, 1 << 24);
    assert_eq!(MAX_DIR_TREE_BYTES, 1 << 30);
    assert_eq!(MAX_HELD_TREE_BYTES, 2 << 30);
}

fn files(prefix: &str, n: usize) -> Vec<pb::Node> {
    (0..n)
        .map(|i| file(&format!("{prefix}{i}"), &[], 0))
        .collect()
}

#[tokio::test]
async fn restore_hostile_wide_directory_stops_by_dir_limit() {
    let mut fx = Fx::new().await;
    let pieces: Vec<Vec<u8>> = ["a", "b", "c"]
        .iter()
        .map(|p| {
            let id = fx.tree(files(p, 5));
            id.to_vec()
        })
        .collect();
    let mut wide = dir("w", fx.tree(Vec::new()));
    wide.subtrees = pieces;
    let snap = fx.snapshot(vec![wide]).await;
    let before = fx.listing();

    let mut reader = fx.reader().await;
    reader.set_dir_limits(10, u64::MAX);
    let wide_limit = |err: &Error| {
        matches!(
            err,
            Error::WalkLimit {
                what: "entries in one directory",
                limit: 10
            }
        )
    };
    let err = reader
        .restore(&snap, &fx.target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap_err();
    assert!(wide_limit(&err), "{err:?}");
    assert_eq!(fx.listing(), before);
    let err = reader.list_dir(&snap, b"/w").await.unwrap_err();
    assert!(wide_limit(&err), "{err:?}");
    let err = reader
        .walk_subtree(&snap, b"/", &mut |_, _| Ok(()))
        .await
        .unwrap_err();
    assert!(wide_limit(&err), "{err:?}");
    assert_eq!(reader.list_dir(&snap, b"/").await.unwrap().len(), 1);
}

/// The check reports the snapshot, stops reading the directory at the piece
/// that passes the limit and goes on with the next snapshot.
#[tokio::test]
async fn check_hostile_wide_directory_is_a_finding() {
    let mut fx = Fx::new().await;
    let pieces: Vec<Vec<u8>> = ["a", "b", "c", "d"]
        .iter()
        .map(|p| {
            let id = fx.tree(files(p, 5));
            id.to_vec()
        })
        .collect();
    let mut wide = dir("w", fx.tree(Vec::new()));
    wide.subtrees = pieces;
    let snap = fx.snapshot(vec![wide]).await;
    fx.snapshot(files("g", 2)).await;

    let report = fx.check_limited(10, u64::MAX).await;
    assert_eq!(report.snapshots_checked, 2);
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    let f = &report.findings[0];
    assert_eq!(f.key, snapshot_object_key(&snap));
    assert_eq!(f.kind, FindingKind::UnreadableTree);
    assert!(f.detail.contains("entries in one directory"), "{f:?}");
    assert_eq!(report.trees_checked, 5);

    let report = fx.check_limited(20, u64::MAX).await;
    assert!(report.findings.is_empty(), "{:?}", report.findings);
}

#[tokio::test]
async fn restore_hostile_held_entries_stop_by_limit() {
    let mut fx = Fx::new().await;
    let inner = fx.tree(files("i", 6));
    let mut top = vec![dir("d0", inner)];
    top.extend(files("f", 5));
    let snap = fx.snapshot(top).await;
    let before = fx.listing();

    let mut reader = fx.reader().await;
    reader.set_dir_limits(10, 10);
    let held_limit = |err: &Error| {
        matches!(
            err,
            Error::WalkLimit {
                what: "entries held at once",
                limit: 10
            }
        )
    };
    let err = reader
        .restore(&snap, &fx.target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap_err();
    assert!(held_limit(&err), "{err:?}");
    assert_eq!(fx.listing(), before);
    let err = reader
        .walk_subtree(&snap, b"/", &mut |_, _| Ok(()))
        .await
        .unwrap_err();
    assert!(held_limit(&err), "{err:?}");
    assert_eq!(reader.list_dir(&snap, b"/").await.unwrap().len(), 6);
}

#[tokio::test]
async fn check_hostile_held_entries_are_a_finding() {
    let mut fx = Fx::new().await;
    let inner = fx.tree(files("i", 6));
    let mut top = vec![dir("d0", inner)];
    top.extend(files("f", 5));
    let snap = fx.snapshot(top).await;

    let report = fx.check_limited(10, 10).await;
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    let f = &report.findings[0];
    assert_eq!(f.key, snapshot_object_key(&snap));
    assert_eq!(f.kind, FindingKind::UnreadableTree);
    assert!(f.detail.contains("entries held at once"), "{f:?}");

    let report = fx.check_limited(10, 12).await;
    assert!(report.findings.is_empty(), "{:?}", report.findings);
}

/// Two snapshots share a directory of 5 entries: `shallow` holds it under a
/// root of 1 entry, `deep` under a root of 8. With a held limit of 10 only
/// `deep` passes it; the snapshots are made again until the check reads them
/// in the wanted order.
async fn shared_dir_pair(shallow_first: bool) -> (Fx, BlobId, BlobId) {
    for _ in 0..64 {
        let mut fx = Fx::new().await;
        let shared = fx.tree(files("d", 5));
        let shallow = fx.snapshot(vec![dir("d", shared)]).await;
        let mut top = vec![dir("d", shared)];
        top.extend(files("f", 7));
        let deep = fx.snapshot(top).await;
        if (snapshot_object_key(&shallow) < snapshot_object_key(&deep)) == shallow_first {
            return (fx, shallow, deep);
        }
    }
    panic!("no pair in the wanted order");
}

async fn assert_only_deep_is_found(shallow_first: bool) {
    let (fx, shallow, deep) = shared_dir_pair(shallow_first).await;
    let report = fx.check_limited(10, 10).await;
    assert_eq!(report.snapshots_checked, 2);
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    let f = &report.findings[0];
    assert_eq!(f.key, snapshot_object_key(&deep));
    assert_eq!(f.kind, FindingKind::UnreadableTree);
    assert!(f.detail.contains("entries held at once"), "{f:?}");
    assert!(f.detail.contains("--allow-large-directories"), "{f:?}");

    let mut reader = fx.reader().await;
    reader.set_dir_limits(10, 10);
    reader
        .walk_subtree(&shallow, b"/", &mut |_, _| Ok(()))
        .await
        .unwrap();
    let err = reader
        .walk_subtree(&deep, b"/", &mut |_, _| Ok(()))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::WalkLimit {
                what: "entries held at once",
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn check_hostile_held_shared_dir_shallow_then_deep() {
    assert_only_deep_is_found(true).await;
}

#[tokio::test]
async fn check_hostile_held_shared_dir_deep_then_shallow() {
    assert_only_deep_is_found(false).await;
}

/// Every directory ends with the next one, so when the walk enters it the
/// directory above has nothing left to hand out but is still held whole.
#[tokio::test]
async fn restore_hostile_held_entries_count_consumed_directories() {
    let mut fx = Fx::new().await;
    let mut inner = fx.tree(files("f", 5));
    for _ in 0..2 {
        let mut nodes = files("f", 4);
        nodes.push(dir("z", inner));
        inner = fx.tree(nodes);
    }
    let mut top = files("f", 4);
    top.push(dir("z", inner));
    let snap = fx.snapshot(top).await;
    let before = fx.listing();

    let mut reader = fx.reader().await;
    reader.set_dir_limits(10, 12);
    let held_limit = |err: &Error| {
        matches!(
            err,
            Error::WalkLimit {
                what: "entries held at once",
                limit: 12
            }
        )
    };
    let err = reader
        .restore(&snap, &fx.target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap_err();
    assert!(held_limit(&err), "{err:?}");
    assert_eq!(fx.listing(), before);
    let err = reader
        .walk_subtree(&snap, b"/", &mut |_, _| Ok(()))
        .await
        .unwrap_err();
    assert!(held_limit(&err), "{err:?}");

    reader.set_dir_limits(10, 20);
    reader
        .walk_subtree(&snap, b"/", &mut |_, _| Ok(()))
        .await
        .unwrap();
}

/// The same chain as above: every directory on the path counts whole.
#[tokio::test]
async fn check_hostile_held_entries_count_consumed_directories() {
    let mut fx = Fx::new().await;
    let mut inner = fx.tree(files("f", 5));
    for _ in 0..2 {
        let mut nodes = files("f", 4);
        nodes.push(dir("z", inner));
        inner = fx.tree(nodes);
    }
    let mut top = files("f", 4);
    top.push(dir("z", inner));
    let snap = fx.snapshot(top).await;

    let report = fx.check_limited(10, 12).await;
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    let f = &report.findings[0];
    assert_eq!(f.key, snapshot_object_key(&snap));
    assert_eq!(f.kind, FindingKind::UnreadableTree);
    assert!(f.detail.contains("entries held at once"), "{f:?}");

    let report = fx.check_limited(10, 20).await;
    assert!(report.findings.is_empty(), "{:?}", report.findings);
}

/// The chain of the tests above: a root and two levels of `f0..f3` plus `z`,
/// and a leaf of `f0..f4`; with the tree bytes of the four directories.
async fn held_chain(fx: &mut Fx) -> (BlobId, u64) {
    let mut held = encode_tree(files("f", 5)).len() as u64;
    let mut inner = fx.tree(files("f", 5));
    for _ in 0..2 {
        let mut nodes = files("f", 4);
        nodes.push(dir("z", inner));
        held += encode_tree(nodes.clone()).len() as u64;
        inner = fx.tree(nodes);
    }
    let mut top = files("f", 4);
    top.push(dir("z", inner));
    held += encode_tree(top.clone()).len() as u64;
    (fx.snapshot(top).await, held)
}

/// The parent a backup reads is held to the limits of a path, as a restore is.
#[tokio::test]
async fn parent_tree_held_entries_stop_by_limit() {
    let mut fx = Fx::new().await;
    let (snap, _) = held_chain(&mut fx).await;

    let mut reader = fx.reader().await;
    reader.set_dir_limits(10, 12);
    let mut parent = ParentTree::open(reader, snap).await.unwrap();
    let err = parent.lookup(b"/z/z/z/f0").await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::WalkLimit {
                what: "entries held at once",
                limit: 12
            }
        ),
        "{err:?}"
    );

    let mut reader = fx.reader().await;
    reader.set_dir_limits(10, 20);
    let mut parent = ParentTree::open(reader, snap).await.unwrap();
    let node = parent.lookup(b"/z/z/z/f0").await.unwrap().unwrap();
    assert_eq!(node.name, b"f0");
    assert!(parent.lookup(b"/z/f1").await.unwrap().is_some());
    assert!(parent.lookup(b"/z/z/z/f9").await.unwrap().is_none());
}

#[tokio::test]
async fn parent_tree_held_bytes_stop_by_limit() {
    let mut fx = Fx::new().await;
    let (snap, held) = held_chain(&mut fx).await;

    let mut reader = fx.reader().await;
    reader.set_dir_byte_limits(u64::MAX, held - 1);
    let mut parent = ParentTree::open(reader, snap).await.unwrap();
    let err = parent.lookup(b"/z/z/z/f0").await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::WalkLimit {
                what: "bytes held at once",
                ..
            }
        ),
        "{err:?}"
    );

    let mut reader = fx.reader().await;
    reader.set_dir_byte_limits(u64::MAX, held);
    let mut parent = ParentTree::open(reader, snap).await.unwrap();
    assert!(parent.lookup(b"/z/z/z/f0").await.unwrap().is_some());
}

fn finding_kinds(report: &StructureReport, snapshot: &BlobId) -> Vec<FindingKind> {
    let key = snapshot_object_key(snapshot);
    report
        .findings
        .iter()
        .filter(|f| f.key == key)
        .map(|f| f.kind)
        .collect()
}

/// A finding in a directory two snapshots share belongs to each of them, in
/// whatever order the check reads them.
#[tokio::test]
async fn check_hostile_finding_below_shared_dir_reaches_every_snapshot() {
    for deep in [false, true] {
        let mut fx = Fx::new().await;
        let mut shared = fx.tree(vec![file("x", &[[9u8; 32]], 1)]);
        let mut trees = 3;
        if deep {
            shared = fx.tree(vec![dir("e", shared)]);
            trees += 1;
        }
        let first = fx.snapshot(vec![dir("d", shared)]).await;
        let mut top = vec![dir("d", shared)];
        top.push(file("f", &[], 0));
        let second = fx.snapshot(top).await;

        let report = fx.check().await;
        assert_eq!(report.snapshots_checked, 2);
        for snap in [&first, &second] {
            assert_eq!(
                finding_kinds(&report, snap),
                [FindingKind::UnknownBlob],
                "deep={deep}: {:?}",
                report.findings
            );
        }
        assert_eq!(report.trees_checked, trees, "deep={deep}");
    }
}

/// A tree blob whose pack is gone is a finding of every snapshot that reaches
/// it, and the check goes on to the next one.
#[tokio::test]
async fn check_hostile_missing_tree_pack_is_a_snapshot_finding() {
    let mut fx = Fx::new().await;
    let shared = fx.tree(files("s", 3));
    let decoy = fx.snapshot(files("pad", 1)).await;
    fs::remove_dir_all(fx.repo_dir.join("packs")).unwrap();
    fs::remove_file(fx.repo_dir.join(snapshot_object_key(&decoy))).unwrap();
    let first = fx.snapshot(vec![dir("s", shared)]).await;
    let mut top = vec![dir("s", shared)];
    top.extend(files("f", 2));
    let second = fx.snapshot(top).await;
    let clean = fx.snapshot(files("c", 2)).await;

    let report = fx.check().await;
    assert_eq!(report.snapshots_checked, 3);
    for snap in [&first, &second] {
        assert_eq!(
            finding_kinds(&report, snap),
            [FindingKind::UnreadableTree],
            "{:?}",
            report.findings
        );
    }
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.kind == FindingKind::UnreadableTree
                && f.detail.contains("is missing from the bucket")),
        "{:?}",
        report.findings
    );
    assert!(finding_kinds(&report, &clean).is_empty());
}

#[tokio::test]
async fn restore_hostile_hardlink_group_mismatch() {
    let mut fx = Fx::new().await;
    let (one, two) = (fx.data(b"12345"), fx.data(b"123456"));
    let mut x = file("x", &[one], 5);
    let mut y = file("y", &[two], 6);
    x.hardlink_group = 7;
    y.hardlink_group = 7;
    let snap = fx.hostile(vec![x, y]).await;
    fx.assert_refused(&snap).await;
    fx.assert_check_finds(&snap).await;
}

fn in_group(mut node: pb::Node, group: u64) -> pb::Node {
    node.hardlink_group = group;
    node
}

/// A hardlink group whose members differ is a finding of the snapshot that
/// has both members, not of the others that share one of the directories, and
/// not of the one the check happens to read second.
#[tokio::test]
async fn check_hostile_hardlink_conflict_stays_with_its_snapshot() {
    for conflicted_first in [false, true] {
        let mut found = None;
        for _ in 0..64 {
            let mut fx = Fx::new().await;
            let (c5, c6) = (fx.data(b"12345"), fx.data(b"123456"));
            let shared = fx.tree(vec![in_group(file("m", &[c5], 5), 7)]);
            let other = fx.tree(vec![in_group(file("m", &[c6], 6), 7)]);
            let conflicted = fx.snapshot(vec![dir("a", shared), dir("b", other)]).await;
            let honest = fx.snapshot(vec![dir("a", shared)]).await;
            if (snapshot_object_key(&conflicted) < snapshot_object_key(&honest)) == conflicted_first
            {
                found = Some((fx, conflicted, honest));
                break;
            }
        }
        let (fx, conflicted, honest) = found.expect("no pair in the wanted order");

        let report = fx.check().await;
        assert_eq!(report.snapshots_checked, 2);
        assert_eq!(
            finding_kinds(&report, &conflicted),
            [FindingKind::UnreadableTree],
            "first={conflicted_first}: {:?}",
            report.findings
        );
        assert!(
            report.findings[0].detail.contains("hardlink group"),
            "{:?}",
            report.findings
        );
        assert!(
            finding_kinds(&report, &honest).is_empty(),
            "first={conflicted_first}: {:?}",
            report.findings
        );

        let err = fx
            .restore(&conflicted, &RestoreOptions::default())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Corrupt { reason, .. } if reason.contains("members of one hardlink group differ")),
            "{err:?}"
        );
        fx.restore(&honest, &RestoreOptions::default())
            .await
            .unwrap();
    }
}

/// A conflict between a directory and one below it is a finding of every
/// snapshot that reaches the directory, whichever is read first.
#[tokio::test]
async fn check_hostile_hardlink_conflict_inside_shared_dir_reaches_every_snapshot() {
    for p_first in [false, true] {
        let mut found = None;
        for _ in 0..64 {
            let mut fx = Fx::new().await;
            let (c5, c6) = (fx.data(b"12345"), fx.data(b"123456"));
            let below = fx.tree(vec![in_group(file("y", &[c6], 6), 7)]);
            let shared = fx.tree(vec![dir("s", below), in_group(file("x", &[c5], 5), 7)]);
            let p = fx.snapshot(vec![dir("d", shared)]).await;
            let q = fx.snapshot(vec![dir("d", shared), file("f", &[], 0)]).await;
            if (snapshot_object_key(&p) < snapshot_object_key(&q)) == p_first {
                found = Some((fx, p, q));
                break;
            }
        }
        let (fx, p, q) = found.expect("no pair in the wanted order");

        let report = fx.check().await;
        for snap in [&p, &q] {
            assert_eq!(
                finding_kinds(&report, snap),
                [FindingKind::UnreadableTree],
                "first={p_first}: {:?}",
                report.findings
            );
            let key = snapshot_object_key(snap);
            assert!(
                report
                    .findings
                    .iter()
                    .any(|f| f.key == key && f.detail.contains("hardlink group")),
                "{:?}",
                report.findings
            );
            fx.restore(snap, &RestoreOptions::default())
                .await
                .unwrap_err();
        }
    }
}

#[tokio::test]
async fn restore_hostile_size_mismatch() {
    let mut fx = Fx::new().await;
    let id = fx.data(b"12345");
    let snap = fx.hostile(vec![file("liar", &[id], 10)]).await;
    fx.assert_refused(&snap).await;
    fx.assert_check_finds(&snap).await;

    let mut fx = Fx::new().await;
    let id = fx.data(b"12345");
    let snap = fx.hostile(vec![file("liar", &[id], 2)]).await;
    fx.assert_refused(&snap).await;
    fx.assert_check_finds(&snap).await;
}

#[tokio::test]
async fn restore_hostile_existing_symlink_not_followed() {
    for (overwrite, replaced) in [
        (Overwrite::IfChanged, true),
        (Overwrite::Always, true),
        (Overwrite::Never, false),
    ] {
        let mut fx = Fx::new().await;
        let outside = fx.root.join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, fx.target.join("a/x")).unwrap();

        let data = fx.data(b"payload");
        let evil = fx.tree(vec![file("f.txt", &[data], 7)]);
        let x = fx.tree(vec![dir("evil", evil)]);
        let a = fx.tree(vec![dir("x", x)]);
        let snap = fx.snapshot(vec![dir("a", a)]).await;

        let report = fx
            .restore(
                &snap,
                &RestoreOptions {
                    overwrite,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(
            listing(&outside).is_empty(),
            "{overwrite:?}: nothing leaked out"
        );
        let meta = fs::symlink_metadata(fx.target.join("a/x")).unwrap();
        if replaced {
            assert!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "{overwrite:?}"
            );
            assert_eq!(
                fs::read(fx.target.join("a/x/evil/f.txt")).unwrap(),
                b"payload"
            );
            assert!(report.errors.is_empty(), "{:?}", report.errors);
        } else {
            assert!(meta.file_type().is_symlink());
            assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
            assert_eq!(report.errors[0].path, b"a/x");
        }
    }
}

#[tokio::test]
async fn restore_hostile_bomb_ulen_lie() {
    let mut fx = Fx::new().await;
    let bomb = zstd::bulk::compress(&vec![0u8; 4 << 20], 3).unwrap();
    assert!(bomb.len() < 1024);
    let id = fx
        .writer
        .put_blob_raw(BLOB_TYPE_DATA, CODEC_ZSTD, &bomb, 1024)
        .unwrap();
    let good = fx.data(b"still restored");
    let d = fx.tree(vec![
        file("bomb.bin", &[id], 1024),
        file("good.txt", &[good], 14),
    ]);
    let snap = fx.snapshot(vec![dir("d", d)]).await;

    let report = fx.restore(&snap, &RestoreOptions::default()).await.unwrap();
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert_eq!(report.errors[0].path, b"d/bomb.bin");
    assert!(!fx.target.join("d/bomb.bin").exists());
    assert!(
        leftovers(&fx.target).is_empty(),
        "{:?}",
        leftovers(&fx.target)
    );
    assert_eq!(
        fs::read(fx.target.join("d/good.txt")).unwrap(),
        b"still restored"
    );
    assert_eq!(report.files, 1);
}

#[tokio::test]
async fn restore_hostile_ulen_over_cap() {
    let mut fx = Fx::new().await;
    let bomb = zstd::bulk::compress(&vec![0u8; 1 << 20], 3).unwrap();
    let id = fx
        .writer
        .put_blob_raw(BLOB_TYPE_DATA, CODEC_ZSTD, &bomb, 64 << 20)
        .unwrap();
    let snap = fx.hostile(vec![file("huge", &[id], 64 << 20)]).await;
    fx.assert_refused(&snap).await;
}

#[tokio::test]
async fn restore_hostile_oversized_tree_blob() {
    let mut fx = Fx::new().await;
    let small = encode_tree(vec![file("f", &[], 0)]);
    let id = fx
        .writer
        .put_blob_raw(BLOB_TYPE_TREE, CODEC_RAW, &small, 17 << 20)
        .unwrap();
    let mut big = dir("big", id);
    big.subtrees = vec![id.to_vec()];
    let snap = fx.hostile(vec![big]).await;
    fx.assert_refused(&snap).await;
}

fn xattr(path: &Path, name: &str) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; 256];
    match rustix::fs::lgetxattr(path, name, &mut buf[..]) {
        Ok(n) => {
            buf.truncate(n);
            Some(buf)
        }
        Err(_) => None,
    }
}

#[tokio::test]
async fn restore_hostile_no_owners_drops_setuid_and_capabilities() {
    let mut fx = Fx::new().await;
    let data = fx.data(b"#!/bin/sh\n");
    let mut suid = file("suid", &[data], 10);
    suid.mode = 0o4755;
    suid.xattrs = vec![
        pb::Xattr {
            name: b"security.capability".to_vec(),
            value: vec![1, 0, 0, 2],
        },
        pb::Xattr {
            name: b"user.lodestore.test".to_vec(),
            value: b"kept".to_vec(),
        },
    ];
    let snap = fx.snapshot(vec![suid]).await;

    let report = fx
        .restore(
            &snap,
            &RestoreOptions {
                skip_owners: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let out = fx.target.join("suid");
    assert_eq!(fs::metadata(&out).unwrap().mode() & 0o7777, 0o755);
    assert_eq!(xattr(&out, "security.capability"), None);
    assert!(
        !report
            .unapplied
            .iter()
            .any(|u| u.what.contains("security.capability")),
        "{:?}",
        report.unapplied
    );
    assert_eq!(xattr(&out, "user.lodestore.test"), Some(b"kept".to_vec()));

    fs::remove_file(&out).unwrap();
    fx.restore(&snap, &RestoreOptions::default()).await.unwrap();
    assert_eq!(fs::metadata(&out).unwrap().mode() & 0o7777, 0o4755);
}

fn bytes_in_dir_limit(err: &Error, limit: u64) -> bool {
    matches!(
        err,
        Error::WalkLimit {
            what: "bytes in one directory",
            limit: l
        } if *l == limit
    )
}

/// A directory of pieces that add up to more than the real limit is refused
/// by the sizes the index lists, before any piece is downloaded.
#[tokio::test]
async fn restore_hostile_wide_directory_stops_by_byte_limit() {
    let mut fx = Fx::new().await;
    let pieces = tree_support::wide_piece_count();
    let snap = tree_support::wide_snapshot(&mut fx.writer, pieces).await;
    let before = fx.listing();

    let store = RangedOnly::new(LocalFsStore::new(fx.repo_dir.clone()), &[]);
    let repo = Repository::open(&store, b"pw").await.unwrap();
    let mut reader = Reader::open(&store, repo.read_keys().duplicate(), repo.config()).unwrap();

    let err = reader
        .restore(&snap, &fx.target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap_err();
    assert!(bytes_in_dir_limit(&err, MAX_DIR_TREE_BYTES), "{err:?}");
    assert_eq!(fx.listing(), before);
    let err = reader.list_dir(&snap, b"/wide").await.unwrap_err();
    assert!(bytes_in_dir_limit(&err, MAX_DIR_TREE_BYTES), "{err:?}");
    let err = reader
        .walk_subtree(&snap, b"/", &mut |_, _| Ok(()))
        .await
        .unwrap_err();
    assert!(bytes_in_dir_limit(&err, MAX_DIR_TREE_BYTES), "{err:?}");
    let err = reader
        .selection_totals(&snap, &Selection::all())
        .await
        .unwrap_err();
    assert!(bytes_in_dir_limit(&err, MAX_DIR_TREE_BYTES), "{err:?}");

    let pack_reads = store.ranges_under("packs/").len();
    assert!(pack_reads < pieces, "{pack_reads} reads of packs");
}

/// Pieces the shards the snapshot names do not list count for nothing before
/// they are read, and by their length once they are.
#[tokio::test]
async fn restore_hostile_byte_limit_counts_pieces_the_index_did_not_list() {
    let mut fx = Fx::new().await;
    let sets = ["a", "b", "c", "d"].map(|p| files(p, 5));
    let lens = sets.clone().map(|n| encode_tree(n).len() as u64);
    let ids = sets.map(|n| fx.tree(n));
    fx.snapshot(files("x", 1)).await;
    let mut wide = dir("w", fx.tree(Vec::new()));
    wide.subtrees = ids.iter().map(|id| id.to_vec()).collect();
    let snap = fx.snapshot(vec![wide]).await;
    let before = fx.listing();

    let mut reader = fx.reader().await;
    let limit = lens[0] + lens[1];
    reader.set_dir_byte_limits(limit, u64::MAX);
    let err = reader
        .restore(&snap, &fx.target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap_err();
    assert!(bytes_in_dir_limit(&err, limit), "{err:?}");
    assert_eq!(fx.listing(), before);

    reader.set_dir_byte_limits(lens.iter().sum(), u64::MAX);
    reader
        .restore(&snap, &fx.target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap();
}

fn held_bytes_limit(err: &Error, limit: u64) -> bool {
    matches!(
        err,
        Error::WalkLimit {
            what: "bytes held at once",
            limit: l
        } if *l == limit
    )
}

/// The directories open together are held by their tree bytes, and the
/// directory that would pass the limit is not read.
#[tokio::test]
async fn restore_hostile_held_bytes_stop_by_limit() {
    let mut fx = Fx::new().await;
    let inner_nodes = files("i", 6);
    let inner = fx.tree(inner_nodes.clone());
    let mut top = vec![dir("d0", inner)];
    top.extend(files("f", 5));
    let held = (encode_tree(top.clone()).len() + encode_tree(inner_nodes).len()) as u64;
    let snap = fx.snapshot(top).await;
    let before = fx.listing();

    let mut reader = fx.reader().await;
    reader.set_dir_byte_limits(u64::MAX, held - 1);
    let err = reader
        .restore(&snap, &fx.target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap_err();
    assert!(held_bytes_limit(&err, held - 1), "{err:?}");
    assert_eq!(fx.listing(), before);
    let err = reader
        .walk_subtree(&snap, b"/", &mut |_, _| Ok(()))
        .await
        .unwrap_err();
    assert!(held_bytes_limit(&err, held - 1), "{err:?}");
    assert_eq!(reader.list_dir(&snap, b"/").await.unwrap().len(), 6);

    reader.allow_large_directories();
    reader
        .restore(&snap, &fx.target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap();

    let mut reader = fx.reader().await;
    reader.set_dir_byte_limits(u64::MAX, held);
    reader
        .restore(&snap, &fx.target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap();

    let mut pack_reads = Vec::new();
    for held_limit in [held - 1, held] {
        let store = RangedOnly::new(LocalFsStore::new(fx.repo_dir.clone()), &[]);
        let repo = Repository::open(&store, b"pw").await.unwrap();
        let mut reader = Reader::open(&store, repo.read_keys().duplicate(), repo.config()).unwrap();
        reader.set_dir_byte_limits(u64::MAX, held_limit);
        let _ = reader.walk_subtree(&snap, b"/", &mut |_, _| Ok(())).await;
        pack_reads.push(store.ranges_under("packs/").len());
    }
    assert!(pack_reads[0] < pack_reads[1], "{pack_reads:?}");
}

/// A chain of directories whose `subtrees` lists are long and whose entries
/// are few: the bytes on the stack, not the entries, run out.
#[tokio::test]
async fn restore_hostile_held_bytes_count_subtree_lists() {
    let mut fx = Fx::new().await;
    let empty = fx.tree(Vec::new());
    let level = |next: BlobId| {
        let mut d = dir("z", empty);
        d.subtrees = vec![empty.to_vec(); 100];
        d.subtrees.push(next.to_vec());
        d
    };
    let mut inner = fx.tree(files("f", 1));
    for _ in 0..9 {
        inner = fx.tree(vec![level(inner)]);
    }
    let snap = fx.snapshot(vec![level(inner)]).await;
    let before = fx.listing();

    let mut reader = fx.reader().await;
    reader.set_dir_byte_limits(u64::MAX, 20_000);
    let err = reader
        .restore(&snap, &fx.target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap_err();
    assert!(held_bytes_limit(&err, 20_000), "{err:?}");
    assert_eq!(fx.listing(), before);

    reader.allow_large_directories();
    reader
        .restore(&snap, &fx.target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap();
}

/// The check refuses a directory that is over the byte limit by the sizes the
/// index lists, without reading its pieces, and goes on with the next snapshot.
#[tokio::test]
async fn check_hostile_wide_directory_over_byte_limit_is_a_finding() {
    let mut fx = Fx::new().await;
    let pieces = tree_support::wide_piece_count();
    let wide = tree_support::write_wide_dir(&mut fx.writer, "wide", pieces);
    let snap = fx.snapshot(vec![wide]).await;
    fx.snapshot(files("g", 2)).await;

    let report = fx.check().await;
    assert_eq!(report.snapshots_checked, 2);
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    let f = &report.findings[0];
    assert_eq!(f.key, snapshot_object_key(&snap));
    assert_eq!(f.kind, FindingKind::UnreadableTree);
    assert!(f.detail.contains("bytes in one directory"), "{f:?}");
    assert!(f.detail.contains("--allow-large-directories"), "{f:?}");
    assert_eq!(report.trees_checked, 2);
}

/// The directories on the path are held by their tree bytes. The check does
/// not stop reading at the limit of a path: the directory that passes it is
/// read, once, and the snapshot gets the finding.
#[tokio::test]
async fn check_hostile_held_bytes_are_a_finding() {
    let mut fx = Fx::new().await;
    let inner_nodes = files("i", 6);
    let inner = fx.tree(inner_nodes.clone());
    let mut top = vec![dir("d0", inner)];
    top.extend(files("f", 5));
    let held = (encode_tree(top.clone()).len() + encode_tree(inner_nodes).len()) as u64;
    let snap = fx.snapshot(top).await;
    let budget = |held_bytes| TreeBudget::default().with_dir_byte_limits(u64::MAX, held_bytes);

    let report = fx.check_budget(budget(held - 1)).await;
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    let f = &report.findings[0];
    assert_eq!(f.key, snapshot_object_key(&snap));
    assert_eq!(f.kind, FindingKind::UnreadableTree);
    assert!(f.detail.contains("bytes held at once"), "{f:?}");

    let report = fx.check_budget(budget(held)).await;
    assert!(report.findings.is_empty(), "{:?}", report.findings);

    let mut pack_reads = Vec::new();
    for held_limit in [held - 1, held] {
        let store = RangedOnly::new(LocalFsStore::new(fx.repo_dir.clone()), &[]);
        let repo = Repository::open(&store, b"pw").await.unwrap();
        check_structure_with_budget(
            &store,
            repo.read_keys(),
            repo.config(),
            &|_, _, _| {},
            budget(held_limit),
        )
        .await
        .unwrap();
        pack_reads.push(store.ranges_under("packs/").len());
    }
    assert_eq!(pack_reads[0], pack_reads[1], "{pack_reads:?}");
}

/// A chain of directories whose `subtrees` lists are long and whose entries
/// are few: the bytes on the path, not the entries, run out.
#[tokio::test]
async fn check_hostile_held_bytes_count_subtree_lists() {
    let mut fx = Fx::new().await;
    let empty = fx.tree(Vec::new());
    let level = |next: BlobId| {
        let mut d = dir("z", empty);
        d.subtrees = vec![empty.to_vec(); 100];
        d.subtrees.push(next.to_vec());
        d
    };
    let mut inner = fx.tree(files("f", 1));
    for _ in 0..9 {
        inner = fx.tree(vec![level(inner)]);
    }
    let snap = fx.snapshot(vec![level(inner)]).await;

    let report = fx
        .check_budget(TreeBudget::default().with_dir_byte_limits(u64::MAX, 20_000))
        .await;
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    let f = &report.findings[0];
    assert_eq!(f.key, snapshot_object_key(&snap));
    assert!(f.detail.contains("bytes held at once"), "{f:?}");

    let report = fx
        .check_budget(TreeBudget::default().with_dir_byte_limits(u64::MAX, u64::MAX))
        .await;
    assert!(report.findings.is_empty(), "{:?}", report.findings);
}

/// Every blob of the record one byte shorter than it is.
fn shrink_ulen(record: &pb::PackRecord) -> pb::PackRecord {
    let mut entries = Vec::with_capacity(record.entries.len());
    for e in parse_entries(&record.entries).unwrap() {
        entries.extend_from_slice(&e.blob_id);
        entries.push(e.blob_type);
        entries.extend_from_slice(&e.offset.to_le_bytes());
        entries.extend_from_slice(&e.enc_len.to_le_bytes());
        entries.extend_from_slice(&(e.ulen - 1).to_le_bytes());
    }
    assert_eq!(entries.len(), record.entries.len());
    assert!(entries.len().is_multiple_of(ENTRY_LEN));
    pb::PackRecord {
        entries,
        ..record.clone()
    }
}

struct SizeLie {
    fx: Fx,
    store: LocalFsStore,
    data_packs: Vec<String>,
    /// Parents of one directory whose file the index lists too short.
    snap: BlobId,
    /// A snapshot the shard lies about nothing in.
    aux: BlobId,
    /// The tree bytes a restore of `snap` reads: the top once, the shared
    /// directory eight times.
    tree_bytes: u64,
    expected: Vec<u8>,
}

/// A file of two chunks, 1000 and 500 bytes, whose index entries a shard
/// planted ahead of the real one lists one byte short each.
async fn size_lie(parents: usize) -> SizeLie {
    let mut fx = Fx::new().await;
    let (first, second) = (vec![1u8; 1000], vec![2u8; 500]);
    let chunks = [fx.data(&first), fx.data(&second)];
    let shared_nodes = vec![file("f", &chunks, 1500)];
    let shared = fx.tree(shared_nodes.clone());
    let aux = fx.snapshot(files("x", 1)).await;

    let store = LocalFsStore::new(fx.repo_dir.clone());
    let repo = Repository::open(&store, b"pw").await.unwrap();
    let keys = attacker_keys(&repo);
    let shards = open_all_shards(&store, &keys).await;
    assert_eq!(shards.len(), 1);
    let (real_id, real) = &shards[0];
    let forged = real.packs.iter().map(shrink_ulen).collect();
    plant_shard_where(&store, &keys, forged, Vec::new(), |id| id < real_id).await;
    let data_packs: Vec<String> = real
        .packs
        .iter()
        .map(|p| pack_object_key(&p.pack_id.as_slice().try_into().unwrap()))
        .collect();

    let top: Vec<pb::Node> = (0..parents)
        .map(|i| dir(&format!("p{i}"), shared))
        .collect();
    let tree_bytes =
        (encode_tree(top.clone()).len() + parents * encode_tree(shared_nodes).len()) as u64;
    let snap = fx.snapshot(top).await;
    SizeLie {
        fx,
        store,
        data_packs,
        snap,
        aux,
        tree_bytes,
        expected: [first, second].concat(),
    }
}

/// A file whose chunks the index lists with a wrong size is read to be
/// checked; however many parents reach the file, that happens once.
#[tokio::test]
async fn restore_hostile_diamond_size_lie_reads_chunks_once() {
    let mut fx = Fx::new().await;
    let (first, second) = (vec![1u8; 1000], vec![2u8; 500]);
    let chunks = [fx.data(&first), fx.data(&second)];
    let shared = fx.tree(vec![file("f", &chunks, 1500)]);
    fx.snapshot(files("x", 1)).await;

    let store = LocalFsStore::new(fx.repo_dir.clone());
    let repo = Repository::open(&store, b"pw").await.unwrap();
    let keys = attacker_keys(&repo);
    let shards = open_all_shards(&store, &keys).await;
    assert_eq!(shards.len(), 1);
    let (real_id, real) = &shards[0];
    let forged = real.packs.iter().map(shrink_ulen).collect();
    plant_shard_where(&store, &keys, forged, Vec::new(), |id| id < real_id).await;
    let data_packs: Vec<String> = real
        .packs
        .iter()
        .map(|p| pack_object_key(&p.pack_id.as_slice().try_into().unwrap()))
        .collect();

    let mut snapshots = Vec::new();
    for parents in [2, 8] {
        let top = (0..parents)
            .map(|i| dir(&format!("p{i}"), shared))
            .collect();
        snapshots.push(fx.snapshot(top).await);
    }

    let mut reads = Vec::new();
    for snap in &snapshots {
        let ranged = RangedOnly::new(&store, &[]);
        let mut reader =
            Reader::open(&ranged, repo.read_keys().duplicate(), repo.config()).unwrap();
        let totals = reader
            .selection_totals(snap, &Selection::all())
            .await
            .unwrap();
        assert_eq!(totals.bytes % 1500, 0);
        let n = data_packs
            .iter()
            .map(|key| ranged.ranges_of(key).len())
            .sum::<usize>();
        reads.push(n);
    }
    assert!(reads[0] > 0, "the lie was never reached");
    assert_eq!(reads[0], reads[1], "chunks were read again on a revisit");

    fx.restore(&snapshots[1], &RestoreOptions::default())
        .await
        .unwrap();
    let expected = [first, second].concat();
    assert_eq!(fs::read(fx.target.join("p5/f")).unwrap(), expected);
}

fn is_walk_limit(err: &Error, what: &str) -> bool {
    matches!(err, Error::WalkLimit { what: w, .. } if *w == what)
}

/// Each visit of a file whose sizes in the index do not add up charges its
/// chunks and their bytes, whether the cache has its length or not. The
/// eight visits of the file cost 8 x 2 entries and 8 x 1500 bytes on top of
/// the 8 + 8 entries and the tree bytes of the directories: 32 in all, the
/// same for a totals pass and for a restore.
#[tokio::test]
async fn restore_hostile_size_lie_counts_against_the_walk() {
    let lie = size_lie(8).await;
    let fx = &lie.fx;
    for lens in [None, Some(0)] {
        let reader = || async {
            let mut reader = fx.reader().await;
            if let Some(n) = lens {
                reader.set_file_lens_limit(n);
            }
            reader
        };
        for (nodes, bytes, ok) in [
            (32, u64::MAX, true),
            (31, u64::MAX, false),
            (u64::MAX, lie.tree_bytes + 12_000, true),
            (u64::MAX, lie.tree_bytes + 11_999, false),
        ] {
            let mut r = reader().await;
            r.set_walk_limits(nodes, bytes);
            let totals = r.selection_totals(&lie.snap, &Selection::all()).await;
            let mut r = reader().await;
            r.set_walk_limits(nodes, bytes);
            let restored = r
                .restore(&lie.snap, &fx.target, &RestoreOptions::default(), &|_| {})
                .await;
            let what = if nodes == u64::MAX {
                "bytes of directory data"
            } else {
                "entries"
            };
            for result in [totals.map(|_| ()), restored.map(|_| ())] {
                match (ok, result) {
                    (true, Ok(())) => {}
                    (false, Err(e)) => assert!(is_walk_limit(&e, what), "{lens:?} {e:?}"),
                    (ok, other) => panic!("{lens:?} {nodes} {bytes} ok={ok}: {other:?}"),
                }
            }
        }
    }

    let mut reader = fx.reader().await;
    reader.set_file_lens_limit(0);
    reader
        .restore(&lie.snap, &fx.target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap();
    assert_eq!(fs::read(fx.target.join("p5/f")).unwrap(), lie.expected);
}

/// The structure check charges the same, once per occurrence of the shared
/// directory, and reads the chunks of the file once however many parents it
/// has.
#[tokio::test]
async fn check_hostile_size_lie_counts_against_the_walk_like_restore() {
    let lie = size_lie(8).await;
    let fx = &lie.fx;
    let refused = |limits: (u64, u64)| {
        let reader = fx.reader();
        async move {
            let mut reader = reader.await;
            reader.set_walk_limits(limits.0, limits.1);
            reader
                .restore(&lie.snap, &fx.target, &RestoreOptions::default(), &|_| {})
                .await
                .unwrap_err()
        }
    };
    let cases = [
        ((31, u64::MAX), TreeBudget::new(31, MAX_WALK_TREE_BYTES)),
        (
            (u64::MAX, lie.tree_bytes + 11_999),
            TreeBudget::new(MAX_WALK_NODES, lie.tree_bytes + 11_999),
        ),
    ];
    for (limits, budget) in cases {
        let err = refused(limits).await;
        let report = fx.check_budget(budget).await;
        assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
        let f = &report.findings[0];
        assert_eq!(f.key, snapshot_object_key(&lie.snap));
        assert_eq!(f.kind, FindingKind::UnreadableTree);
        assert_eq!(f.detail, err.to_string());
        assert_ne!(f.key, snapshot_object_key(&lie.aux));
    }
    for budget in [
        TreeBudget::new(32, MAX_WALK_TREE_BYTES),
        TreeBudget::new(MAX_WALK_NODES, lie.tree_bytes + 12_000),
    ] {
        let report = fx.check_budget(budget).await;
        assert!(report.findings.is_empty(), "{:?}", report.findings);
    }

    let single = size_lie(1).await;
    let (once, many) = (pack_reads(&single).await, pack_reads(&lie).await);
    assert!(once > 0, "the chunks were never read");
    assert_eq!(once, many, "chunks were read again for another parent");
}

/// How many ranges the default structure check reads from the packs.
async fn pack_reads(lie: &SizeLie) -> usize {
    let ranged = RangedOnly::new(&lie.store, &[]);
    let repo = Repository::open(&lie.store, b"pw").await.unwrap();
    let report = check_structure_with_budget(
        &ranged,
        repo.read_keys(),
        repo.config(),
        &|_, _, _| {},
        TreeBudget::default(),
    )
    .await
    .unwrap();
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    lie.data_packs
        .iter()
        .map(|key| ranged.ranges_of(key).len())
        .sum()
}

/// A snapshot of one file that names the same 1000-byte chunk `n` times and
/// says it holds `size` bytes; the index is honest. Returns the blobs read
/// from the pack of the chunk by a restore and by the structure check, each
/// of which must refuse the file without naming a partial sum.
async fn repeated_chunk(n: usize, size: u64) -> (usize, usize) {
    let mut fx = Fx::new().await;
    let c = fx.data(&[7u8; 1000]);
    let snap = fx.snapshot(vec![file("f", &vec![c; n], size)]).await;
    let store = LocalFsStore::new(fx.repo_dir.clone());
    let repo = Repository::open(&store, b"pw").await.unwrap();
    let says = format!("hold more than the {size} bytes the node says");
    let pack = data_pack_of(&store, &repo, &c).await;

    let ranged = RangedOnly::new(&store, &[]);
    let mut reader = Reader::open(&ranged, repo.read_keys().duplicate(), repo.config()).unwrap();
    let err = reader
        .restore(&snap, &fx.target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap_err();
    let restore_reads = blob_reads(&ranged, &pack);
    assert!(matches!(err, Error::Corrupt { .. }), "{err:?}");
    assert!(err.to_string().contains(&says), "{err}");

    let ranged = RangedOnly::new(&store, &[]);
    let report = check_structure_with_budget(
        &ranged,
        repo.read_keys(),
        repo.config(),
        &|_, _, _| {},
        TreeBudget::default(),
    )
    .await
    .unwrap();
    let check_reads = blob_reads(&ranged, &pack);
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    let f = &report.findings[0];
    assert_eq!(f.key, snapshot_object_key(&snap));
    assert_eq!(f.kind, FindingKind::UnreadableTree);
    assert!(f.detail.contains("chunks hold"), "{f:?}");
    assert!(f.detail.contains(&says), "{f:?}");
    (restore_reads, check_reads)
}

/// Each operation reads the chunk exactly once, for 4, 64 and 1024
/// references, at a size of zero and at one byte short of what the chunks
/// hold.
async fn repeated_chunk_reads(pick: fn((usize, usize)) -> usize) {
    for short in [false, true] {
        for n in [4usize, 64, 1024] {
            let size = if short { n as u64 * 1000 - 1 } else { 0 };
            let reads = pick(repeated_chunk(n, size).await);
            assert_eq!(reads, 1, "short={short}, n={n}");
        }
    }
}

/// A file that names one chunk many times and lies about its size costs a
/// restore the same reads however many times it names the chunk.
#[tokio::test]
async fn restore_hostile_repeated_chunk_reads_do_not_grow() {
    repeated_chunk_reads(|(restore, _)| restore).await;
}

/// The same for the structure check.
#[tokio::test]
async fn check_hostile_repeated_chunk_reads_do_not_grow() {
    repeated_chunk_reads(|(_, check)| check).await;
}

/// The chunks of files whose sizes in the index do not add up are read
/// within one walk budget for the whole check. Only `check_file` charges it:
/// 1500 bytes for `d1` and 1500 for `d2`, once each, since a directory is
/// read once; `aux` charges nothing. With 3000 there is no finding, so the
/// tree bytes every snapshot has are not charged to it; with 2999 there is
/// one, although `d1` and `d2` sit in different snapshots that would each
/// charge 1500 alone, so the budget is not started again between snapshots.
/// The directory read second is the one not checked, and every snapshot that
/// reaches it has the finding.
#[tokio::test]
async fn check_hostile_chunk_read_limit_is_a_finding() {
    let mut fx = Fx::new().await;
    let c1 = fx.data(&[1u8; 1000]);
    let c2 = fx.data(&[2u8; 500]);
    let c3 = fx.data(&[3u8; 1000]);
    let c4 = fx.data(&[4u8; 500]);
    let d1_nodes = vec![file("f", &[c1, c2], 1500)];
    let d2_nodes = vec![file("g", &[c3, c4], 1500)];
    let d1 = fx.tree(d1_nodes.clone());
    let d2 = fx.tree(d2_nodes.clone());
    let aux = fx.snapshot(files("x", 1)).await;

    let store = LocalFsStore::new(fx.repo_dir.clone());
    let repo = Repository::open(&store, b"pw").await.unwrap();
    let keys = attacker_keys(&repo);
    let shards = open_all_shards(&store, &keys).await;
    assert_eq!(shards.len(), 1);
    let (real_id, real) = &shards[0];
    let forged = real.packs.iter().map(shrink_ulen).collect();
    plant_shard_where(&store, &keys, forged, Vec::new(), |id| id < real_id).await;

    for (top, nodes) in [
        (vec![dir("p", d1)], &d1_nodes),
        (vec![dir("p", d2)], &d2_nodes),
        (vec![dir("q", d2)], &d2_nodes),
    ] {
        let tree_bytes = encode_tree(top).len() + encode_tree(nodes.clone()).len();
        assert!(tree_bytes + 1500 <= 2999, "{tree_bytes}");
    }
    let s1 = fx.snapshot(vec![dir("p", d1)]).await;
    let s2 = fx.snapshot(vec![dir("p", d2)]).await;
    let s3 = fx.snapshot(vec![dir("q", d2)]).await;

    let report = fx.check_budget(TreeBudget::new(MAX_WALK_NODES, 2999)).await;
    assert!(!report.findings.is_empty());
    for f in &report.findings {
        assert_eq!(f.kind, FindingKind::UnreadableTree, "{f:?}");
        assert!(f.detail.contains("was not checked"), "{f:?}");
    }
    let keys: std::collections::BTreeSet<String> =
        report.findings.iter().map(|f| f.key.clone()).collect();
    let set = |ids: &[&BlobId]| ids.iter().map(|id| snapshot_object_key(id)).collect();
    assert!(
        keys == set(&[&s1]) || keys == set(&[&s2, &s3]),
        "{:?}",
        report.findings
    );
    assert!(!keys.contains(&snapshot_object_key(&aux)));

    let report = fx.check_budget(TreeBudget::new(MAX_WALK_NODES, 3000)).await;
    assert!(report.findings.is_empty(), "{:?}", report.findings);
}

/// The key of the pack that holds the data blob `id`.
async fn data_pack_of(store: &LocalFsStore, repo: &Repository, id: &BlobId) -> String {
    let pack = open_all_shards(store, &attacker_keys(repo))
        .await
        .into_iter()
        .flat_map(|(_, shard)| shard.packs)
        .find(|p| {
            parse_entries(&p.entries)
                .unwrap()
                .iter()
                .any(|e| e.blob_type == BLOB_TYPE_DATA && e.blob_id == *id)
        })
        .unwrap();
    pack_object_key(&pack.pack_id.as_slice().try_into().unwrap())
}

/// The blobs read from `pack`, leaving out the reads of its prefix.
fn blob_reads<S>(ranged: &RangedOnly<S>, pack: &str) -> usize {
    let prefix = 0..PACK_PREFIX_LEN as u64;
    ranged
        .ranges_of(pack)
        .into_iter()
        .filter(|r| *r != prefix)
        .count()
}

/// The reads of one 1000-byte chunk by a check of one directory: `e` names
/// the chunk twice and spends the bytes of the budget on its second reference,
/// then `k` files each name it once and say they are empty. The index is
/// honest. The snapshot is reported, with files not checked.
async fn spent_budget_reads(k: usize) -> usize {
    let mut fx = Fx::new().await;
    let c = fx.data(&[7u8; 1000]);
    let mut nodes = vec![file("e", &[c, c], 1999)];
    nodes.extend((0..k).map(|i| file(&format!("f{i:04}"), &[c], 0)));
    let d = fx.tree(nodes);
    let snap = fx.snapshot(vec![dir("d", d)]).await;
    let store = LocalFsStore::new(fx.repo_dir.clone());
    let repo = Repository::open(&store, b"pw").await.unwrap();
    let pack = data_pack_of(&store, &repo, &c).await;

    let ranged = RangedOnly::new(&store, &[]);
    let report = check_structure_with_budget(
        &ranged,
        repo.read_keys(),
        repo.config(),
        &|_, _, _| {},
        TreeBudget::new(MAX_WALK_NODES, 1500),
    )
    .await
    .unwrap();
    for f in &report.findings {
        assert_eq!(f.key, snapshot_object_key(&snap));
        assert_eq!(f.kind, FindingKind::UnreadableTree, "{f:?}");
    }
    let unchecked = report
        .findings
        .iter()
        .filter(|f| f.detail.contains("was not checked"))
        .count();
    assert!(unchecked > 0, "{:?}", report.findings);
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.detail.contains("at most 1500 bytes of chunks of files")),
        "{:?}",
        report.findings
    );
    blob_reads(&ranged, &pack)
}

/// Once the bytes of the check's budget are spent, the files after that read
/// no chunk, however many there are.
#[tokio::test]
async fn check_hostile_spent_budget_reads_no_more_chunks() {
    for k in [4usize, 64, 1024] {
        assert_eq!(spent_budget_reads(k).await, 1, "k={k}");
    }
}

/// The same record with the decoded size of every blob `f` picks changed.
fn change_ulen(record: &pb::PackRecord, f: impl Fn(u8, u32) -> u32) -> pb::PackRecord {
    let mut entries = Vec::with_capacity(record.entries.len());
    for e in parse_entries(&record.entries).unwrap() {
        entries.extend_from_slice(&e.blob_id);
        entries.push(e.blob_type);
        entries.extend_from_slice(&e.offset.to_le_bytes());
        entries.extend_from_slice(&e.enc_len.to_le_bytes());
        entries.extend_from_slice(&f(e.blob_type, e.ulen).to_le_bytes());
    }
    pb::PackRecord {
        entries,
        ..record.clone()
    }
}

/// `snap` sealed again by a writer-keys holder, naming only `refs`.
async fn reseal(fx: &Fx, snap: &BlobId, refs: &[BlobId]) -> BlobId {
    let store = LocalFsStore::new(fx.repo_dir.clone());
    let repo = Repository::open(&store, b"pw").await.unwrap();
    let keys = attacker_keys(&repo);
    let mut snapshot = fx.reader().await.snapshot(snap).await.unwrap();
    snapshot.index_refs = refs.iter().map(|r| r.to_vec()).collect();
    let (id, bytes) = seal_snapshot(
        &snapshot,
        &keys.hpke_public,
        &keys.repo_id,
        &mut Entropy::os(),
    )
    .unwrap();
    put_named(&store, &snapshot_object_key(&id), bytes)
        .await
        .unwrap();
    id
}

/// A shard that copies the real records with every tree listed larger, and
/// a copy of an honest snapshot that names only that shard. A restore sizes a
/// directory by the shards its snapshot names, so it refuses the copy before
/// reading the root; the structure check reports the copy and not the
/// original, in whichever order it reads them.
#[tokio::test]
async fn check_hostile_snapshot_shard_lists_trees_larger_like_restore() {
    let mut fx = Fx::new().await;
    let top = files("f", 3);
    let limit = encode_tree(top.clone()).len() as u64;
    let honest = fx.snapshot(top).await;
    let store = LocalFsStore::new(fx.repo_dir.clone());
    let repo = Repository::open(&store, b"pw").await.unwrap();
    let keys = attacker_keys(&repo);
    let shards = open_all_shards(&store, &keys).await;
    assert_eq!(shards.len(), 1);
    let grown = shards[0]
        .1
        .packs
        .iter()
        .map(|r| change_ulen(r, |t, u| if t == BLOB_TYPE_TREE { u + 1000 } else { u }))
        .collect();
    let planted = plant_shard(&store, &keys, grown, Vec::new()).await;
    let copy = reseal(&fx, &honest, &[planted]).await;

    let mut reader = fx.reader().await;
    reader.set_dir_byte_limits(limit, u64::MAX);
    let err = reader
        .restore(&copy, &fx.target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap_err();
    assert!(bytes_in_dir_limit(&err, limit), "{err:?}");
    let mut reader = fx.reader().await;
    reader.set_dir_byte_limits(limit, u64::MAX);
    reader
        .restore(&honest, &fx.target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap();

    let budget = TreeBudget::default().with_dir_byte_limits(limit, u64::MAX);
    let report = fx.check_budget(budget).await;
    let keys: Vec<&str> = report.findings.iter().map(|f| f.key.as_str()).collect();
    assert_eq!(keys, [snapshot_object_key(&copy)], "{:?}", report.findings);
    assert_eq!(report.findings[0].kind, FindingKind::UnreadableIndex);
}

/// The same for the sizes of chunks: the planted shard lists every chunk one
/// byte short and sorts after the real one, so the check's index keeps the
/// real sizes and the file adds up, while a restore of the copy takes the
/// planted sizes first, reads the chunks and charges their bytes.
#[tokio::test]
async fn check_hostile_snapshot_shard_lists_chunks_wrong_like_restore() {
    let mut fx = Fx::new().await;
    let chunks = [fx.data(&[1u8; 1000]), fx.data(&[2u8; 500])];
    let top = vec![file("f", &chunks, 1500)];
    let tree_bytes = encode_tree(top.clone()).len() as u64;
    let honest = fx.snapshot(top).await;
    let store = LocalFsStore::new(fx.repo_dir.clone());
    let repo = Repository::open(&store, b"pw").await.unwrap();
    let keys = attacker_keys(&repo);
    let shards = open_all_shards(&store, &keys).await;
    assert_eq!(shards.len(), 1);
    let (real_id, real) = &shards[0];
    let forged = real.packs.iter().map(shrink_ulen).collect();
    let planted = plant_shard_where(&store, &keys, forged, Vec::new(), |id| id > real_id).await;
    let copy = reseal(&fx, &honest, &[planted]).await;

    let limits = (MAX_WALK_NODES, tree_bytes + 1499);
    let mut reader = fx.reader().await;
    reader.set_walk_limits(limits.0, limits.1);
    let err = reader
        .restore(&copy, &fx.target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap_err();
    assert!(is_walk_limit(&err, "bytes of directory data"), "{err:?}");
    let mut reader = fx.reader().await;
    reader.set_walk_limits(limits.0, limits.1);
    reader
        .restore(&honest, &fx.target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap();

    let report = fx.check_budget(TreeBudget::new(limits.0, limits.1)).await;
    let keys: Vec<&str> = report.findings.iter().map(|f| f.key.as_str()).collect();
    assert_eq!(keys, [snapshot_object_key(&copy)], "{:?}", report.findings);
    assert_eq!(report.findings[0].kind, FindingKind::UnreadableIndex);
}

async fn root_over(fx: &mut Fx, inner: BlobId, i: usize) -> BlobId {
    let mut top = files(&format!("f{i}"), 1);
    top.push(dir("z", inner));
    fx.snapshot(top).await
}

/// Snapshots whose roots differ but reach the same directory the same way read
/// the directories on the way once, down to the leaf.
async fn cut_path_pack_reads(snapshots: usize) -> (usize, StructureReport, Vec<BlobId>) {
    let mut fx = Fx::new().await;
    let mut inner = fx.tree(files("d", 3));
    for _ in 0..10 {
        inner = fx.tree(vec![dir("p", inner)]);
    }
    let mut ids = vec![root_over(&mut fx, inner, 0).await];
    let store = LocalFsStore::new(fx.repo_dir.clone());
    let path_packs: Vec<String> = store
        .list("packs/")
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.key)
        .collect();
    assert!(!path_packs.is_empty());
    for i in 1..snapshots {
        ids.push(root_over(&mut fx, inner, i).await);
    }

    let ranged = RangedOnly::new(&store, &[]);
    let repo = Repository::open(&store, b"pw").await.unwrap();
    let report = check_structure_with_budget(
        &ranged,
        repo.read_keys(),
        repo.config(),
        &|_, _, _| {},
        TreeBudget::default().with_dir_limits(u64::MAX, 10),
    )
    .await
    .unwrap();
    let prefix = 0..PACK_PREFIX_LEN as u64;
    let reads = path_packs
        .iter()
        .flat_map(|key| ranged.ranges_of(key))
        .filter(|r| *r != prefix)
        .count();
    (reads, report, ids)
}

#[tokio::test]
async fn check_hostile_shared_cut_path_is_walked_once() {
    let mut reads = Vec::new();
    for n in [2usize, 8] {
        let (count, report, ids) = cut_path_pack_reads(n).await;
        assert_eq!(report.snapshots_checked, n as u64);
        for id in &ids {
            let key = snapshot_object_key(id);
            let mine: Vec<_> = report.findings.iter().filter(|f| f.key == key).collect();
            assert_eq!(mine.len(), 1, "{:?}", report.findings);
            assert!(mine[0].detail.contains("entries held at once"), "{mine:?}");
        }
        assert_eq!(report.trees_checked, n as u64 + 11);
        reads.push(count);
    }
    assert!(reads[0] > 0, "the path was never read");
    assert_eq!(reads[0], reads[1], "the shared path was read again");
}

/// A diamond of `levels` steps over a chain of 60 directories, with
/// incomparable paths: at each step `a` is heavy in bytes and `b` in entries,
/// and both lead on to the next step. Three snapshots reach the top, with
/// roots that are heavier in entries, in bytes, and light. The limit is one
/// byte below the lightest path, so every path passes it.
struct Diamond {
    fx: Fx,
    snapshots: Vec<BlobId>,
    built: u64,
    held: u64,
}

async fn incomparable_diamond(levels: usize) -> Diamond {
    let mut fx = Fx::new().await;
    let size = |nodes: &[pb::Node]| encode_tree(nodes.to_vec()).len() as u64;
    let mut built = 0u64;
    let mut lightest = 0u64;
    let leaf = files("l", 1);
    lightest += size(&leaf);
    let mut below = fx.tree(leaf);
    built += 1;
    for _ in 0..60 {
        let nodes = vec![file("c", &[], 0), dir("p", below)];
        lightest += size(&nodes);
        below = fx.tree(nodes);
        built += 1;
    }
    for j in (0..levels).rev() {
        let heavy: Vec<pb::Node> = (0..1usize << j)
            .map(|i| file(&format!("{i:0>200}"), &[], 0))
            .chain([dir("p", below)])
            .collect();
        let many: Vec<pb::Node> = (0..4usize << j)
            .map(|i| file(&format!("{i:08}"), &[], 0))
            .chain([dir("p", below)])
            .collect();
        lightest += size(&many);
        let top = vec![dir("a", fx.tree(heavy)), dir("b", fx.tree(many))];
        lightest += size(&top);
        below = fx.tree(top);
        built += 3;
    }
    let by_entries: Vec<pb::Node> = (0..1000usize)
        .map(|i| file(&format!("{i:08}"), &[], 0))
        .chain([dir("z", below)])
        .collect();
    let by_bytes: Vec<pb::Node> = (0..20usize)
        .map(|i| file(&format!("{i:0>200}"), &[], 0))
        .chain([dir("z", below)])
        .collect();
    let light = vec![dir("z", below)];
    lightest += size(&light);
    let mut snapshots = Vec::new();
    for root in [by_entries, by_bytes, light] {
        snapshots.push(fx.snapshot(root).await);
        built += 1;
    }
    Diamond {
        fx,
        snapshots,
        built,
        held: lightest - 1,
    }
}

/// A directory is read once however many incomparable paths lead to it, so
/// the reads are the trees checked and do not grow with the number of paths.
#[tokio::test]
async fn check_hostile_incomparable_paths_read_each_tree_once() {
    for levels in [4usize, 10] {
        let d = incomparable_diamond(levels).await;
        let store = RangedOnly::new(LocalFsStore::new(d.fx.repo_dir.clone()), &[]);
        let repo = Repository::open(&store, b"pw").await.unwrap();
        let report = check_structure_with_budget(
            &store,
            repo.read_keys(),
            repo.config(),
            &|_, _, _| {},
            TreeBudget::default().with_dir_byte_limits(u64::MAX, d.held),
        )
        .await
        .unwrap();
        let prefix = 0..PACK_PREFIX_LEN as u64;
        let reads = store
            .ranges_under("packs/")
            .into_iter()
            .filter(|(_, r)| *r != prefix)
            .count() as u64;
        assert_eq!(reads, report.trees_checked, "levels={levels}");
        assert!(report.trees_checked <= d.built, "levels={levels}");
        for snap in &d.snapshots {
            let key = snapshot_object_key(snap);
            let mine: Vec<_> = report.findings.iter().filter(|f| f.key == key).collect();
            assert_eq!(mine.len(), 1, "levels={levels}: {:?}", report.findings);
            assert_eq!(mine[0].kind, FindingKind::UnreadableTree);
            assert!(mine[0].detail.contains("bytes held at once"), "{mine:?}");
        }
        let mut reader = d.fx.reader().await;
        reader.set_dir_byte_limits(u64::MAX, d.held);
        let err = reader
            .restore(
                &d.snapshots[0],
                &d.fx.target,
                &RestoreOptions::default(),
                &|_| {},
            )
            .await
            .unwrap_err();
        assert!(held_bytes_limit(&err, d.held), "levels={levels}: {err:?}");
    }
}

/// `R` reaches `H`, whose subtree passes the limit of a path on its own, and
/// then `X`, which has a finding. `R` gets the limit and nothing from `X`; `S`
/// reaches `X` alone and gets its finding.
#[tokio::test]
async fn check_hostile_directory_over_path_limit_alone_skips_the_rest() {
    let mut fx = Fx::new().await;
    let bottom = fx.tree(files("f", 5));
    let mut middle = files("f", 5);
    middle.push(dir("z", bottom));
    let middle = fx.tree(middle);
    let mut h = files("f", 5);
    h.push(dir("z", middle));
    let h = fx.tree(h);
    let x = fx.tree(vec![file("x", &[[9u8; 32]], 1)]);
    let r = fx
        .snapshot(vec![dir("a", h), dir("b", x), dir("c", h)])
        .await;
    let s = fx.snapshot(vec![dir("x", x)]).await;

    let report = fx.check_limited(10, 10).await;
    assert_eq!(report.snapshots_checked, 2);
    assert_eq!(
        finding_kinds(&report, &r),
        [FindingKind::UnreadableTree],
        "{:?}",
        report.findings
    );
    let key = snapshot_object_key(&r);
    let f = report.findings.iter().find(|f| f.key == key).unwrap();
    assert!(f.detail.contains("entries held at once"), "{f:?}");
    assert!(
        finding_kinds(&report, &s).contains(&FindingKind::UnknownBlob),
        "{:?}",
        report.findings
    );

    let mut reader = fx.reader().await;
    reader.set_dir_limits(10, 10);
    let err = reader
        .restore(&r, &fx.target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::WalkLimit {
                what: "entries held at once",
                ..
            }
        ),
        "{err:?}"
    );
    fx.restore(&s, &RestoreOptions::default())
        .await
        .unwrap_err();
}

/// The same with nesting: `R` reaches a chain deeper than the limit, then `X`.
#[tokio::test]
async fn check_hostile_directory_too_deep_alone_skips_the_rest() {
    let mut fx = Fx::new().await;
    let mut deep = fx.tree(files("l", 1));
    for _ in 0..=MAX_DEPTH {
        deep = fx.tree(vec![dir("d", deep)]);
    }
    let x = fx.tree(vec![file("x", &[[9u8; 32]], 1)]);
    let r = fx
        .snapshot(vec![dir("a", deep), dir("b", x), dir("c", deep)])
        .await;
    let s = fx.snapshot(vec![dir("x", x)]).await;

    let report = fx.check().await;
    assert_eq!(report.snapshots_checked, 2);
    let key = snapshot_object_key(&r);
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.key == key && f.detail.contains("nested too deeply")),
        "{:?}",
        report.findings
    );
    assert!(
        !finding_kinds(&report, &r).contains(&FindingKind::UnknownBlob),
        "{:?}",
        report.findings
    );
    assert!(
        finding_kinds(&report, &s).contains(&FindingKind::UnknownBlob),
        "{:?}",
        report.findings
    );

    let err = fx
        .restore(&r, &RestoreOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Corrupt { reason, .. } if reason.contains("nested")),
        "{err:?}"
    );
    fx.restore(&s, &RestoreOptions::default())
        .await
        .unwrap_err();
}

/// `D -> E -> F` with an unknown chunk in `F`. One snapshot reaches `D` at
/// `MAX_DEPTH`, so its restore stops before `F`; the other reaches it near the
/// root and restores as far as the chunk.
async fn depth_cut_pair(deep_first: bool) -> (Fx, BlobId, BlobId) {
    let mut fx = Fx::new().await;
    let f = fx.tree(vec![file("x", &[[9u8; 32]], 1)]);
    let e = fx.tree(vec![dir("f", f)]);
    let d = fx.tree(vec![dir("e", e)]);
    let mut chain = d;
    for _ in 0..MAX_DEPTH - 2 {
        chain = fx.tree(vec![dir("d", chain)]);
    }
    for _ in 0..64 {
        let deep = fx.snapshot(vec![dir("d", chain)]).await;
        let shallow = fx.snapshot(vec![dir("d", d)]).await;
        if (snapshot_object_key(&deep) < snapshot_object_key(&shallow)) == deep_first {
            return (fx, deep, shallow);
        }
        for id in [&deep, &shallow] {
            fs::remove_file(fx.repo_dir.join(snapshot_object_key(id))).unwrap();
        }
    }
    panic!("no pair in the wanted order");
}

async fn assert_depth_is_found_only_for_deep(deep_first: bool) {
    let (fx, deep, shallow) = depth_cut_pair(deep_first).await;
    let report = fx.check().await;
    assert_eq!(report.snapshots_checked, 2);
    let nested = |snap: &BlobId| {
        let key = snapshot_object_key(snap);
        report
            .findings
            .iter()
            .any(|f| f.key == key && f.detail.contains("nested too deeply"))
    };
    assert!(nested(&deep), "{:?}", report.findings);
    assert!(!nested(&shallow), "{:?}", report.findings);
    assert!(
        finding_kinds(&report, &shallow).contains(&FindingKind::UnknownBlob),
        "{:?}",
        report.findings
    );

    let err = fx
        .restore(&deep, &RestoreOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Corrupt { reason, .. } if reason.contains("nested")),
        "{err:?}"
    );
    let err = fx
        .restore(&shallow, &RestoreOptions::default())
        .await
        .unwrap_err();
    assert!(
        !matches!(&err, Error::Corrupt { reason, .. } if reason.contains("nested")),
        "{err:?}"
    );
}

#[tokio::test]
async fn check_hostile_depth_cut_shared_dir_deep_first() {
    assert_depth_is_found_only_for_deep(true).await;
}

#[tokio::test]
async fn check_hostile_depth_cut_shared_dir_shallow_first() {
    assert_depth_is_found_only_for_deep(false).await;
}

#[tokio::test]
async fn check_hostile_short_subtree_id_is_a_finding() {
    let mut fx = Fx::new().await;
    let mut bad = dir("z", [0u8; 32]);
    bad.subtrees = vec![vec![7u8; 31]];
    let snap = fx.snapshot(vec![bad]).await;

    fx.assert_refused(&snap).await;
    let report = fx.check().await;
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    let f = &report.findings[0];
    assert_eq!(f.key, snapshot_object_key(&snap));
    assert_eq!(f.kind, FindingKind::UnreadableTree);
    assert!(f.detail.contains("32 bytes"), "{f:?}");
}

#[tokio::test]
async fn check_hostile_fan_out_over_walk_budget_matches_restore() {
    let mut fx = Fx::new().await;
    let snap = fan_out(&mut fx).await;

    let report = fx.check().await;
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert_eq!(report.trees_checked, 5);

    let mut reader = fx.reader().await;
    reader.set_walk_limits(10_000, u64::MAX);
    let nodes = reader
        .restore(&snap, &fx.target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap_err();
    let mut reader = fx.reader().await;
    reader.set_walk_limits(u64::MAX, 4096);
    let bytes = reader
        .restore(&snap, &fx.target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap_err();
    let budgets = [
        (nodes, TreeBudget::new(10_000, MAX_WALK_TREE_BYTES)),
        (bytes, TreeBudget::new(MAX_WALK_NODES, 4096)),
    ];
    for (err, budget) in budgets {
        let report = fx.check_budget(budget).await;
        assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
        let f = &report.findings[0];
        assert_eq!(f.key, snapshot_object_key(&snap));
        assert_eq!(f.kind, FindingKind::UnreadableTree);
        assert_eq!(f.detail, err.to_string());
    }
}

/// Forty levels of two directory nodes that name the same directory below:
/// 2^40 visits of the file at the bottom, from 42 trees.
#[tokio::test]
async fn check_hostile_huge_diamond_is_not_expanded() {
    let mut fx = Fx::new().await;
    let content = fx.data(b"fine");
    let mut inner = fx.tree(vec![file("f", &[content], 4)]);
    for _ in 0..40 {
        inner = fx.tree(vec![dir("a", inner), dir("b", inner)]);
    }
    let snap = fx.snapshot(vec![dir("a", inner)]).await;

    let report = fx.check().await;
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    let f = &report.findings[0];
    assert_eq!(f.key, snapshot_object_key(&snap));
    assert_eq!(f.kind, FindingKind::UnreadableTree);
    let limit = Error::WalkLimit {
        what: "entries",
        limit: MAX_WALK_NODES,
    };
    assert_eq!(f.detail, limit.to_string());
}
