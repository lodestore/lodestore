use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use lodestore_format::crypto::Entropy;
use lodestore_format::error::Error;
use lodestore_format::index::hex32;
use lodestore_format::keys::KdfCost;
use lodestore_format::pb;
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::snapshot::snapshot_object_key;
use lodestore_format::store::{BlobStore, LocalFsStore};
use lodestore_format::tree::encode_tree;
use lodestore_format::writer::{BackupOptions, Writer};
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{Rng, SeedableRng};

const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

async fn init_repo(dir: &Path) -> LocalFsStore {
    let store = LocalFsStore::new(dir.to_path_buf());
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
    store
}

async fn writer(store: &LocalFsStore) -> Writer<&LocalFsStore> {
    let repo = Repository::open(store, b"pw").await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    Writer::new(store, keys, repo.config()).await.unwrap()
}

async fn reader(store: &LocalFsStore) -> Reader<&LocalFsStore> {
    let repo = Repository::open(store, b"pw").await.unwrap();
    Reader::open(store, repo.read_keys().duplicate(), repo.config()).unwrap()
}

fn set_mtime(path: &Path, secs: u64, nanos: u32) {
    let t = UNIX_EPOCH + Duration::new(secs, nanos);
    fs::File::open(path).unwrap().set_modified(t).unwrap();
}

fn write_file(path: &Path, data: &[u8], mode: u32, mtime: u64) {
    fs::write(path, data).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::new(mtime, 123_456_789))
        .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn make_tree(root: &Path) {
    let mut rng = ChaCha20Rng::from_seed([7; 32]);
    let mut random = vec![0u8; 3 << 20];
    rng.fill_bytes(&mut random);
    let repeated: Vec<u8> = b"lodestore "
        .iter()
        .copied()
        .cycle()
        .take(5 << 20)
        .collect();

    fs::create_dir_all(root.join("a/b/c")).unwrap();
    fs::create_dir(root.join("empty")).unwrap();
    write_file(&root.join("top.txt"), b"hello", 0o644, 1_600_000_000);
    write_file(
        &root.join("a/script.sh"),
        b"#!/bin/sh\n",
        0o751,
        1_600_000_100,
    );
    write_file(&root.join("a/b/random.bin"), &random, 0o600, 1_600_000_200);
    write_file(
        &root.join("a/b/c/repeat.bin"),
        &repeated,
        0o640,
        1_600_000_300,
    );
    write_file(
        &root.join("a/b/c/ro.txt"),
        b"read only",
        0o400,
        1_600_000_400,
    );
    #[cfg(target_os = "linux")]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let name = OsStr::from_bytes(b"not-utf8-\xff\xfe");
        write_file(
            &root.join("a").join(name),
            b"odd name",
            0o644,
            1_600_000_500,
        );
    }

    for (dir, t) in [
        ("a/b/c", 1_600_001_000),
        ("a/b", 1_600_001_100),
        ("a", 1_600_001_200),
        ("empty", 1_600_001_300),
        ("", 1_600_001_400),
    ] {
        set_mtime(&root.join(dir), t, 987_654_321);
    }
}

/// Where a backed-up path ends up below a restore target.
fn restored_path(target: &Path, src: &Path) -> PathBuf {
    let canonical = src.canonicalize().unwrap();
    target.join(canonical.strip_prefix("/").unwrap())
}

fn assert_same(a: &Path, b: &Path) {
    let (ma, mb) = (
        fs::symlink_metadata(a).unwrap(),
        fs::symlink_metadata(b).unwrap(),
    );
    assert_eq!(ma.file_type(), mb.file_type(), "{}", a.display());
    assert_eq!(
        ma.permissions().mode() & 0o7777,
        mb.permissions().mode() & 0o7777,
        "mode of {}",
        a.display()
    );
    assert_eq!(
        ma.modified().unwrap(),
        mb.modified().unwrap(),
        "mtime of {}",
        a.display()
    );
    if ma.is_dir() {
        let names = |p: &Path| {
            let mut v: Vec<OsString> = fs::read_dir(p)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            v.sort();
            v
        };
        let names_a = names(a);
        assert_eq!(names_a, names(b), "entries of {}", a.display());
        for n in names_a {
            assert_same(&a.join(&n), &b.join(&n));
        }
    } else {
        assert_eq!(
            fs::read(a).unwrap(),
            fs::read(b).unwrap(),
            "{}",
            a.display()
        );
    }
}

async fn objects(store: &LocalFsStore, prefix: &str) -> Vec<String> {
    let mut keys: Vec<String> = store
        .list(prefix)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.key)
        .collect();
    keys.sort();
    keys
}

fn read_all(dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            read_all(&path, out);
        } else {
            out.insert(path.clone(), fs::read(&path).unwrap());
        }
    }
}

#[tokio::test]
async fn roundtrip_local_library() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src, target) = (
        tmp.path().join("repo"),
        tmp.path().join("src"),
        tmp.path().join("out"),
    );
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir(&src).unwrap();
    make_tree(&src);

    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;
    let seen = std::cell::Cell::new(0u64);
    let outcome = w
        .backup(
            std::slice::from_ref(&src),
            BackupOptions {
                tags: vec!["t".into()],
                ..Default::default()
            },
            &|p| seen.set(p.files_done),
        )
        .await
        .unwrap();
    assert!(!outcome.incomplete);
    assert!(outcome.stats.files_new >= 5);
    assert_eq!(seen.get(), outcome.stats.files_new);

    let mut r = reader(&store).await;
    let listed = r.list_snapshots().await.unwrap().entries;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, outcome.snapshot_id);
    assert_eq!(listed[0].snapshot.tags, vec!["t".to_string()]);
    assert_eq!(
        listed[0].snapshot.paths,
        vec![
            src.canonicalize()
                .unwrap()
                .as_os_str()
                .as_encoded_bytes()
                .to_vec()
        ]
    );

    let report = r
        .restore(
            &outcome.snapshot_id,
            &target,
            &RestoreOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    // Directories above the source exist only to hold the path; as a non-root
    // user their (root) owner cannot be applied, and that is reported.
    assert!(
        report.unapplied.iter().all(|u| u.what == "owner"),
        "{:?}",
        report.unapplied
    );
    assert_eq!(report.files, outcome.stats.files_new);
    assert_same(&src, &restored_path(&target, &src));

    let by_prefix = r
        .resolve_snapshot(&lodestore_format::index::hex32(&outcome.snapshot_id)[..8])
        .await
        .unwrap();
    assert_eq!(by_prefix.id, outcome.snapshot_id);
}

#[tokio::test]
async fn roundtrip_rebackup_no_new_packs() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src, target) = (
        tmp.path().join("repo"),
        tmp.path().join("src"),
        tmp.path().join("out"),
    );
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir(&src).unwrap();
    make_tree(&src);
    let store = init_repo(&repo_dir).await;

    let first = writer(&store)
        .await
        .backup(
            std::slice::from_ref(&src),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    let packs = objects(&store, "packs/").await;
    let shards = objects(&store, "index/").await;
    assert!(!packs.is_empty());
    let mut before = BTreeMap::new();
    read_all(&repo_dir, &mut before);

    // A fresh writer discovers the index by LIST and finds everything.
    let second = writer(&store)
        .await
        .backup(
            std::slice::from_ref(&src),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert_ne!(first.snapshot_id, second.snapshot_id);
    assert_eq!(objects(&store, "packs/").await, packs);
    assert_eq!(objects(&store, "index/").await, shards);
    assert_eq!(second.stats.bytes_added, 0);

    let mut r = reader(&store).await;
    r.restore(
        &second.snapshot_id,
        &target,
        &RestoreOptions::default(),
        &|_| {},
    )
    .await
    .unwrap();
    assert_same(&src, &restored_path(&target, &src));

    // Nothing that existed before was changed or removed.
    let mut after = BTreeMap::new();
    read_all(&repo_dir, &mut after);
    for (path, bytes) in &before {
        assert_eq!(after.get(path), Some(bytes), "{}", path.display());
    }
}

#[tokio::test]
async fn roundtrip_empty_inputs() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src, target) = (
        tmp.path().join("repo"),
        tmp.path().join("src"),
        tmp.path().join("out"),
    );
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir_all(src.join("sub")).unwrap();
    write_file(&src.join("zero"), b"", 0o644, 1_600_000_000);
    set_mtime(&src.join("sub"), 1_600_000_100, 5);
    set_mtime(&src, 1_600_000_200, 6);
    let lone = tmp.path().join("lone");
    write_file(&lone, b"", 0o600, 1_600_000_300);

    let store = init_repo(&repo_dir).await;
    let mut r = reader(&store).await;
    assert!(r.list_snapshots().await.unwrap().entries.is_empty());

    let outcome = writer(&store)
        .await
        .backup(
            &[src.clone(), lone.clone()],
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    r.restore(
        &outcome.snapshot_id,
        &target,
        &RestoreOptions::default(),
        &|_| {},
    )
    .await
    .unwrap();
    assert_same(&src, &restored_path(&target, &src));
    assert_same(&lone, &restored_path(&target, &lone));
    assert_eq!(
        fs::metadata(restored_path(&target, &lone)).unwrap().len(),
        0
    );
}

#[tokio::test]
async fn roundtrip_missing_root_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir(&src).unwrap();
    write_file(&src.join("f"), b"data", 0o644, 1_600_000_000);
    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;

    for paths in [
        vec![tmp.path().join("does-not-exist")],
        vec![src.clone(), tmp.path().join("does-not-exist")],
    ] {
        let err = w
            .backup(&paths, BackupOptions::default(), &|_| {})
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)), "{err:?}");
    }
    for prefix in ["packs/", "index/", "snapshots/"] {
        assert!(objects(&store, prefix).await.is_empty(), "{prefix}");
    }
}

#[tokio::test]
async fn roundtrip_snapshot_order() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir(&src).unwrap();
    write_file(&src.join("f"), b"data", 0o644, 1_600_000_000);
    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;

    let at = |seconds| BackupOptions {
        time: Some(pb::Timestamp { seconds, nanos: 0 }),
        ..Default::default()
    };
    let mut ids = Vec::new();
    for seconds in [500, 500, 500, 100] {
        let out = w
            .backup(std::slice::from_ref(&src), at(seconds), &|_| {})
            .await
            .unwrap();
        ids.push((seconds, out.snapshot_id));
    }

    let listed: Vec<_> = reader(&store)
        .await
        .list_snapshots()
        .await
        .unwrap()
        .entries
        .into_iter()
        .map(|e| (e.snapshot.time.unwrap().seconds, e.id))
        .collect();
    let mut expected = ids.clone();
    expected.sort();
    assert_eq!(listed, expected);
    assert_eq!(listed[0].0, 100);
}

#[tokio::test]
async fn roundtrip_renamed_object_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir(&src).unwrap();
    write_file(&src.join("f"), b"data", 0o644, 1_600_000_000);
    let store = init_repo(&repo_dir).await;
    let out = writer(&store)
        .await
        .backup(
            std::slice::from_ref(&src),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();

    let real = repo_dir.join(snapshot_object_key(&out.snapshot_id));
    let fake = repo_dir.join(snapshot_object_key(&[0xAB; 32]));
    fs::copy(&real, &fake).unwrap();

    let list = reader(&store).await.list_snapshots().await.unwrap();
    assert_eq!(list.entries.len(), 1);
    assert_eq!(list.entries[0].id, out.snapshot_id);
    assert_eq!(list.skipped.len(), 1);
    assert_eq!(list.skipped[0].0, snapshot_object_key(&[0xAB; 32]));
    assert!(list.skipped[0].1.contains("does not match"));

    let by_id = hex32(&out.snapshot_id);
    let found = reader(&store).await.resolve_snapshot(&by_id).await.unwrap();
    assert_eq!(found.id, out.snapshot_id);
}

#[tokio::test]
async fn roundtrip_renamed_index_shard_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir(&src).unwrap();
    write_file(&src.join("f"), b"data", 0o644, 1_600_000_000);
    let store = init_repo(&repo_dir).await;
    writer(&store)
        .await
        .backup(
            std::slice::from_ref(&src),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();

    let shards = objects(&store, "index/").await;
    let packs = objects(&store, "packs/").await;
    let fake = format!("index/{}", hex32(&[0xCD; 32]));
    fs::copy(repo_dir.join(&shards[0]), repo_dir.join(&fake)).unwrap();

    let mut w = writer(&store).await;
    assert_eq!(w.skipped_index_shards().len(), 1);
    assert_eq!(w.skipped_index_shards()[0].0, fake);
    let out = w
        .backup(
            std::slice::from_ref(&src),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert!(!out.incomplete);
    assert_eq!(objects(&store, "packs/").await, packs);
}

#[tokio::test]
async fn roundtrip_data_and_tree_with_equal_plaintext() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src, target) = (
        tmp.path().join("repo"),
        tmp.path().join("src"),
        tmp.path().join("out"),
    );
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir(&src).unwrap();
    let tree_bytes = encode_tree(vec![pb::Node {
        name: b"x".to_vec(),
        ..Default::default()
    }]);
    write_file(&src.join("f"), &tree_bytes, 0o644, 1_600_000_000);

    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;
    let tree_id = w.put_tree_raw(&tree_bytes).unwrap();
    w.commit_snapshot_raw(tree_id, Vec::new()).await.unwrap();

    let out = writer(&store)
        .await
        .backup(
            std::slice::from_ref(&src),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert!(!out.incomplete);

    let report = reader(&store)
        .await
        .restore(
            &out.snapshot_id,
            &target,
            &RestoreOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(
        fs::read(restored_path(&target, &src).join("f")).unwrap(),
        tree_bytes
    );
}

#[tokio::test]
async fn roundtrip_missing_pack_fails_one_file() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src, target) = (
        tmp.path().join("repo"),
        tmp.path().join("src"),
        tmp.path().join("out"),
    );
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir(&src).unwrap();
    let mut rng = ChaCha20Rng::from_seed([3; 32]);
    for name in ["a", "b"] {
        let mut data = vec![0u8; 100 << 10];
        rng.fill_bytes(&mut data);
        write_file(&src.join(name), &data, 0o644, 1_600_000_000);
    }
    let store = init_repo(&repo_dir).await;
    let out = writer(&store)
        .await
        .backup(
            std::slice::from_ref(&src),
            BackupOptions {
                pack_target_size: 1,
                ..Default::default()
            },
            &|_| {},
        )
        .await
        .unwrap();

    let mut files = BTreeMap::new();
    read_all(&repo_dir.join("packs"), &mut files);
    let (largest, _) = files.iter().max_by_key(|(_, b)| b.len()).unwrap();
    fs::remove_file(largest).unwrap();

    let mut r = reader(&store).await;
    let report = r
        .restore(
            &out.snapshot_id,
            &target,
            &RestoreOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(report.errors[0].error.contains("pack is missing"));
    assert_eq!(report.files, 1);
    let mut restored = BTreeMap::new();
    read_all(&target, &mut restored);
    assert_eq!(restored.len(), 1);
}

#[tokio::test]
async fn roundtrip_writer_reuploads_blobs_of_missing_packs() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src, target) = (
        tmp.path().join("repo"),
        tmp.path().join("src"),
        tmp.path().join("out"),
    );
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir(&src).unwrap();
    make_tree(&src);
    let store = init_repo(&repo_dir).await;
    writer(&store)
        .await
        .backup(
            std::slice::from_ref(&src),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();

    let packs = objects(&store, "packs/").await;
    assert!(!packs.is_empty());
    for pack in &packs {
        fs::remove_file(repo_dir.join(pack)).unwrap();
    }

    let mut w = writer(&store).await;
    assert_eq!(w.drop_unbacked_packs().await.unwrap(), packs.len());
    let out = w
        .backup(
            std::slice::from_ref(&src),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert!(!objects(&store, "packs/").await.is_empty());

    let report = reader(&store)
        .await
        .restore(
            &out.snapshot_id,
            &target,
            &RestoreOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_same(&src, &restored_path(&target, &src));
}

#[tokio::test]
async fn roundtrip_tampered_pack_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src, target) = (
        tmp.path().join("repo"),
        tmp.path().join("src"),
        tmp.path().join("out"),
    );
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir(&src).unwrap();
    make_tree(&src);
    let store = init_repo(&repo_dir).await;
    let out = writer(&store)
        .await
        .backup(
            std::slice::from_ref(&src),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();

    let mut files = BTreeMap::new();
    read_all(&repo_dir.join("packs"), &mut files);
    let (largest, bytes) = files.iter().max_by_key(|(_, b)| b.len()).unwrap();
    let mut bytes = bytes.clone();
    bytes[2000] ^= 0x01;
    fs::write(largest, bytes).unwrap();

    // The damaged file is reported and left out; the others are restored.
    let report = reader(&store)
        .await
        .restore(
            &out.snapshot_id,
            &target,
            &RestoreOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(report.errors[0].error.contains("authentication failed"));
    let damaged = PathBuf::from(String::from_utf8(report.errors[0].path.clone()).unwrap());
    assert!(!target.join(&damaged).exists());
    let mut restored = BTreeMap::new();
    read_all(&target, &mut restored);
    assert!(!restored.is_empty());
    assert!(restored.keys().all(|p| {
        !p.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".lode-restore-")
    }));
}

use std::collections::BTreeSet;
use std::os::unix::fs::{MetadataExt, symlink};

use lodestore_format::snapshot::ParentTree;
use proptest::prelude::{Just, ProptestConfig, Strategy, any, prop, prop_oneof, proptest};

#[derive(Debug, Clone)]
enum GenKind {
    File { len: usize, seed: u64 },
    Sparse { len: usize, at: usize },
    Dir,
    Symlink { target: u8 },
    Hardlink { of: usize },
}

#[derive(Debug, Clone)]
struct GenEntry {
    parent: usize,
    kind: GenKind,
    mode: u8,
    mtime: (u64, u32),
    xattr: Option<(u8, Vec<u8>)>,
}

#[derive(Debug, Clone)]
struct GenEdit {
    pick: usize,
    op: u8,
    seed: u64,
    len: usize,
}

const GEN_FILE_MODES: [u32; 6] = [0o644, 0o600, 0o640, 0o755, 0o400, 0o444];
const GEN_DIR_MODES: [u32; 4] = [0o755, 0o700, 0o750, 0o775];
const GEN_LINK_TARGETS: [&str; 5] = ["/etc/hosts", "e0", "no/such/place", "../up", "."];

fn gen_entry() -> impl Strategy<Value = GenEntry> {
    let len = prop_oneof![
        1 => Just(0usize),
        4 => 1..300usize,
        3 => 300..8192usize,
        1 => 8192..262_145usize,
    ];
    let kind = prop_oneof![
        5 => (len, any::<u64>()).prop_map(|(len, seed)| GenKind::File { len, seed }),
        1 => (65_536..262_144usize, any::<usize>())
            .prop_map(|(len, at)| GenKind::Sparse { len, at: at % (len - 100) }),
        3 => Just(GenKind::Dir),
        2 => (0..5u8).prop_map(|target| GenKind::Symlink { target }),
        2 => any::<usize>().prop_map(|of| GenKind::Hardlink { of }),
    ];
    (
        any::<usize>(),
        kind,
        any::<u8>(),
        (1_500_000_000..1_600_000_000u64, 0..1_000_000_000u32),
        prop::option::of((0..3u8, prop::collection::vec(any::<u8>(), 0..64))),
    )
        .prop_map(|(parent, kind, mode, mtime, xattr)| GenEntry {
            parent,
            kind,
            mode,
            mtime,
            xattr,
        })
}

fn gen_edit() -> impl Strategy<Value = GenEdit> {
    (any::<usize>(), 0..3u8, any::<u64>(), 0..4096usize).prop_map(|(pick, op, seed, len)| GenEdit {
        pick,
        op,
        seed,
        len,
    })
}

fn gen_content(seed: u64, len: usize) -> Vec<u8> {
    if seed % 2 == 1 {
        return vec![(seed >> 8) as u8; len];
    }
    let mut key = [0u8; 32];
    key[..8].copy_from_slice(&seed.to_le_bytes());
    let mut out = vec![0u8; len];
    ChaCha20Rng::from_seed(key).fill_bytes(&mut out);
    out
}

#[cfg(target_os = "linux")]
fn gen_xattr_name(index: u8) -> String {
    format!("user.lode.k{index}")
}

#[cfg(target_os = "linux")]
fn gen_set_xattr(path: &Path, index: u8, value: &[u8]) {
    let _ = rustix::fs::lsetxattr(
        path,
        gen_xattr_name(index),
        value,
        rustix::fs::XattrFlags::empty(),
    );
}

#[cfg(not(target_os = "linux"))]
fn gen_set_xattr(_path: &Path, _index: u8, _value: &[u8]) {}

#[cfg(target_os = "linux")]
fn gen_xattrs(path: &Path) -> Vec<Option<Vec<u8>>> {
    (0..3u8)
        .map(|index| {
            let mut buf = vec![0u8; 4096];
            rustix::fs::lgetxattr(path, gen_xattr_name(index), &mut buf[..])
                .ok()
                .map(|n| buf[..n].to_vec())
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn gen_xattrs(_path: &Path) -> Vec<Option<Vec<u8>>> {
    Vec::new()
}

fn gen_write_sparse(path: &Path, len: usize, at: usize) {
    let f = fs::File::create(path).unwrap();
    f.set_len(len as u64).unwrap();
    std::os::unix::fs::FileExt::write_all_at(&f, &[0xA5; 100], at as u64).unwrap();
}

fn gen_build(root: &Path, entries: &[GenEntry]) {
    let mut dirs = vec![root.to_path_buf()];
    let mut files: Vec<PathBuf> = Vec::new();
    let mut stamps: Vec<(PathBuf, u64, u32)> = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let path = dirs[e.parent % dirs.len()].join(format!("e{i}"));
        let file_mode = GEN_FILE_MODES[e.mode as usize % GEN_FILE_MODES.len()];
        let mut own_inode = true;
        match &e.kind {
            GenKind::File { len, seed } => {
                fs::write(&path, gen_content(*seed, *len)).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(file_mode)).unwrap();
                files.push(path.clone());
            }
            GenKind::Sparse { len, at } => {
                gen_write_sparse(&path, *len, *at);
                fs::set_permissions(&path, fs::Permissions::from_mode(file_mode)).unwrap();
                files.push(path.clone());
            }
            GenKind::Dir => {
                fs::create_dir(&path).unwrap();
                let mode = GEN_DIR_MODES[e.mode as usize % GEN_DIR_MODES.len()];
                fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
                dirs.push(path.clone());
            }
            GenKind::Symlink { target } => {
                symlink(
                    GEN_LINK_TARGETS[*target as usize % GEN_LINK_TARGETS.len()],
                    &path,
                )
                .unwrap();
                continue;
            }
            GenKind::Hardlink { of } => {
                if files.is_empty() {
                    fs::write(&path, b"").unwrap();
                    files.push(path.clone());
                } else {
                    fs::hard_link(&files[of % files.len()], &path).unwrap();
                    own_inode = false;
                }
            }
        }
        if own_inode {
            if let Some((index, value)) = &e.xattr {
                gen_set_xattr(&path, *index, value);
            }
            stamps.push((path, e.mtime.0, e.mtime.1));
        }
    }
    for (path, secs, nanos) in stamps.iter().rev() {
        set_mtime(path, *secs, *nanos);
    }
    set_mtime(root, 1_550_000_000, 424_242_424);
}

fn gen_walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                stack.push(entry.path());
            }
            out.push(entry.path());
        }
    }
    out.sort();
    out
}

fn gen_apply_edits(root: &Path, edits: &[GenEdit]) {
    for (i, edit) in edits.iter().enumerate() {
        let all = gen_walk(root);
        let is_dir = |p: &PathBuf| fs::symlink_metadata(p).unwrap().is_dir();
        let is_file = |p: &PathBuf| fs::symlink_metadata(p).unwrap().is_file();
        match edit.op {
            0 => {
                let files: Vec<&PathBuf> = all.iter().filter(|p| is_file(p)).collect();
                if let Some(path) = files.get(edit.pick % files.len().max(1)) {
                    fs::remove_file(path).unwrap();
                    fs::write(path, gen_content(edit.seed, edit.len)).unwrap();
                    set_mtime(path, 1_600_000_000 + 1000 + i as u64, 5);
                }
            }
            1 => {
                if let Some(path) = all.get(edit.pick % all.len().max(1)) {
                    if is_dir(path) {
                        fs::remove_dir_all(path).unwrap();
                    } else {
                        fs::remove_file(path).unwrap();
                    }
                }
            }
            _ => {
                let mut dirs: Vec<PathBuf> = all.iter().filter(|p| is_dir(p)).cloned().collect();
                dirs.push(root.to_path_buf());
                let dir = &dirs[edit.pick % dirs.len()];
                let path = dir.join(format!("new{i}"));
                fs::write(&path, gen_content(edit.seed, edit.len)).unwrap();
                set_mtime(&path, 1_600_000_000 + 2000 + i as u64, 7);
            }
        }
    }
}

type GenTree = (Vec<String>, BTreeSet<BTreeSet<PathBuf>>);

fn gen_describe(root: &Path) -> GenTree {
    let mut lines = Vec::new();
    let mut inodes: BTreeMap<u64, BTreeSet<PathBuf>> = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let meta = fs::symlink_metadata(&path).unwrap();
        let rel = path.strip_prefix(root).unwrap().to_path_buf();
        let kind = if meta.is_symlink() {
            format!("symlink -> {:?}", fs::read_link(&path).unwrap())
        } else if meta.is_dir() {
            for entry in fs::read_dir(&path).unwrap() {
                stack.push(entry.unwrap().path());
            }
            format!("dir {:o}", meta.mode() & 0o7777)
        } else {
            let data = fs::read(&path).unwrap();
            if meta.nlink() > 1 {
                inodes.entry(meta.ino()).or_default().insert(rel.clone());
            }
            format!(
                "file {:o} len={} blake3={}",
                meta.mode() & 0o7777,
                data.len(),
                blake3::hash(&data).to_hex()
            )
        };
        let xattrs = if meta.is_symlink() {
            Vec::new()
        } else {
            gen_xattrs(&path)
        };
        lines.push(format!(
            "{rel:?} {kind} mtime={}.{:09} xattrs={xattrs:?}",
            meta.mtime(),
            meta.mtime_nsec()
        ));
    }
    lines.sort();
    (lines, inodes.into_values().collect())
}

async fn gen_round(entries: Vec<GenEntry>, edits: Vec<GenEdit>) {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir(&src).unwrap();
    gen_build(&src, &entries);
    let store = init_repo(&repo_dir).await;

    let first = writer(&store)
        .await
        .backup(
            std::slice::from_ref(&src),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert!(!first.incomplete);
    let expected = gen_describe(&src);
    gen_restore_and_compare(
        &store,
        &first.snapshot_id,
        &tmp.path().join("out1"),
        &src,
        expected,
    )
    .await;

    gen_apply_edits(&src, &edits);
    let parent = ParentTree::open(reader(&store).await, first.snapshot_id)
        .await
        .unwrap();
    let second = writer(&store)
        .await
        .backup(
            std::slice::from_ref(&src),
            BackupOptions {
                parent: Some(parent),
                ..Default::default()
            },
            &|_| {},
        )
        .await
        .unwrap();
    assert!(!second.incomplete);
    let expected = gen_describe(&src);
    gen_restore_and_compare(
        &store,
        &second.snapshot_id,
        &tmp.path().join("out2"),
        &src,
        expected,
    )
    .await;
}

async fn gen_restore_and_compare(
    store: &LocalFsStore,
    id: &[u8; 32],
    target: &Path,
    src: &Path,
    expected: GenTree,
) {
    let report = reader(store)
        .await
        .restore(id, target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap();
    assert!(
        report.unapplied.iter().all(|u| u.what == "owner"),
        "{:?}",
        report.unapplied
    );
    let restored = gen_describe(&restored_path(target, src));
    assert_eq!(restored.0, expected.0, "restored tree differs");
    assert_eq!(restored.1, expected.1, "hardlink groups differ");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn roundtrip_generated_tree_restores_identically(
        entries in prop::collection::vec(gen_entry(), 0..14),
        edits in prop::collection::vec(gen_edit(), 1..5),
    ) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(gen_round(entries, edits));
    }
}
