use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use lodestore_format::crypto::{BLOB_TYPE_TREE, BlobId, Entropy};
use lodestore_format::keys::{KdfCost, WriteKeys};
use lodestore_format::pb;
use lodestore_format::reader::Reader;
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::store::LocalFsStore;
use lodestore_format::tree::decode_tree;
use lodestore_format::tree_cache::{CacheWriter, open_parent};
use lodestore_format::writer::{BackupOptions, BackupOutcome, Writer};
use std::os::unix::fs::PermissionsExt;

const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

struct Fixture {
    _tmp: tempfile::TempDir,
    store: LocalFsStore,
    src: PathBuf,
    cache: PathBuf,
}

/// A file the writer may trust as unchanged: its mtime is well before the
/// snapshot that will be the parent.
fn write_old(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    let old = SystemTime::now() - Duration::from_secs(3600);
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(old)
        .unwrap();
}

async fn init(store: &LocalFsStore) {
    let mut entropy = Entropy::os();
    let options = InitOptions {
        kdf: LOW_KDF,
        ..Default::default()
    };
    PendingInit::generate(&mut entropy, options)
        .unwrap()
        .commit(store, b"pw", &mut entropy)
        .await
        .unwrap();
}

async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    let src = tmp.path().join("src");
    fs::create_dir(&repo_dir).unwrap();
    for dir in ["a", "b"] {
        fs::create_dir_all(src.join(dir)).unwrap();
        for i in 0..10 {
            write_old(
                &src.join(dir).join(format!("{dir}{i}.txt")),
                &format!("{dir} file {i} {}", "x".repeat(i * 100)),
            );
        }
    }
    let store = LocalFsStore::new(repo_dir);
    init(&store).await;
    Fixture {
        cache: tmp.path().join("cache").join("policy"),
        _tmp: tmp,
        store,
        src,
    }
}

impl Fixture {
    async fn repo(&self) -> Repository {
        Repository::open(&self.store, b"pw").await.unwrap()
    }

    async fn keys(&self) -> WriteKeys {
        let repo = self.repo().await;
        repo.read_keys().write_keys(repo.config_hash())
    }

    async fn reader(&self) -> Reader<&LocalFsStore> {
        let repo = self.repo().await;
        Reader::open(&self.store, repo.read_keys().duplicate(), repo.config()).unwrap()
    }

    async fn backup(&self, parent_dir: Option<&Path>) -> BackupOutcome {
        let repo = self.repo().await;
        let keys = repo.read_keys().write_keys(repo.config_hash());
        let parent = match parent_dir {
            Some(dir) => open_parent(dir, &keys).await,
            None => None,
        };
        let mut writer = Writer::new(&self.store, keys, repo.config()).await.unwrap();
        let options = BackupOptions {
            hostname: Some(b"host".to_vec()),
            parent,
            ..Default::default()
        };
        writer
            .backup(std::slice::from_ref(&self.src), options, &|_| {})
            .await
            .unwrap()
    }

    async fn snapshot(&self, outcome: &BackupOutcome) -> pb::Snapshot {
        self.reader()
            .await
            .list_snapshots()
            .await
            .unwrap()
            .entries
            .into_iter()
            .find(|e| e.id == outcome.snapshot_id)
            .unwrap()
            .snapshot
    }

    /// Fills the cache the way the agent does from its tree sink, here by
    /// reading the repository with the password.
    async fn cache_snapshot(&self, outcome: &BackupOutcome) {
        let snapshot = self.snapshot(outcome).await;
        let mut reader = self.reader().await;
        let root: BlobId = snapshot.root_tree.as_slice().try_into().unwrap();
        let mut cache = CacheWriter::create(&self.cache).unwrap();
        let mut pending = vec![vec![snapshot.root_tree.clone()]];
        while let Some(ids) = pending.pop() {
            let mut previous: Option<Vec<u8>> = None;
            for raw in ids {
                let id: BlobId = raw.as_slice().try_into().unwrap();
                let bytes = reader.read_blob(&id, BLOB_TYPE_TREE).await.unwrap();
                cache.put(&id, &bytes).unwrap();
                let nodes = decode_tree(&bytes, previous.as_deref()).unwrap();
                previous = nodes.last().map(|n| n.name.clone());
                pending.extend(
                    nodes
                        .into_iter()
                        .filter(|n| !n.subtrees.is_empty())
                        .map(|n| n.subtrees),
                );
            }
        }
        cache
            .commit(
                self.repo().await.read_keys().repo_id(),
                &outcome.snapshot_id,
                &snapshot.time.unwrap(),
                &root,
            )
            .unwrap();
    }
}

#[tokio::test]
async fn tree_cache_parent_skips_unchanged_files() {
    let fx = fixture().await;
    let first = fx.backup(None).await;
    assert_eq!(first.stats.files_new, 20);
    fx.cache_snapshot(&first).await;

    let second = fx.backup(Some(&fx.cache)).await;
    assert_eq!(second.stats.bytes_read, 0);
    assert_eq!(second.stats.files_unchanged, 20);
    let (a, b) = (fx.snapshot(&first).await, fx.snapshot(&second).await);
    assert_eq!(a.root_tree, b.root_tree);
    assert_eq!(b.parent, first.snapshot_id);
}

#[tokio::test]
async fn tree_cache_missing_or_corrupt_is_ignored() {
    let fx = fixture().await;
    let keys = fx.keys().await;
    assert!(open_parent(&fx.cache, &keys).await.is_none());

    let first = fx.backup(None).await;
    fx.cache_snapshot(&first).await;
    assert!(open_parent(&fx.cache, &keys).await.is_some());

    let parent = fx.cache.join("parent.json");
    let good = fs::read(&parent).unwrap();
    for junk in [&b"not json"[..], b"{}", b"{\"repository_id\": 1}", b""] {
        fs::write(&parent, junk).unwrap();
        assert!(open_parent(&fx.cache, &keys).await.is_none());
    }
    fs::write(&parent, &good).unwrap();
    assert!(open_parent(&fx.cache, &keys).await.is_some());

    let root = fx.snapshot(&first).await.root_tree;
    let root_file = fx.cache.join("trees").join(hex(&root));
    fs::write(&root_file, b"garbage").unwrap();
    assert!(open_parent(&fx.cache, &keys).await.is_none());

    let again = fx.backup(Some(&fx.cache)).await;
    assert_eq!(again.stats.files_unchanged, 0);
    assert_eq!(again.stats.files_new, 20);
    assert_eq!(
        fx.snapshot(&again).await.root_tree,
        fx.snapshot(&first).await.root_tree
    );
}

fn hex(id: &[u8]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::test]
async fn tree_cache_tampered_tree_is_reread() {
    let fx = fixture().await;
    let first = fx.backup(None).await;
    fx.cache_snapshot(&first).await;

    let mut victim = None;
    for entry in fs::read_dir(fx.cache.join("trees")).unwrap() {
        let path = entry.unwrap().path();
        let bytes = zstd::bulk::decompress(&fs::read(&path).unwrap(), 1 << 24).unwrap();
        if decode_tree(&bytes, None)
            .unwrap()
            .iter()
            .any(|n| n.name == b"a0.txt")
        {
            victim = Some(path);
        }
    }
    let victim = victim.expect("the directory a is cached");
    fs::write(&victim, b"not a tree").unwrap();

    let second = fx.backup(Some(&fx.cache)).await;
    assert_eq!(second.stats.files_unchanged, 10);
    assert!(second.stats.bytes_read > 0);
    assert_eq!(
        fx.snapshot(&second).await.root_tree,
        fx.snapshot(&first).await.root_tree
    );
}

#[tokio::test]
async fn tree_cache_commit_is_atomic() {
    let fx = fixture().await;
    let keys = fx.keys().await;
    let first = fx.backup(None).await;
    fx.cache_snapshot(&first).await;
    let before = fs::read(fx.cache.join("parent.json")).unwrap();

    let mut abandoned = CacheWriter::create(&fx.cache).unwrap();
    abandoned.put(&[7; 32], b"half a cache").unwrap();
    drop(abandoned);
    assert_eq!(fs::read(fx.cache.join("parent.json")).unwrap(), before);
    assert!(open_parent(&fx.cache, &keys).await.is_some());
    let new = fx.cache.with_file_name("policy.new");
    assert!(new.exists());

    let second = fx.backup(Some(&fx.cache)).await;
    fx.cache_snapshot(&second).await;
    assert!(!new.exists());
    assert!(!fx.cache.with_file_name("policy.old").exists());
    let after = fs::read(fx.cache.join("parent.json")).unwrap();
    assert!(
        String::from_utf8(after)
            .unwrap()
            .contains(&hex(&second.snapshot_id))
    );

    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&fx.cache), 0o700);
    assert_eq!(mode(&fx.cache.join("trees")), 0o700);
    assert_eq!(mode(&fx.cache.join("parent.json")), 0o600);
    let tree = fs::read_dir(fx.cache.join("trees"))
        .unwrap()
        .next()
        .unwrap();
    assert_eq!(mode(&tree.unwrap().path()), 0o600);
}

#[tokio::test]
async fn tree_cache_rejects_other_repository() {
    let fx = fixture().await;
    let first = fx.backup(None).await;
    fx.cache_snapshot(&first).await;
    assert!(open_parent(&fx.cache, &fx.keys().await).await.is_some());

    let other_dir = fx._tmp.path().join("other");
    fs::create_dir(&other_dir).unwrap();
    let other = LocalFsStore::new(other_dir);
    init(&other).await;
    let repo = Repository::open(&other, b"pw").await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    assert!(open_parent(&fx.cache, &keys).await.is_none());
}
