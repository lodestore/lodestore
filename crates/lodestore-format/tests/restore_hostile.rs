//! A restore must refuse snapshots a writer-keys holder could forge, and
//! survive a hostile target, without changing anything it was not meant to.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};

use lodestore_format::check::FindingKind;
use lodestore_format::check::structure::{
    StructureReport, check_structure, check_structure_with_dir_limits,
};
use lodestore_format::crypto::{BLOB_TYPE_DATA, BLOB_TYPE_TREE, BlobId, Entropy};
use lodestore_format::error::Error;
use lodestore_format::keys::KdfCost;
use lodestore_format::pack::{CODEC_RAW, CODEC_ZSTD};
use lodestore_format::pb;
use lodestore_format::reader::{Overwrite, Reader, RestoreOptions, RestoreReport};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::snapshot::snapshot_object_key;
use lodestore_format::store::LocalFsStore;
use lodestore_format::tree::{
    MAX_DIR_ENTRIES, MAX_HELD_ENTRIES, MAX_WALK_NODES, MAX_WALK_TREE_BYTES, encode_tree,
};
use lodestore_format::writer::Writer;

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
