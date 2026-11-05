//! Readers of one repository share an index, pack keys and decrypted trees;
//! what one has read, another does not read again.

use std::fs;
use std::ops::Range;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use lodestore_format::crypto::{BLOB_TYPE_DATA, BLOB_TYPE_TREE, BlobId, Entropy};
use lodestore_format::error::Error;
use lodestore_format::index::{BlobLocation, Index};
use lodestore_format::keys::KdfCost;
use lodestore_format::pack::pack_object_key;
use lodestore_format::pb;
use lodestore_format::reader::{DEFAULT_TREE_BUDGET, Reader, ReaderCache, SelectionTotals};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::selection::Selection;
use lodestore_format::store::{BlobError, BlobStore, ListEntry, LocalFsStore};
use lodestore_format::tree::SelectionWalk;
use lodestore_format::writer::{BackupOptions, Writer};

const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

mod adv_support;

const BIG: usize = 300_000;

/// A `LocalFsStore` that counts the reads of index shards and packs.
#[derive(Clone)]
struct Counting {
    inner: Arc<LocalFsStore>,
    reads: Arc<AtomicUsize>,
}

impl Counting {
    fn new(inner: LocalFsStore) -> Self {
        Counting {
            inner: Arc::new(inner),
            reads: Arc::default(),
        }
    }

    fn count(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }

    fn reset(&self) {
        self.reads.store(0, Ordering::SeqCst);
    }

    fn note(&self, key: &str) {
        if key.starts_with("index/") || key.starts_with("packs/") {
            self.reads.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl BlobStore for Counting {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<(), BlobError> {
        self.inner.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        self.note(key);
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.note(key);
        self.inner.get_range(key, range).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        self.inner.list(prefix).await
    }
}

fn write_file(path: &Path, data: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
}

struct Fx {
    _tmp: tempfile::TempDir,
    repo_dir: PathBuf,
    src: PathBuf,
    snapshot: BlobId,
}

impl Fx {
    async fn new() -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let repo_dir = root.join("repo");
        fs::create_dir(&repo_dir).unwrap();
        let src = root.join("src");
        write_file(&src.join("a/x"), b"content of a/x");
        write_file(&src.join("a/y"), b"content of a/y");
        write_file(&src.join("b/z"), b"content of b/z");
        write_file(&src.join("c/deep/w"), &vec![7u8; BIG]);

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
        let mut writer = Writer::new(&store, keys, repo.config()).await.unwrap();
        let outcome = writer
            .backup(
                std::slice::from_ref(&src),
                BackupOptions::default(),
                &|_| {},
            )
            .await
            .unwrap();
        Fx {
            _tmp: tmp,
            repo_dir,
            src,
            snapshot: outcome.snapshot_id,
        }
    }

    fn store(&self) -> LocalFsStore {
        LocalFsStore::new(self.repo_dir.clone())
    }

    async fn reader<S: BlobStore>(&self, store: S, cache: &Arc<ReaderCache>) -> Reader<S> {
        let local = self.store();
        let repo = Repository::open(&local, b"pw").await.unwrap();
        Reader::open_shared(
            store,
            repo.read_keys().duplicate(),
            repo.config(),
            Arc::clone(cache),
        )
        .unwrap()
    }

    fn abs(&self, rel: &str) -> Vec<u8> {
        let mut p = self.src.as_os_str().as_encoded_bytes().to_vec();
        if !rel.is_empty() {
            p.push(b'/');
            p.extend_from_slice(rel.as_bytes());
        }
        p
    }

    fn selection(&self, rels: &[&str]) -> Selection {
        Selection::from_paths(rels.iter().map(|r| self.abs(r))).unwrap()
    }
}

fn names(nodes: &[pb::Node]) -> Vec<String> {
    nodes
        .iter()
        .map(|n| String::from_utf8_lossy(&n.name).into_owned())
        .collect()
}

#[tokio::test]
async fn reader_shared_second_reader_reads_nothing() {
    let fx = Fx::new().await;
    let cache = ReaderCache::new(DEFAULT_TREE_BUDGET);
    let counting = Counting::new(fx.store());

    let mut first = fx.reader(counting.clone(), &cache).await;
    let top = first.list_dir(&fx.snapshot, b"").await.unwrap();
    let c = first.list_dir(&fx.snapshot, &fx.abs("c")).await.unwrap();
    assert!(counting.count() > 0);

    counting.reset();
    let mut second = fx.reader(counting.clone(), &cache).await;
    assert_eq!(second.list_dir(&fx.snapshot, b"").await.unwrap(), top);
    assert_eq!(
        second.list_dir(&fx.snapshot, &fx.abs("c")).await.unwrap(),
        c
    );
    assert_eq!(counting.count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reader_shared_concurrent_readers() {
    let fx = Fx::new().await;
    let alone = ReaderCache::new(DEFAULT_TREE_BUDGET);
    let mut reader = fx.reader(fx.store(), &alone).await;
    let want_a = reader.list_dir(&fx.snapshot, &fx.abs("a")).await.unwrap();
    let want_deep = reader
        .list_dir(&fx.snapshot, &fx.abs("c/deep"))
        .await
        .unwrap();

    let shared = ReaderCache::new(DEFAULT_TREE_BUDGET);
    let mut tasks = Vec::new();
    for rel in ["a", "c/deep", "a", "c/deep"] {
        let mut reader = fx.reader(Counting::new(fx.store()), &shared).await;
        let (snapshot, path) = (fx.snapshot, fx.abs(rel));
        tasks.push(tokio::spawn(async move {
            let mut last = Vec::new();
            for _ in 0..20 {
                last = reader.list_dir(&snapshot, &path).await.unwrap();
            }
            last
        }));
    }
    let mut got = Vec::new();
    for task in tasks {
        got.push(task.await.unwrap());
    }
    assert_eq!(got[0], want_a);
    assert_eq!(got[1], want_deep);
    assert_eq!(got[2], want_a);
    assert_eq!(got[3], want_deep);
}

#[tokio::test]
async fn reader_shared_tree_budget() {
    let fx = Fx::new().await;
    let counting = Counting::new(fx.store());

    let tiny = ReaderCache::new(1);
    let mut first = fx.reader(counting.clone(), &tiny).await;
    let want = first.list_dir(&fx.snapshot, &fx.abs("c")).await.unwrap();
    counting.reset();
    let mut second = fx.reader(counting.clone(), &tiny).await;
    assert_eq!(
        second.list_dir(&fx.snapshot, &fx.abs("c")).await.unwrap(),
        want
    );
    assert!(counting.count() > 0, "a one byte budget kept a tree");

    let roomy = ReaderCache::new(DEFAULT_TREE_BUDGET);
    let mut first = fx.reader(counting.clone(), &roomy).await;
    first.list_dir(&fx.snapshot, &fx.abs("c")).await.unwrap();
    counting.reset();
    let mut second = fx.reader(counting.clone(), &roomy).await;
    assert_eq!(
        second.list_dir(&fx.snapshot, &fx.abs("c")).await.unwrap(),
        want
    );
    assert_eq!(counting.count(), 0);
}

#[tokio::test]
async fn reader_shared_list_dir() {
    let fx = Fx::new().await;
    let cache = ReaderCache::new(DEFAULT_TREE_BUDGET);
    let mut reader = fx.reader(fx.store(), &cache).await;

    let c = reader.list_dir(&fx.snapshot, &fx.abs("c")).await.unwrap();
    assert_eq!(names(&c), ["deep"]);
    let a = reader.list_dir(&fx.snapshot, &fx.abs("a")).await.unwrap();
    assert_eq!(names(&a), ["x", "y"]);

    let top = reader.list_dir(&fx.snapshot, b"").await.unwrap();
    assert_eq!(reader.list_dir(&fx.snapshot, b"/").await.unwrap(), top);
    let first = fx.src.components().nth(1).unwrap();
    assert_eq!(names(&top)[0], first.as_os_str().to_string_lossy());

    let err = reader
        .list_dir(&fx.snapshot, &fx.abs("a/x"))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)), "{err:?}");
    let err = reader
        .list_dir(&fx.snapshot, &fx.abs("nope"))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)), "{err:?}");
}

#[tokio::test]
async fn reader_shared_selection_totals() {
    let fx = Fx::new().await;
    let cache = ReaderCache::new(DEFAULT_TREE_BUDGET);
    let mut reader = fx.reader(fx.store(), &cache).await;

    let totals = reader
        .selection_totals(&fx.snapshot, &fx.selection(&["a", "c/deep/w"]))
        .await
        .unwrap();
    assert_eq!(
        totals,
        SelectionTotals {
            files: 3,
            dirs: 1,
            bytes: 28 + BIG as u64,
        }
    );
    let all = reader
        .selection_totals(&fx.snapshot, &Selection::all())
        .await
        .unwrap();
    assert_eq!(all.files, 4);

    let local = fx.store();
    let repo = Repository::open(&local, b"pw").await.unwrap();
    let top = reader.list_dir(&fx.snapshot, &fx.abs("")).await.unwrap();
    let a_tree: BlobId = top.iter().find(|n| n.name == b"a").unwrap().subtrees[0]
        .as_slice()
        .try_into()
        .unwrap();
    let mut index = Index::default();
    index
        .load_all(&local, repo.read_keys().k_idx(), repo.read_keys().repo_id())
        .await
        .unwrap();
    let loc = *index.get(&a_tree, BLOB_TYPE_TREE).unwrap();
    let pack = fx.repo_dir.join(pack_object_key(&loc.pack_id));
    let mut bytes = fs::read(&pack).unwrap();
    bytes[loc.offset as usize + loc.enc_len as usize / 2] ^= 1;
    fs::set_permissions(&pack, fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(&pack, bytes).unwrap();

    let fresh = ReaderCache::new(DEFAULT_TREE_BUDGET);
    let mut reader = fx.reader(fx.store(), &fresh).await;
    let err = reader
        .selection_totals(&fx.snapshot, &fx.selection(&["a"]))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Corrupt { .. }), "{err:?}");
}

fn assert_send<T: Send>(_: T) {}

#[tokio::test]
async fn reader_shared_futures_are_send() {
    let fx = Fx::new().await;
    let cache = ReaderCache::new(DEFAULT_TREE_BUDGET);
    let mut reader = fx.reader(fx.store(), &cache).await;
    let selection = Selection::all();
    let id = [0u8; 32];

    assert_send(reader.list_dir(&fx.snapshot, b"/"));
    assert_send(reader.selection_totals(&fx.snapshot, &selection));
    assert_send(reader.read_blob(&id, BLOB_TYPE_DATA));
    assert_send(reader.chunk_locations(&pb::Node::default()));
    assert_send(reader.pack_key(&id));
    assert_send(SelectionWalk::start(&mut reader, &fx.snapshot, &selection));

    let mut walk = SelectionWalk::start(&mut reader, &fx.snapshot, &selection)
        .await
        .unwrap();
    assert_send(walk.next(&mut reader));
}

/// Fails every read of a pack with `Denied` and counts the attempts.
#[derive(Clone)]
struct DenyingPacks {
    inner: Arc<LocalFsStore>,
    attempts: Arc<AtomicUsize>,
}

impl BlobStore for DenyingPacks {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<(), BlobError> {
        self.inner.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, BlobError> {
        if key.starts_with("packs/") {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            return Err(BlobError::Denied {
                message: "no".into(),
            });
        }
        self.inner.get_range(key, range).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        self.inner.list(prefix).await
    }
}

/// The tree blob of `a`, where its frame is, and every record of its pack.
async fn tree_of_a(fx: &Fx) -> (BlobId, BlobLocation, Vec<pb::PackRecord>) {
    let local = fx.store();
    let repo = Repository::open(&local, b"pw").await.unwrap();
    let cache = ReaderCache::new(DEFAULT_TREE_BUDGET);
    let mut reader = fx.reader(&local, &cache).await;
    let top = reader.list_dir(&fx.snapshot, &fx.abs("")).await.unwrap();
    let tree: BlobId = top.iter().find(|n| n.name == b"a").unwrap().subtrees[0]
        .as_slice()
        .try_into()
        .unwrap();
    let mut index = Index::default();
    index
        .load_all(&local, repo.read_keys().k_idx(), repo.read_keys().repo_id())
        .await
        .unwrap();
    let loc = *index.get(&tree, BLOB_TYPE_TREE).unwrap();
    let keys = adv_support::attacker_keys(&repo);
    let records = adv_support::open_all_shards(&local, &keys)
        .await
        .into_iter()
        .flat_map(|(_, shard)| shard.packs)
        .filter(|p| p.pack_id == loc.pack_id)
        .collect();
    (tree, loc, records)
}

#[tokio::test]
async fn reader_shared_read_blob_skips_forged_location() {
    let fx = Fx::new().await;
    let (tree, _, _) = tree_of_a(&fx).await;
    let cache = ReaderCache::new(DEFAULT_TREE_BUDGET);
    let want = fx
        .reader(fx.store(), &cache)
        .await
        .read_blob(&tree, BLOB_TYPE_TREE)
        .await
        .unwrap();

    let local = fx.store();
    let repo = Repository::open(&local, b"pw").await.unwrap();
    let keys = adv_support::attacker_keys(&repo);
    let shards = adv_support::open_all_shards(&local, &keys).await;
    let lowest = shards.iter().map(|(id, _)| *id).min().unwrap();
    let forged = shards
        .iter()
        .flat_map(|(_, s)| s.packs.iter().map(adv_support::forge_offsets))
        .collect();
    adv_support::plant_shard_where(&local, &keys, forged, Vec::new(), |id| *id < lowest).await;

    let fresh = ReaderCache::new(DEFAULT_TREE_BUDGET);
    let mut reader = fx.reader(fx.store(), &fresh).await;
    assert_eq!(reader.read_blob(&tree, BLOB_TYPE_TREE).await.unwrap(), want);
}

#[tokio::test]
async fn reader_shared_read_blob_lists_shards_when_known_copies_fail() {
    let fx = Fx::new().await;
    let (_, loc, records) = tree_of_a(&fx).await;
    let local = fx.store();
    let repo = Repository::open(&local, b"pw").await.unwrap();
    let keys = adv_support::attacker_keys(&repo);

    let copy: BlobId = Entropy::os().bytes();
    let original = fx.repo_dir.join(pack_object_key(&loc.pack_id));
    let bytes = fs::read(&original).unwrap();
    local
        .put_if_absent(&pack_object_key(&copy), Bytes::from(bytes.clone()))
        .await
        .unwrap();
    let moved = records
        .into_iter()
        .map(|mut r| {
            r.pack_id = copy.to_vec();
            r
        })
        .collect();
    adv_support::plant_shard(&local, &keys, moved, Vec::new()).await;

    let mut broken = bytes;
    broken[loc.offset as usize + loc.enc_len as usize / 2] ^= 1;
    fs::set_permissions(&original, fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(&original, broken).unwrap();

    let fresh = ReaderCache::new(DEFAULT_TREE_BUDGET);
    let mut reader = fx.reader(fx.store(), &fresh).await;
    reader
        .selection_totals(&fx.snapshot, &fx.selection(&["a"]))
        .await
        .unwrap();
}

#[tokio::test]
async fn reader_shared_read_blob_store_errors_are_not_retried() {
    let fx = Fx::new().await;
    let (tree, _, records) = tree_of_a(&fx).await;
    let local = fx.store();
    let repo = Repository::open(&local, b"pw").await.unwrap();
    let keys = adv_support::attacker_keys(&repo);
    let forged = records.iter().map(adv_support::forge_offsets).collect();
    adv_support::plant_shard(&local, &keys, forged, Vec::new()).await;

    let denying = DenyingPacks {
        inner: Arc::new(fx.store()),
        attempts: Arc::default(),
    };
    let fresh = ReaderCache::new(DEFAULT_TREE_BUDGET);
    let mut reader = fx.reader(denying.clone(), &fresh).await;
    let err = reader.read_blob(&tree, BLOB_TYPE_TREE).await.unwrap_err();
    assert!(
        matches!(err, Error::Store(BlobError::Denied { .. })),
        "{err:?}"
    );
    assert_eq!(denying.attempts.load(Ordering::SeqCst), 1);
}
