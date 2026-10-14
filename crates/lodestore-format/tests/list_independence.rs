//! Restoring a snapshot must not depend on LIST: shards come from the
//! snapshot's `index_refs`, and LIST is only a fallback.

use std::fs;
use std::ops::Range;
use std::path::Path;

use bytes::Bytes;
use lodestore_format::crypto::Entropy;
use lodestore_format::index::{hex32, open_shard, parse_hex32, seal_shard};
use lodestore_format::keys::KdfCost;
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::store::{BlobError, BlobStore, ListEntry, LocalFsStore};
use lodestore_format::writer::{BackupOptions, BackupOutcome, Writer};
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{Rng, SeedableRng};

const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

/// Lists nothing under `index/`, as an eventually consistent store might.
struct LaggyListStore<S: BlobStore>(S);

impl<S: BlobStore> BlobStore for LaggyListStore<S> {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<(), BlobError> {
        self.0.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        self.0.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.0.get_range(key, range).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        if prefix.starts_with("index/") {
            return Ok(Vec::new());
        }
        self.0.list(prefix).await
    }

    async fn ensure_prefixes(&self, prefixes: &[&str]) -> Result<(), BlobError> {
        self.0.ensure_prefixes(prefixes).await
    }
}

fn make_tree(root: &Path) {
    let mut rng = ChaCha20Rng::from_seed([5; 32]);
    let mut random = vec![0u8; 3 << 20];
    rng.fill_bytes(&mut random);
    fs::create_dir_all(root.join("a/b")).unwrap();
    fs::write(root.join("a/b/random.bin"), &random).unwrap();
    fs::write(root.join("top.txt"), b"hello").unwrap();
}

async fn init_and_backup(repo_dir: &Path, src: &Path) -> (LocalFsStore, BackupOutcome) {
    let store = LocalFsStore::new(repo_dir.to_path_buf());
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
    let mut w = Writer::new(&store, keys, repo.config()).await.unwrap();
    let outcome = w
        .backup(&[src.to_path_buf()], BackupOptions::default(), &|_| {})
        .await
        .unwrap();
    (store, outcome)
}

async fn restore_and_compare<S: BlobStore>(store: S, outcome: &BackupOutcome, src: &Path) {
    let repo = Repository::open(&store, b"pw").await.unwrap();
    let mut r = Reader::open(&store, repo.read_keys().duplicate(), repo.config()).unwrap();
    let out = tempfile::tempdir().unwrap();
    r.restore(
        &outcome.snapshot_id,
        out.path(),
        &RestoreOptions::default(),
        &|_| {},
    )
    .await
    .unwrap();
    let restored = out
        .path()
        .join(src.canonicalize().unwrap().strip_prefix("/").unwrap());
    for rel in ["a/b/random.bin", "top.txt"] {
        assert!(
            fs::read(src.join(rel)).unwrap() == fs::read(restored.join(rel)).unwrap(),
            "{rel} differs"
        );
    }
}

#[tokio::test]
async fn list_independence_restore_by_id() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    make_tree(&src);
    let (store, outcome) = init_and_backup(&tmp.path().join("repo"), &src).await;

    let laggy = LaggyListStore(&store);
    assert!(laggy.list("index/").await.unwrap().is_empty());
    assert!(!store.list("index/").await.unwrap().is_empty());

    restore_and_compare(laggy, &outcome, &src).await;
}

#[tokio::test]
async fn list_independence_missing_ref_falls_back() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    make_tree(&src);
    let repo_dir = tmp.path().join("repo");
    let (store, outcome) = init_and_backup(&repo_dir, &src).await;

    // Re-seal every shard under a new name and drop the old one, so the
    // snapshot's references point at objects that no longer exist.
    let repo = Repository::open(&store, b"pw").await.unwrap();
    let keys = repo.read_keys();
    let mut entropy = Entropy::os();
    for shard in store.list("index/").await.unwrap() {
        let old_id = parse_hex32(shard.key.strip_prefix("index/").unwrap()).unwrap();
        let bytes = store.get(&shard.key).await.unwrap();
        let opened = open_shard(keys.k_idx(), keys.repo_id(), &old_id, &bytes).unwrap();
        let (new_id, new_bytes) =
            seal_shard(keys.k_idx(), keys.repo_id(), opened.packs, &mut entropy).unwrap();
        assert_ne!(new_id, old_id);
        store
            .put_if_absent(&format!("index/{}", hex32(&new_id)), Bytes::from(new_bytes))
            .await
            .unwrap();
        fs::remove_file(repo_dir.join(&shard.key)).unwrap();
        assert!(!repo_dir.join(&shard.key).exists());
    }

    restore_and_compare(&store, &outcome, &src).await;
}
