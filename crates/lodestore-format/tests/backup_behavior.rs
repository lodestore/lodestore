use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use lodestore_format::Error;
use lodestore_format::crypto::{BLOB_TYPE_TREE, BlobId, Entropy};
use lodestore_format::fs::filter::PathMatcher;
use lodestore_format::fs::walk::WalkOptions;
use lodestore_format::keys::KdfCost;
use lodestore_format::pb;
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::store::{BlobStore, LocalFsStore};
use lodestore_format::tree::decode_tree;
use lodestore_format::writer::{BackupOptions, BackupProgress, IssueEvent, Writer};
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

/// Where a backed-up path ends up below a restore target.
fn restored_path(target: &Path, src: &Path) -> PathBuf {
    let canonical = src.canonicalize().unwrap();
    target.join(canonical.strip_prefix("/").unwrap())
}

fn random_bytes(len: usize, seed: u8) -> Vec<u8> {
    let mut rng = ChaCha20Rng::from_seed([seed; 32]);
    let mut out = vec![0u8; len];
    rng.fill_bytes(&mut out);
    out
}

async fn objects(store: &LocalFsStore, prefix: &str) -> Vec<String> {
    store
        .list(prefix)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.key)
        .collect()
}

#[tokio::test]
async fn backup_changed_during_read_warns() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src, target) = (
        tmp.path().join("repo"),
        tmp.path().join("src"),
        tmp.path().join("out"),
    );
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir(&src).unwrap();
    fs::write(src.join("steady.txt"), b"does not change").unwrap();
    fs::write(src.join("victim.txt"), b"before").unwrap();
    let victim = src.canonicalize().unwrap().join("victim.txt");

    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;
    let touched = Cell::new(false);
    let outcome = w
        .backup(std::slice::from_ref(&src), BackupOptions::default(), &|p| {
            if p.starting && p.current == victim.as_os_str().as_bytes() && !touched.get() {
                touched.set(true);
                let mut f = fs::OpenOptions::new().append(true).open(&victim).unwrap();
                std::io::Write::write_all(&mut f, b" and after").unwrap();
            }
        })
        .await
        .unwrap();
    assert!(touched.get());
    assert!(!outcome.incomplete);
    assert_eq!(outcome.errors_total, 0);
    assert_eq!(outcome.warnings_total, 1);

    let mut r = reader(&store).await;
    let snapshot = r.list_snapshots().await.unwrap().entries.remove(0).snapshot;
    assert!(!snapshot.incomplete);
    let [warning] = snapshot.warnings.as_slice() else {
        panic!("expected one warning, got {:?}", snapshot.warnings);
    };
    assert_eq!(warning.kind, pb::IssueKind::ChangedDuringRead as i32);
    assert_eq!(warning.path, victim.as_os_str().as_bytes());

    r.restore(
        &outcome.snapshot_id,
        &target,
        &RestoreOptions::default(),
        &|_| {},
    )
    .await
    .unwrap();
    let restored = restored_path(&target, &src);
    assert_eq!(
        fs::read(restored.join("victim.txt")).unwrap(),
        b"before and after"
    );
}

#[tokio::test]
async fn backup_hardlink_changed_between_names_still_restores() {
    use std::os::unix::fs::MetadataExt;

    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src, target) = (
        tmp.path().join("repo"),
        tmp.path().join("src"),
        tmp.path().join("out"),
    );
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir(&src).unwrap();
    fs::write(src.join("a.txt"), b"before").unwrap();
    fs::hard_link(src.join("a.txt"), src.join("b.txt")).unwrap();
    let second = src.canonicalize().unwrap().join("b.txt");

    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;
    let touched = Cell::new(false);
    let outcome = w
        .backup(std::slice::from_ref(&src), BackupOptions::default(), &|p| {
            if p.starting && p.current == second.as_os_str().as_bytes() && !touched.get() {
                touched.set(true);
                let mut f = fs::OpenOptions::new().append(true).open(&second).unwrap();
                std::io::Write::write_all(&mut f, b" and after").unwrap();
            }
        })
        .await
        .unwrap();
    assert!(touched.get());
    assert!(!outcome.incomplete);
    assert_eq!(outcome.errors_total, 0);
    assert!(outcome.warnings_total >= 1);

    let mut r = reader(&store).await;
    r.restore(
        &outcome.snapshot_id,
        &target,
        &RestoreOptions::default(),
        &|_| {},
    )
    .await
    .unwrap();
    let restored = restored_path(&target, &src);
    assert_eq!(fs::read(restored.join("a.txt")).unwrap(), b"before");
    assert_eq!(
        fs::read(restored.join("b.txt")).unwrap(),
        b"before and after"
    );
    let (a, b) = (
        fs::metadata(restored.join("a.txt")).unwrap(),
        fs::metadata(restored.join("b.txt")).unwrap(),
    );
    assert_ne!(a.ino(), b.ino());
}

#[tokio::test]
async fn backup_index_flushed_during_run() {
    const FILES: u8 = 12;
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir(&src).unwrap();
    for i in 0..FILES {
        fs::write(src.join(format!("f{i:02}.bin")), random_bytes(1 << 20, i)).unwrap();
    }

    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;
    let cancel = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&cancel);
    let interrupted = w
        .backup(
            std::slice::from_ref(&src),
            BackupOptions {
                pack_target_size: 1 << 20,
                index_flush_packs: 2,
                cancel,
                ..Default::default()
            },
            &|p| {
                if !p.starting && p.bytes_added >= 5 << 20 {
                    stop.store(true, Ordering::Relaxed);
                }
            },
        )
        .await;
    assert!(matches!(interrupted, Err(Error::Interrupted)));
    assert!(objects(&store, "snapshots/").await.is_empty());
    assert!(
        objects(&store, "index/").await.len() >= 2,
        "a shard is written along the run, not only at its end"
    );
    let uploaded = objects(&store, "packs/").await.len();
    assert!(uploaded >= 5);

    let mut w = writer(&store).await;
    let outcome = w
        .backup(
            std::slice::from_ref(&src),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert!(!outcome.incomplete);
    assert!(
        outcome.stats.bytes_added < 8 << 20,
        "the packs of the interrupted run were found in the index: added {}",
        outcome.stats.bytes_added
    );
    assert_eq!(objects(&store, "snapshots/").await.len(), 1);
}

#[tokio::test]
async fn backup_refuses_directory_over_entry_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src, target) = (
        tmp.path().join("repo"),
        tmp.path().join("src"),
        tmp.path().join("out"),
    );
    fs::create_dir(&repo_dir).unwrap();
    let (ten, eleven) = (src.join("ten"), src.join("eleven"));
    fs::create_dir_all(&ten).unwrap();
    fs::create_dir_all(&eleven).unwrap();
    for i in 0..11 {
        fs::write(eleven.join(format!("f{i:02}")), b"x").unwrap();
    }
    for i in 0..10 {
        fs::write(ten.join(format!("f{i:02}")), b"x").unwrap();
    }
    let store = init_repo(&repo_dir).await;

    let mut w = writer(&store).await;
    w.set_dir_limit(10);
    let err = w
        .backup(
            std::slice::from_ref(&eleven),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("more than the 10"), "{err}");
    assert!(objects(&store, "snapshots/").await.is_empty());

    let mut w = writer(&store).await;
    w.set_dir_limit(10);
    w.backup(
        std::slice::from_ref(&ten),
        BackupOptions::default(),
        &|_| {},
    )
    .await
    .unwrap();
    let mut r = reader(&store).await;
    let id = r.list_snapshots().await.unwrap().entries.remove(0).id;
    r.restore(&id, &target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap();
    assert_eq!(
        fs::read(restored_path(&target, &ten).join("f09")).unwrap(),
        b"x"
    );
}

/// A restore holds every directory on the path it is in; a backup refuses a
/// tree whose deepest path holds more than that.
#[tokio::test]
async fn backup_refuses_tree_over_held_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, target) = (tmp.path().join("repo"), tmp.path().join("out"));
    fs::create_dir(&repo_dir).unwrap();
    let src = tmp.path().canonicalize().unwrap().join("src");
    let deepest = src.join("b").join("c");
    fs::create_dir_all(&deepest).unwrap();
    for (dir, n) in [(&src, 4), (&src.join("b"), 4), (&deepest, 5)] {
        for i in 0..n {
            fs::write(dir.join(format!("f{i}")), b"x").unwrap();
        }
    }
    // The root and every directory above `src` list one entry each.
    let held = src.components().count() as u64 - 1 + 15;
    let store = init_repo(&repo_dir).await;

    let mut w = writer(&store).await;
    w.set_held_limit(held - 1);
    let err = w
        .backup(
            std::slice::from_ref(&src),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)), "{err:?}");
    assert!(
        err.to_string()
            .contains(&format!("more than the {} a restore holds", held - 1)),
        "{err}"
    );
    assert!(objects(&store, "snapshots/").await.is_empty());

    let mut w = writer(&store).await;
    w.set_held_limit(held);
    w.backup(
        std::slice::from_ref(&src),
        BackupOptions::default(),
        &|_| {},
    )
    .await
    .unwrap();
    let mut r = reader(&store).await;
    let id = r.list_snapshots().await.unwrap().entries.remove(0).id;
    r.set_dir_limits(u64::MAX, held - 1);
    let err = r
        .restore(&id, &target, &RestoreOptions::default(), &|_| {})
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
    r.set_dir_limits(u64::MAX, held);
    r.restore(&id, &target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap();
    assert_eq!(
        fs::read(restored_path(&target, &deepest).join("f4")).unwrap(),
        b"x"
    );
}

/// The smallest byte limit a backup of `src` passes: each refusal names the
/// size that tripped it, which is the next limit to try. The probe runs in a
/// repository of its own, so the passing run leaves no snapshot behind.
async fn byte_bound(src: &PathBuf, held: bool) -> u64 {
    let probe = tempfile::tempdir().unwrap();
    let store = init_repo(probe.path()).await;
    let store = &store;
    let mut limit = 1;
    for _ in 0..64 {
        let mut w = writer(store).await;
        if held {
            w.set_held_byte_limit(limit);
        } else {
            w.set_dir_byte_limit(limit);
        }
        match w
            .backup(std::slice::from_ref(src), BackupOptions::default(), &|_| {})
            .await
        {
            Ok(_) => return limit,
            Err(err) => {
                let text = err.to_string();
                let (_, rest) = text
                    .split_once(if held { "hold " } else { "holds " })
                    .unwrap();
                limit = rest.split(' ').next().unwrap().parse().unwrap();
            }
        }
    }
    panic!("no byte limit found");
}

#[tokio::test]
async fn backup_refuses_directory_over_byte_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, target) = (tmp.path().join("repo"), tmp.path().join("out"));
    fs::create_dir(&repo_dir).unwrap();
    let src = tmp.path().canonicalize().unwrap().join("src");
    let (big, small) = (src.join("big"), src.join("small"));
    fs::create_dir_all(&big).unwrap();
    fs::create_dir_all(&small).unwrap();
    for i in 0..20 {
        fs::write(big.join(format!("{i:02}-{}", "n".repeat(100))), b"x").unwrap();
    }
    fs::write(small.join("f"), b"x").unwrap();
    let store = init_repo(&repo_dir).await;
    let bound = byte_bound(&src, false).await;
    assert!(bound > 2000, "{bound}");

    let mut w = writer(&store).await;
    w.set_dir_byte_limit(bound - 1);
    let err = w
        .backup(
            std::slice::from_ref(&src),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)), "{err:?}");
    assert!(
        err.to_string()
            .contains(&format!("more than the {} a restore reads", bound - 1)),
        "{err}"
    );
    assert!(objects(&store, "snapshots/").await.is_empty());

    let mut w = writer(&store).await;
    w.set_dir_byte_limit(bound);
    w.backup(
        std::slice::from_ref(&src),
        BackupOptions::default(),
        &|_| {},
    )
    .await
    .unwrap();
    let mut r = reader(&store).await;
    let id = r.list_snapshots().await.unwrap().entries.remove(0).id;
    r.set_dir_byte_limits(bound - 1, u64::MAX);
    let err = r
        .restore(&id, &target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::WalkLimit {
                what: "bytes in one directory",
                limit
            } if limit == bound - 1
        ),
        "{err:?}"
    );
    r.set_dir_byte_limits(bound, u64::MAX);
    r.restore(&id, &target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap();
    assert_eq!(
        fs::read(restored_path(&target, &small).join("f")).unwrap(),
        b"x"
    );
}

/// The writer and the reader add up the tree bytes of a path the same way.
#[tokio::test]
async fn backup_refuses_tree_over_held_byte_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, target) = (tmp.path().join("repo"), tmp.path().join("out"));
    fs::create_dir(&repo_dir).unwrap();
    let src = tmp.path().canonicalize().unwrap().join("src");
    let deepest = src.join("b").join("c");
    fs::create_dir_all(&deepest).unwrap();
    for (dir, n) in [(&src, 4), (&src.join("b"), 4), (&deepest, 5)] {
        for i in 0..n {
            fs::write(dir.join(format!("f{i}")), b"x").unwrap();
        }
    }
    let store = init_repo(&repo_dir).await;
    let held = byte_bound(&src, true).await;

    let mut w = writer(&store).await;
    w.set_held_byte_limit(held - 1);
    let err = w
        .backup(
            std::slice::from_ref(&src),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)), "{err:?}");
    assert!(
        err.to_string().contains(&format!(
            "more than the {} a restore holds at once",
            held - 1
        )),
        "{err}"
    );
    assert!(objects(&store, "snapshots/").await.is_empty());

    let mut w = writer(&store).await;
    w.set_held_byte_limit(held);
    w.backup(
        std::slice::from_ref(&src),
        BackupOptions::default(),
        &|_| {},
    )
    .await
    .unwrap();
    let mut r = reader(&store).await;
    let id = r.list_snapshots().await.unwrap().entries.remove(0).id;
    r.set_dir_byte_limits(u64::MAX, held - 1);
    let err = r
        .restore(&id, &target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::WalkLimit {
                what: "bytes held at once",
                limit
            } if limit == held - 1
        ),
        "{err:?}"
    );
    r.set_dir_byte_limits(u64::MAX, held);
    r.restore(&id, &target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap();
    assert_eq!(
        fs::read(restored_path(&target, &deepest).join("f4")).unwrap(),
        b"x"
    );
}

#[tokio::test]
async fn backup_large_dir_split() {
    const FILES: usize = 30_000;
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src, target) = (
        tmp.path().join("repo"),
        tmp.path().join("src"),
        tmp.path().join("out"),
    );
    fs::create_dir(&repo_dir).unwrap();
    let big = src.join("big");
    fs::create_dir_all(&big).unwrap();
    let padding = "x".repeat(120);
    for i in 0..FILES {
        fs::write(big.join(format!("{i:06}-{padding}")), b"").unwrap();
    }

    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;
    let outcome = w
        .backup(
            std::slice::from_ref(&src),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert!(!outcome.incomplete);

    let mut r = reader(&store).await;
    let snapshot = r.list_snapshots().await.unwrap().entries.remove(0).snapshot;
    let root: [u8; 32] = snapshot.root_tree.as_slice().try_into().unwrap();
    let mut nodes = decode_tree(&r.read_blob(&root, BLOB_TYPE_TREE).await.unwrap(), None).unwrap();
    let canonical = src.canonicalize().unwrap().join("big");
    let components: Vec<_> = canonical.components().skip(1).collect();
    let mut big_node = None;
    for (depth, part) in components.iter().enumerate() {
        let name = part.as_os_str().as_bytes();
        let node = nodes.iter().find(|n| n.name == name).unwrap().clone();
        if depth + 1 == components.len() {
            big_node = Some(node);
            break;
        }
        nodes.clear();
        for id in &node.subtrees {
            let id: [u8; 32] = id.as_slice().try_into().unwrap();
            nodes.extend(
                decode_tree(&r.read_blob(&id, BLOB_TYPE_TREE).await.unwrap(), None).unwrap(),
            );
        }
    }
    let big_node = big_node.unwrap();
    assert!(
        big_node.subtrees.len() > 1,
        "a directory of {FILES} entries is cut into several tree blobs"
    );

    r.restore(
        &outcome.snapshot_id,
        &target,
        &RestoreOptions::default(),
        &|_| {},
    )
    .await
    .unwrap();
    let restored = restored_path(&target, &src).join("big");
    assert_eq!(fs::read_dir(&restored).unwrap().count(), FILES);
    assert!(
        restored
            .join(format!("{:06}-{padding}", FILES - 1))
            .is_file()
    );
}

#[cfg(target_os = "linux")]
mod linux {
    use std::process::Command;

    use lodestore_format::fs::walk::WalkOptions;

    use super::*;

    struct Mounted(PathBuf);

    impl Drop for Mounted {
        fn drop(&mut self) {
            let _ = Command::new("umount").arg(&self.0).status();
        }
    }

    #[tokio::test]
    #[ignore = "requires root; run scripts/test-linux.sh --root backup_root_"]
    async fn backup_root_one_file_system() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo_dir, src, target) = (
            tmp.path().join("repo"),
            tmp.path().join("src"),
            tmp.path().join("out"),
        );
        fs::create_dir(&repo_dir).unwrap();
        fs::create_dir_all(src.join("mnt")).unwrap();
        fs::write(src.join("local.txt"), b"on the source filesystem").unwrap();
        let status = Command::new("mount")
            .args(["-t", "tmpfs", "tmpfs"])
            .arg(src.join("mnt"))
            .status()
            .unwrap();
        assert!(status.success(), "mounting tmpfs needs root");
        let _mounted = Mounted(src.join("mnt"));
        fs::write(src.join("mnt/other.txt"), b"on another filesystem").unwrap();

        let store = init_repo(&repo_dir).await;
        let mut w = writer(&store).await;
        let outcome = w
            .backup(
                std::slice::from_ref(&src),
                BackupOptions {
                    walk: WalkOptions {
                        one_file_system: true,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                &|_| {},
            )
            .await
            .unwrap();
        assert!(!outcome.incomplete);

        let mut r = reader(&store).await;
        r.restore(
            &outcome.snapshot_id,
            &target,
            &RestoreOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
        let restored = restored_path(&target, &src);
        assert!(restored.join("local.txt").is_file());
        assert!(restored.join("mnt").is_dir());
        assert_eq!(fs::read_dir(restored.join("mnt")).unwrap().count(), 0);
    }
}

fn make_fifo(path: &Path) {
    let status = std::process::Command::new("mkfifo")
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn prescan_totals_match_outcome() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir_all(src.join("a/b")).unwrap();
    fs::create_dir(src.join("cache")).unwrap();
    fs::write(src.join("top.txt"), b"hello").unwrap();
    fs::write(src.join("empty"), b"").unwrap();
    fs::write(src.join("noise.log"), b"excluded by pattern").unwrap();
    fs::write(src.join("a/one.bin"), random_bytes(300_000, 1)).unwrap();
    fs::write(src.join("a/b/two.bin"), random_bytes(2 << 20, 2)).unwrap();
    fs::write(
        src.join("cache/CACHEDIR.TAG"),
        b"Signature: 8a477f597d28d172789f06886806bc55",
    )
    .unwrap();
    fs::write(src.join("cache/skipped.bin"), random_bytes(100_000, 3)).unwrap();
    std::os::unix::fs::symlink("top.txt", src.join("link")).unwrap();

    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;
    let reports = RefCell::new(Vec::<BackupProgress>::new());
    let outcome = w
        .backup(
            std::slice::from_ref(&src),
            BackupOptions {
                prescan: true,
                walk: WalkOptions {
                    exclude: Some(PathMatcher::new(["*.log"]).unwrap()),
                    exclude_caches: true,
                    ..Default::default()
                },
                ..Default::default()
            },
            &|p| reports.borrow_mut().push(p.clone()),
        )
        .await
        .unwrap();

    let reports = reports.into_inner();
    let last = reports.last().unwrap();
    let stats = &outcome.stats;
    assert!(last.scan_done);
    assert_eq!(
        last.files_total,
        stats.files_new + stats.files_changed + stats.files_unchanged
    );
    assert_eq!(last.bytes_total, stats.bytes_total);
    assert_eq!(last.bytes_processed, stats.bytes_total);
    assert!(reports.iter().all(|r| r.files_total <= last.files_total));
}

#[tokio::test]
async fn cancel_after_the_walk_with_prescan_writes_no_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir(&src).unwrap();
    fs::write(src.join("only.bin"), random_bytes(100_000, 1)).unwrap();

    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;
    let cancel = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&cancel);
    let result = w
        .backup(
            std::slice::from_ref(&src),
            BackupOptions {
                prescan: true,
                cancel,
                ..Default::default()
            },
            &|p| {
                if !p.starting && p.files_done == 1 {
                    stop.store(true, Ordering::Relaxed);
                }
            },
        )
        .await;
    assert!(matches!(result, Err(Error::Interrupted)));
    assert!(objects(&store, "snapshots/").await.is_empty());
}

#[tokio::test]
async fn prescan_off_reports_zero_totals() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir(&src).unwrap();
    fs::write(src.join("f"), b"data").unwrap();
    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;
    let reports = RefCell::new(Vec::<BackupProgress>::new());
    w.backup(std::slice::from_ref(&src), BackupOptions::default(), &|p| {
        reports.borrow_mut().push(p.clone())
    })
    .await
    .unwrap();
    let reports = reports.into_inner();
    assert!(!reports.is_empty());
    assert!(
        reports
            .iter()
            .all(|r| r.files_total == 0 && r.bytes_total == 0 && !r.scan_done)
    );
}

#[tokio::test]
async fn progress_reports_inside_large_file() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir(&src).unwrap();
    fs::write(src.join("big.bin"), random_bytes(32 << 20, 4)).unwrap();
    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;
    let reports = RefCell::new(Vec::<BackupProgress>::new());
    w.backup(std::slice::from_ref(&src), BackupOptions::default(), &|p| {
        reports.borrow_mut().push(p.clone())
    })
    .await
    .unwrap();

    let reports = reports.into_inner();
    let inside = reports
        .iter()
        .filter(|r| !r.starting && r.files_done == 0)
        .count();
    assert!(inside > 4, "{inside} reports inside the file");
    assert!(
        reports
            .windows(2)
            .all(|w| w[0].bytes_processed <= w[1].bytes_processed)
    );
    assert_eq!(reports.last().unwrap().bytes_processed, 32 << 20);
}

type Seen = (bool, pb::IssueKind, Vec<u8>);

fn collect_issues(seen: &RefCell<Vec<Seen>>) -> Box<dyn Fn(&IssueEvent<'_>) + '_> {
    Box::new(|e| {
        seen.borrow_mut().push((e.warning, e.kind, e.path.to_vec()));
    })
}

#[tokio::test]
async fn issue_callback_sees_warnings() {
    const FIFOS: usize = 1005;
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir(&src).unwrap();
    for i in 0..FIFOS {
        make_fifo(&src.join(format!("p{i:04}")));
    }
    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;
    let seen = RefCell::new(Vec::new());
    let outcome = w
        .backup(
            std::slice::from_ref(&src),
            BackupOptions {
                on_issue: Some(collect_issues(&seen)),
                ..Default::default()
            },
            &|_| {},
        )
        .await
        .unwrap();

    let seen = seen.into_inner();
    assert_eq!(seen.len() as u64, outcome.warnings_total);
    assert!(seen.len() >= FIFOS);
    assert!(seen.iter().all(|(warning, ..)| *warning));
    let r = reader(&store).await;
    let snapshot = r.list_snapshots().await.unwrap().entries.remove(0).snapshot;
    assert_eq!(snapshot.warnings.len(), 1000);
    assert_eq!(snapshot.warnings_total, outcome.warnings_total);
}

#[tokio::test]
async fn special_files_warn() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir(&src).unwrap();
    make_fifo(&src.join("pipe"));
    std::os::unix::fs::symlink("pipe", src.join("link")).unwrap();
    fs::write(src.join("file"), b"data").unwrap();
    let pipe = src.canonicalize().unwrap().join("pipe");

    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;
    let seen = RefCell::new(Vec::new());
    let outcome = w
        .backup(
            std::slice::from_ref(&src),
            BackupOptions {
                on_issue: Some(collect_issues(&seen)),
                ..Default::default()
            },
            &|_| {},
        )
        .await
        .unwrap();

    assert!(!outcome.incomplete);
    assert_eq!(outcome.errors_total, 0);
    assert_eq!(outcome.warnings_total, 1);
    assert_eq!(
        seen.into_inner(),
        vec![(
            true,
            pb::IssueKind::UnsupportedType,
            pipe.as_os_str().as_bytes().to_vec()
        )]
    );
}

#[tokio::test]
#[ignore = "needs root"]
async fn special_files_warn_devices() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir(&src).unwrap();
    let status = std::process::Command::new("mknod")
        .arg(src.join("null"))
        .args(["c", "1", "3"])
        .status()
        .unwrap();
    assert!(status.success());
    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;
    let outcome = w
        .backup(
            std::slice::from_ref(&src),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert!(!outcome.incomplete);
    assert_eq!(outcome.warnings_total, 1);
}

async fn reachable_trees(r: &mut Reader<&LocalFsStore>, root: BlobId) -> HashMap<BlobId, Vec<u8>> {
    let mut found = HashMap::new();
    let mut todo = vec![root];
    while let Some(id) = todo.pop() {
        let bytes = r.read_blob(&id, BLOB_TYPE_TREE).await.unwrap();
        for node in decode_tree(&bytes, None).unwrap() {
            for sub in &node.subtrees {
                todo.push(sub.as_slice().try_into().unwrap());
            }
        }
        found.insert(id, bytes);
    }
    found
}

#[tokio::test]
async fn tree_sink_receives_every_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir_all(src.join("a/b")).unwrap();
    fs::create_dir(src.join("c")).unwrap();
    fs::write(src.join("a/b/f"), b"x").unwrap();
    fs::write(src.join("c/g"), b"y").unwrap();

    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;
    for run in 0..2 {
        let sunk = RefCell::new(HashMap::<BlobId, Vec<u8>>::new());
        let outcome = w
            .backup(
                std::slice::from_ref(&src),
                BackupOptions {
                    tree_sink: Some(Box::new(|id, bytes| {
                        sunk.borrow_mut().insert(*id, bytes.to_vec());
                    })),
                    ..Default::default()
                },
                &|_| {},
            )
            .await
            .unwrap();
        let sunk = sunk.into_inner();
        let mut r = reader(&store).await;
        let reachable = reachable_trees(&mut r, outcome.root_tree).await;
        assert!(reachable.len() >= 5, "run {run}: {} trees", reachable.len());
        for (id, bytes) in &reachable {
            assert_eq!(sunk.get(id), Some(bytes), "run {run}");
        }
    }
}

#[tokio::test]
async fn agent_id_recorded_in_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir(&src).unwrap();
    fs::write(src.join("f"), b"data").unwrap();
    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;
    let outcome = w
        .backup(
            std::slice::from_ref(&src),
            BackupOptions {
                agent_id: Some(b"agent-1".to_vec()),
                time: Some(pb::Timestamp {
                    seconds: 1_700_000_000,
                    nanos: 5,
                }),
                ..Default::default()
            },
            &|_| {},
        )
        .await
        .unwrap();
    assert_eq!(outcome.time.seconds, 1_700_000_000);

    let r = reader(&store).await;
    let entry = r.list_snapshots().await.unwrap().entries.remove(0);
    assert_eq!(entry.snapshot.agent_id, b"agent-1");
    assert_eq!(entry.snapshot.root_tree, outcome.root_tree.to_vec());
}

#[tokio::test]
async fn connect_opens_with_writer_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir(&src).unwrap();
    fs::write(src.join("f"), b"data").unwrap();
    let store = init_repo(&repo_dir).await;
    let repo = Repository::open(&store, b"pw").await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());

    let mut w = Writer::connect(&store, keys).await.unwrap();
    let outcome = w
        .backup(
            std::slice::from_ref(&src),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert!(!outcome.incomplete);
}

#[tokio::test]
async fn connect_rejects_foreign_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let (dir_a, dir_b, empty) = (
        tmp.path().join("a"),
        tmp.path().join("b"),
        tmp.path().join("empty"),
    );
    for d in [&dir_a, &dir_b, &empty] {
        fs::create_dir(d).unwrap();
    }
    let (a, b) = (init_repo(&dir_a).await, init_repo(&dir_b).await);
    let repo_b = Repository::open(&b, b"pw").await.unwrap();
    let keys_b = repo_b.read_keys().write_keys(repo_b.config_hash());
    assert!(Writer::connect(&a, keys_b).await.is_err());

    let keys_a = {
        let repo = Repository::open(&a, b"pw").await.unwrap();
        repo.read_keys().write_keys(repo.config_hash())
    };
    let nowhere = LocalFsStore::new(empty);
    let err = Writer::connect(&nowhere, keys_a).await.err().unwrap();
    assert!(matches!(err, Error::NotInitialized), "{err}");
}
