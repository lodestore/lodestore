//! The `locks/` protocol seen from a writer: published candidate packs are not
//! trusted for deduplication, unreadable manifests stop the writer.

use std::fs;
use std::ops::Range;
use std::path::Path;

use bytes::Bytes;
use lodestore_format::Error;
use lodestore_format::crypto::{BlobId, Entropy};
use lodestore_format::index::parse_hex32;
use lodestore_format::keys::KdfCost;
use lodestore_format::locks::{
    close_writer_marker, gc_manifest_key, new_lock_id, open_gc_manifest, publish_gc_manifest,
    put_writer_marker, read_gc_locks, read_writer_locks, seal_gc_manifest, writer_marker_key,
};
use lodestore_format::pb;
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

struct Fixture {
    _tmp: tempfile::TempDir,
    store: LocalFsStore,
    src: std::path::PathBuf,
    repo: Repository,
}

async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    let mut data = vec![0u8; 2 << 20];
    ChaCha20Rng::from_seed([3; 32]).fill_bytes(&mut data);
    fs::write(src.join("random.bin"), &data).unwrap();
    let store = LocalFsStore::new(tmp.path().join("repo"));
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
    Fixture {
        _tmp: tmp,
        store,
        src,
        repo,
    }
}

impl Fixture {
    async fn writer(&self) -> Writer<&LocalFsStore> {
        let keys = self.repo.read_keys().write_keys(self.repo.config_hash());
        Writer::connect(&self.store, keys).await.unwrap()
    }

    async fn backup(&self) -> BackupOutcome {
        self.writer()
            .await
            .backup(
                std::slice::from_ref(&self.src),
                BackupOptions::default(),
                &|_| {},
            )
            .await
            .unwrap()
    }

    async fn packs(&self) -> Vec<BlobId> {
        self.store
            .list("packs/")
            .await
            .unwrap()
            .iter()
            .map(|e| parse_hex32(e.key.rsplit('/').next().unwrap()).unwrap())
            .collect()
    }

    async fn publish(&self, packs: &[BlobId]) {
        let keys = self.repo.read_keys();
        let id = new_lock_id(&mut Entropy::os());
        publish_gc_manifest(
            &self.store,
            keys.k_idx(),
            keys.repo_id(),
            &id,
            packs,
            &mut Entropy::os(),
        )
        .await
        .unwrap();
    }

    async fn restores(&self, outcome: &BackupOutcome) -> bool {
        let mut r = Reader::open(
            &self.store,
            self.repo.read_keys().duplicate(),
            self.repo.config(),
        )
        .unwrap();
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
            .join(self.src.canonicalize().unwrap().strip_prefix("/").unwrap());
        same(&self.src, &restored, "random.bin")
    }

    async fn connect_err(&self) -> Option<Error> {
        let keys = self.repo.read_keys().write_keys(self.repo.config_hash());
        Writer::connect(&self.store, keys).await.err()
    }
}

fn same(a: &Path, b: &Path, rel: &str) -> bool {
    fs::read(a.join(rel)).unwrap() == fs::read(b.join(rel)).unwrap()
}

fn garbage() -> Bytes {
    Bytes::from_static(b"not a manifest at all, but long enough to be an AEAD body.....")
}

struct DenyLocks<S>(S);

impl<S: BlobStore> BlobStore for DenyLocks<S> {
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
        if prefix.starts_with("locks/") {
            return Err(BlobError::Denied {
                message: "AccessDenied".into(),
            });
        }
        self.0.list(prefix).await
    }
}

#[tokio::test]
async fn gc_locks_writer_excludes_candidates() {
    let f = fixture().await;
    let first = f.backup().await;
    assert!(first.stats.bytes_added > 0);
    let control = f.backup().await;
    assert_eq!(control.stats.bytes_added, 0, "unmarked packs are reused");

    let before = f.packs().await;
    f.publish(&before).await;
    let second = f.backup().await;
    assert!(
        second.stats.bytes_added > 0,
        "marked packs are uploaded again"
    );
    assert!(f.packs().await.len() > before.len());
    assert!(f.restores(&second).await);
}

#[tokio::test]
async fn gc_locks_second_location_reused() {
    let f = fixture().await;
    f.backup().await;
    let original = f.packs().await;
    f.publish(&original).await;
    f.backup().await;
    let both = f.packs().await;
    assert!(both.len() > original.len());

    let third = f.backup().await;
    assert_eq!(third.stats.bytes_added, 0);
    assert_eq!(f.packs().await.len(), both.len());
    assert!(f.restores(&third).await);
}

#[tokio::test]
async fn gc_locks_corrupt_manifest_fails_closed() {
    let f = fixture().await;
    f.backup().await;
    let id = new_lock_id(&mut Entropy::os());
    f.store
        .put_if_absent(&gc_manifest_key(&id), garbage())
        .await
        .unwrap();
    assert!(matches!(f.connect_err().await, Some(Error::Corrupt { .. })));
}

#[tokio::test]
async fn gc_locks_mismatched_id_fails_closed() {
    let f = fixture().await;
    let keys = f.repo.read_keys();
    let (named, inside) = (
        new_lock_id(&mut Entropy::os()),
        new_lock_id(&mut Entropy::os()),
    );
    let manifest = pb::GcManifest {
        version: 1,
        gc_id: inside.to_vec(),
        published: None,
        candidate_packs: vec![vec![1; 32]],
    };
    let bytes =
        seal_gc_manifest(keys.k_idx(), keys.repo_id(), &manifest, &mut Entropy::os()).unwrap();
    f.store
        .put_if_absent(&gc_manifest_key(&named), Bytes::from(bytes))
        .await
        .unwrap();
    assert!(matches!(f.connect_err().await, Some(Error::Corrupt { .. })));
}

#[tokio::test]
async fn gc_locks_unrelated_objects_ignored() {
    let f = fixture().await;
    f.store
        .put_if_absent("locks/probe-x", garbage())
        .await
        .unwrap();
    let keys = f.repo.read_keys();
    put_writer_marker(
        &f.store,
        keys.k_idx(),
        keys.repo_id(),
        b"h",
        &mut Entropy::os(),
    )
    .await
    .unwrap();
    let w = f.writer().await;
    assert!(!w.gc_locks_denied());
}

#[tokio::test]
async fn gc_locks_denied_is_a_flag() {
    let f = fixture().await;
    let keys = f.repo.read_keys().write_keys(f.repo.config_hash());
    let w = Writer::connect(DenyLocks(&f.store), keys).await.unwrap();
    assert!(w.gc_locks_denied());
}

#[test]
fn gc_locks_roundtrip() {
    let (k, repo) = ([7u8; 32], [8u8; 16]);
    let id = [5u8; 16];
    let manifest = pb::GcManifest {
        version: 1,
        gc_id: id.to_vec(),
        published: Some(pb::Timestamp {
            seconds: 1_700_000_000,
            nanos: 0,
        }),
        candidate_packs: vec![vec![1; 32], vec![2; 32]],
    };
    let bytes = seal_gc_manifest(&k, &repo, &manifest, &mut Entropy::os()).unwrap();
    let key = gc_manifest_key(&id);
    assert_eq!(open_gc_manifest(&k, &repo, &key, &bytes).unwrap(), manifest);
    assert!(open_gc_manifest(&[9; 32], &repo, &key, &bytes).is_err());
    assert!(open_gc_manifest(&k, &[9; 16], &key, &bytes).is_err());
}

#[tokio::test]
async fn gc_locks_publish_is_idempotent_per_id() {
    let f = fixture().await;
    let keys = f.repo.read_keys();
    let id = new_lock_id(&mut Entropy::os());
    for _ in 0..2 {
        publish_gc_manifest(
            &f.store,
            keys.k_idx(),
            keys.repo_id(),
            &id,
            &[[1; 32]],
            &mut Entropy::os(),
        )
        .await
        .unwrap();
    }
    let locks = read_gc_locks(&f.store, keys.k_idx(), keys.repo_id())
        .await
        .unwrap();
    assert_eq!(locks.manifests.len(), 1);
    assert_eq!(locks.manifests[0].gc_id, id);
    assert!(locks.candidate_packs().contains(&[1; 32]));
}

#[tokio::test]
async fn gc_locks_writer_marker_lifecycle() {
    let f = fixture().await;
    let keys = f.repo.read_keys();
    let id = put_writer_marker(
        &f.store,
        keys.k_idx(),
        keys.repo_id(),
        b"host-1",
        &mut Entropy::os(),
    )
    .await
    .unwrap();

    let open = read_writer_locks(&f.store, keys.k_idx(), keys.repo_id())
        .await
        .unwrap();
    assert_eq!(open.len(), 1);
    assert!(!open[0].done);
    assert_eq!(open[0].id, id);
    assert_eq!(open[0].marker.as_ref().unwrap().hostname, b"host-1");
    assert!(open[0].modified_ms.is_some());

    close_writer_marker(&f.store, &id).await.unwrap();
    close_writer_marker(&f.store, &id).await.unwrap();
    let closed = read_writer_locks(&f.store, keys.k_idx(), keys.repo_id())
        .await
        .unwrap();
    assert_eq!(closed.len(), 1);
    assert!(closed[0].done);
}

#[tokio::test]
async fn gc_locks_unreadable_marker_stays_open() {
    let f = fixture().await;
    let keys = f.repo.read_keys();
    let id = new_lock_id(&mut Entropy::os());
    f.store
        .put_if_absent(&writer_marker_key(&id), garbage())
        .await
        .unwrap();
    let locks = read_writer_locks(&f.store, keys.k_idx(), keys.repo_id())
        .await
        .unwrap();
    assert_eq!(locks.len(), 1);
    assert!(locks[0].marker.is_none());
    assert!(!locks[0].done);
}
