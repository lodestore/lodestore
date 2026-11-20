use std::collections::VecDeque;
use std::fs;
use std::future::Future;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::gc_support::{Fx, assert_same_files};
use bytes::Bytes;
use lodestore_format::Error;
use lodestore_format::crypto::{BlobId, Entropy};
use lodestore_format::index::{open_shard, parse_hex32};
use lodestore_format::keys::KdfCost;
use lodestore_format::pack::pack_object_key;
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::store::{BlobError, BlobStore, ListEntry, LocalFsStore, put_named};
use lodestore_format::writer::{BackupOptions, Writer};
use proptest::prelude::*;
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
        .backup(&[src.to_path_buf()], BackupOptions::default(), &|_| {})
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

/// A store whose answers are set by the test.
struct Odd {
    inner: LocalFsStore,
    atomic: bool,
    deny_put: bool,
    deny_stat: bool,
    deny_get: bool,
}

impl Odd {
    fn new(inner: LocalFsStore, atomic: bool) -> Self {
        Odd {
            inner,
            atomic,
            deny_put: false,
            deny_stat: false,
            deny_get: false,
        }
    }
}

fn denied() -> BlobError {
    BlobError::Denied {
        message: "AccessDenied".into(),
    }
}

impl BlobStore for Odd {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<(), BlobError> {
        if self.deny_put {
            return Err(denied());
        }
        self.inner.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, BlobError> {
        if self.deny_get {
            return Err(denied());
        }
        self.inner.get_range(key, range).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        self.inner.list(prefix).await
    }

    async fn object_size(&self, key: &str) -> Result<Option<u64>, BlobError> {
        if self.deny_stat {
            return Err(denied());
        }
        self.inner.object_size(key).await
    }

    fn publishes_atomically(&self) -> bool {
        self.atomic
    }
}

const KEY: &str = "packs/ab/x";

fn body() -> Vec<u8> {
    random_bytes(4096, 9)
}

async fn odd_with(stored: Option<Bytes>, atomic: bool) -> (tempfile::TempDir, Odd) {
    let dir = tempfile::tempdir().unwrap();
    let inner = LocalFsStore::new(dir.path().to_path_buf());
    if let Some(stored) = stored {
        inner.put_if_absent(KEY, stored).await.unwrap();
    }
    (dir, Odd::new(inner, atomic))
}

fn is_burned(r: lodestore_format::Result<()>) -> bool {
    matches!(r, Err(Error::NameBurned { .. }))
}

fn is_denied(r: lodestore_format::Result<()>) -> bool {
    matches!(r, Err(Error::Store(BlobError::Denied { .. })))
}

#[tokio::test]
async fn put_named_non_atomic_full_size_hole_is_name_burned() {
    let bytes = Bytes::from(body());
    let holed = torn(&Tear::Hole(100..300), &bytes);
    assert_eq!(holed.len(), bytes.len());
    let (_d, store) = odd_with(Some(holed), false).await;
    assert!(is_burned(put_named(&store, KEY, bytes.to_vec()).await));
}

#[tokio::test]
async fn put_named_non_atomic_equal_bytes_ok() {
    let bytes = Bytes::from(body());
    let (_d, store) = odd_with(Some(bytes.clone()), false).await;
    put_named(&store, KEY, bytes.to_vec()).await.unwrap();
}

#[tokio::test]
async fn put_named_non_atomic_unreadable_is_error() {
    let bytes = Bytes::from(body());
    let (_d, mut store) = odd_with(Some(bytes.clone()), false).await;
    store.deny_get = true;
    assert!(is_denied(put_named(&store, KEY, bytes.to_vec()).await));
}

#[tokio::test]
async fn put_named_denied_existing_equal_size_ok() {
    let bytes = Bytes::from(body());
    for atomic in [true, false] {
        let (_d, mut store) = odd_with(Some(bytes.clone()), atomic).await;
        store.deny_put = true;
        put_named(&store, KEY, bytes.to_vec()).await.unwrap();

        let short = bytes.slice(..100);
        let (_d, mut store) = odd_with(Some(short), atomic).await;
        store.deny_put = true;
        assert!(is_burned(put_named(&store, KEY, bytes.to_vec()).await));
    }
    let holed = torn(&Tear::Hole(10..20), &bytes);
    let (_d, mut store) = odd_with(Some(holed), false).await;
    store.deny_put = true;
    assert!(is_burned(put_named(&store, KEY, bytes.to_vec()).await));
}

#[tokio::test]
async fn put_named_denied_missing_stays_denied() {
    let (_d, mut store) = odd_with(None, true).await;
    store.deny_put = true;
    assert!(is_denied(put_named(&store, KEY, body()).await));

    let bytes = Bytes::from(body());
    let (_d, mut store) = odd_with(Some(bytes.clone()), true).await;
    store.deny_put = true;
    store.deny_stat = true;
    assert!(is_denied(put_named(&store, KEY, bytes.to_vec()).await));
}

#[tokio::test]
async fn put_named_atomic_unstatable_already_exists_ok() {
    let bytes = Bytes::from(body());
    let (_d, mut store) = odd_with(Some(bytes.clone()), true).await;
    store.deny_stat = true;
    put_named(&store, KEY, bytes.to_vec()).await.unwrap();
}

#[tokio::test]
async fn put_named_non_atomic_unstatable_is_error() {
    let bytes = Bytes::from(body());
    let (_d, mut store) = odd_with(Some(bytes.clone()), false).await;
    store.deny_stat = true;
    assert!(is_denied(put_named(&store, KEY, bytes.to_vec()).await));
}

#[tokio::test]
async fn put_named_size_gc_repack_reseals() {
    use lodestore_format::gc::repack::repack;

    let fx = Fx::new().await;
    let shared = ("shared", 1, 100 << 10);
    let a = fx.backup("a", &[shared, ("only-a", 2, 200 << 10)]).await;
    fx.backup("b", &[shared, ("only-b", 3, 200 << 10)]).await;
    fx.forget(&a.snapshot_id).await;

    let fx = fx.wrap(|local| TornStore::new(local, [Some(Tear::Short(100))]));
    let survey = fx.survey(50).await;
    assert!(!survey.plan.repack.is_empty());
    let out = repack(
        &fx.store,
        fx.repo.read_keys(),
        &survey,
        &mut Entropy::os(),
        &|_, _| {},
    )
    .await
    .unwrap();

    let burned = fx.store.burned();
    assert_eq!(burned.len(), 1);
    assert!(!out.new_packs.is_empty());
    let mut written = 0;
    for id in &out.new_packs {
        let key = pack_object_key(id);
        assert_ne!(key, burned[0]);
        written += fx.store.object_size(&key).await.unwrap().unwrap();
    }
    assert_eq!(written, out.bytes_written);
}

const MAX_RESEALS: usize = 3;

async fn backup_with<S: BlobStore>(store: &S, src: &Path) -> lodestore_format::Result<BlobId> {
    let repo = Repository::open(store, b"pw").await?;
    let keys = repo.read_keys().write_keys(repo.config_hash());
    let mut writer = Writer::new(store, keys, repo.config()).await?;
    let options = BackupOptions {
        pack_target_size: 48 << 10,
        index_flush_packs: 2,
        ..Default::default()
    };
    let outcome = writer
        .backup(&[src.to_path_buf()], options, &|_| {})
        .await?;
    Ok(outcome.snapshot_id)
}

async fn restore_with<S: BlobStore>(store: &S, id: &BlobId, target: &Path) {
    let repo = Repository::open(store, b"pw").await.unwrap();
    let mut reader = Reader::open(store, repo.read_keys().duplicate(), repo.config()).unwrap();
    let report = reader
        .restore(id, target, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
}

/// Every pack an index shard names is stored whole under its own name.
async fn assert_indexed_packs_whole<S: BlobStore>(store: &S) {
    let repo = Repository::open(store, b"pw").await.unwrap();
    let read = repo.read_keys();
    let mut packs = 0;
    for shard in store.list("index/").await.unwrap() {
        let shard_id = parse_hex32(shard.key.strip_prefix("index/").unwrap()).unwrap();
        let bytes = store.get(&shard.key).await.unwrap();
        let opened = open_shard(read.k_idx(), read.repo_id(), &shard_id, &bytes).unwrap();
        for pack in opened.packs {
            let pack_id: BlobId = pack.pack_id.as_slice().try_into().unwrap();
            let stored = store.get(&pack_object_key(&pack_id)).await.unwrap();
            assert_eq!(stored.len() as u64, pack.pack_size);
            assert_eq!(blake3::hash(&stored).as_bytes(), &pack_id);
            packs += 1;
        }
    }
    assert!(packs > 0);
}

fn tear_strategy() -> impl Strategy<Value = Tear> {
    prop_oneof![
        (0usize..100_000).prop_map(Tear::Short),
        (0usize..100_000, 1usize..64).prop_map(|(start, len)| Tear::Hole(start..start + len)),
    ]
}

/// Tears in runs of at most `MAX_RESEALS`, each run ended by an upload that
/// goes through, so no pack is torn more often than the writer retries.
fn plan_strategy() -> impl Strategy<Value = Vec<Option<Tear>>> {
    prop::collection::vec(
        prop::collection::vec(tear_strategy(), 0..=MAX_RESEALS),
        0..6,
    )
    .prop_map(|runs| {
        runs.into_iter()
            .flat_map(|run| run.into_iter().map(Some).chain(std::iter::once(None)))
            .collect()
    })
}

fn run<T>(f: impl Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

async fn torn_matches_local(files: Vec<(usize, u8)>, plan: Vec<Option<Tear>>) {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    for (i, (len, seed)) in files.iter().enumerate() {
        fs::write(src.join(format!("f{i}.bin")), random_bytes(*len, *seed)).unwrap();
    }

    let torn_store = TornStore::new(init_repo(&tmp.path().join("torn")).await, plan);
    let local = init_repo(&tmp.path().join("local")).await;

    let torn_id = backup_with(&torn_store, &src).await.unwrap();
    let local_id = backup_with(&local, &src).await.unwrap();
    assert_indexed_packs_whole(&torn_store).await;

    let (torn_out, local_out) = (tmp.path().join("out-torn"), tmp.path().join("out-local"));
    restore_with(&torn_store, &torn_id, &torn_out).await;
    restore_with(&local, &local_id, &local_out).await;
    assert_same_files(
        &restored_path(&torn_out, &src),
        &restored_path(&local_out, &src),
    );
    assert_same_files(&src, &restored_path(&torn_out, &src));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn put_named_size_torn_store_matches_local_fs(
        files in prop::collection::vec((1usize..150_000, any::<u8>()), 1..5),
        plan in plan_strategy(),
    ) {
        run(torn_matches_local(files, plan));
    }
}

#[tokio::test]
async fn put_named_size_too_many_tears_fails_without_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let src = small_tree(tmp.path());
    let plan = (0..=MAX_RESEALS).map(|_| Some(Tear::Short(10)));
    let store = TornStore::new(init_repo(&tmp.path().join("repo")).await, plan);

    let err = backup_with(&store, &src).await.unwrap_err();
    assert!(matches!(err, Error::NameBurned { .. }), "{err:?}");
    assert_eq!(store.burned().len(), MAX_RESEALS + 1);
    assert!(store.list("snapshots/").await.unwrap().is_empty());
}
