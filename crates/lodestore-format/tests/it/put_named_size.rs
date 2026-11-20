use std::collections::VecDeque;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use bytes::Bytes;
use lodestore_format::Error;
use lodestore_format::crypto::Entropy;
use lodestore_format::keys::KdfCost;
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::store::{BlobError, BlobStore, ListEntry, LocalFsStore, put_named};
use lodestore_format::writer::{BackupOptions, Writer};
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{Rng, SeedableRng};

const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

#[derive(Clone, Debug)]
pub enum Tear {
    Short(usize),
    Hole(Range<usize>),
}

/// A store that publishes non-atomically, like an SFTP direct write: the
/// first write of each `packs/` key listed in the plan leaves a torn object
/// under the final name and reports `AlreadyExists`, as the store does after
/// its own retry finds the half-written file.
pub struct TornStore {
    pub inner: LocalFsStore,
    pub atomic: bool,
    plan: Mutex<VecDeque<Option<Tear>>>,
    burned: Mutex<Vec<String>>,
}

impl TornStore {
    pub fn new(inner: LocalFsStore, plan: impl IntoIterator<Item = Option<Tear>>) -> Self {
        TornStore {
            inner,
            atomic: false,
            plan: Mutex::new(plan.into_iter().collect()),
            burned: Mutex::default(),
        }
    }

    pub fn burned(&self) -> Vec<String> {
        self.burned.lock().unwrap().clone()
    }
}

fn torn(tear: &Tear, data: &Bytes) -> Bytes {
    let len = data.len();
    match tear {
        Tear::Short(n) => data.slice(..(*n).min(len.saturating_sub(1))),
        Tear::Hole(r) => {
            let start = r.start.min(len.saturating_sub(1));
            let end = r.end.clamp(start + 1, len);
            let mut v = data.to_vec();
            v[start..end].fill(0);
            Bytes::from(v)
        }
    }
}

impl BlobStore for TornStore {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<(), BlobError> {
        if key.starts_with("packs/") {
            let tear = self.plan.lock().unwrap().pop_front().flatten();
            if let Some(tear) = tear {
                let leftover = torn(&tear, &data);
                let _ = self.inner.put_if_absent(key, leftover).await;
                self.burned.lock().unwrap().push(key.to_string());
                return Err(BlobError::AlreadyExists);
            }
        }
        self.inner.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.inner.get_range(key, range).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        self.inner.list(prefix).await
    }

    async fn object_size(&self, key: &str) -> Result<Option<u64>, BlobError> {
        self.inner.object_size(key).await
    }

    fn publishes_atomically(&self) -> bool {
        self.atomic
    }
}

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

fn random_bytes(len: usize, seed: u8) -> Vec<u8> {
    let mut rng = ChaCha20Rng::from_seed([seed; 32]);
    let mut out = vec![0u8; len];
    rng.fill_bytes(&mut out);
    out
}

fn restored_path(target: &Path, src: &Path) -> PathBuf {
    let canonical = src.canonicalize().unwrap();
    target.join(canonical.strip_prefix("/").unwrap())
}

async fn backup_and_restore(store: &TornStore, src: &Path, target: &Path) {
    let repo = Repository::open(store, b"pw").await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    let mut writer = Writer::new(store, keys, repo.config()).await.unwrap();
    let outcome = writer
        .backup(
            &[src.to_path_buf()],
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    let mut reader = Reader::open(store, repo.read_keys().duplicate(), repo.config()).unwrap();
    reader
        .restore(
            &outcome.snapshot_id,
            target,
            &RestoreOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
}

fn small_tree(dir: &Path) -> PathBuf {
    let src = dir.join("src");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("a.bin"), random_bytes(300 << 10, 1)).unwrap();
    fs::write(src.join("b.txt"), b"plain text ".repeat(2000)).unwrap();
    src
}

#[tokio::test]
async fn put_named_same_bytes_twice_ok() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalFsStore::new(dir.path().to_path_buf());
    put_named(&store, "packs/ab/x", vec![7; 64]).await.unwrap();
    put_named(&store, "packs/ab/x", vec![7; 64]).await.unwrap();
}

#[tokio::test]
async fn put_named_size_mismatch_is_name_burned() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalFsStore::new(dir.path().to_path_buf());
    store
        .put_if_absent("packs/ab/x", Bytes::from_static(&[7; 10]))
        .await
        .unwrap();
    let err = put_named(&store, "packs/ab/x", vec![7; 64])
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::NameBurned { key } if key == "packs/ab/x"),
        "{err:?}"
    );
}

#[tokio::test]
async fn put_named_size_burned_pack_is_resealed_and_restores() {
    let tmp = tempfile::tempdir().unwrap();
    let src = small_tree(tmp.path());
    let store = TornStore::new(
        init_repo(&tmp.path().join("repo")).await,
        [Some(Tear::Short(100))],
    );
    let target = tmp.path().join("out");
    backup_and_restore(&store, &src, &target).await;

    let burned = store.burned();
    assert_eq!(burned.len(), 1);
    let left = store.inner.object_size(&burned[0]).await.unwrap();
    assert_eq!(left, Some(100), "the torn object keeps its name");
    let restored = restored_path(&target, &src);
    for name in ["a.bin", "b.txt"] {
        assert_eq!(
            fs::read(src.join(name)).unwrap(),
            fs::read(restored.join(name)).unwrap()
        );
    }
}
