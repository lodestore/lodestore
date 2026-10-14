use std::cell::Cell;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use lodestore_format::Error;
use lodestore_format::crypto::{BLOB_TYPE_TREE, Entropy};
use lodestore_format::keys::KdfCost;
use lodestore_format::pb;
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::store::{BlobStore, LocalFsStore};
use lodestore_format::tree::decode_tree;
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
