use std::fs;
use std::path::Path;

use bytes::Bytes;
use lodestore_format::crypto::Entropy;
use lodestore_format::keys::KdfCost;
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::store::{BlobDelete, BlobError, BlobStore, S3Store, StaticCredentials};
use lodestore_format::writer::{BackupOptions, BackupOutcome, Writer};
use lodestore_testkit::minio::MinioServer;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{Rng, SeedableRng};

const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

const BUCKET: &str = "write-only";

fn open_store(minio: &MinioServer, access_key_id: String, secret_access_key: String) -> S3Store {
    let creds = StaticCredentials {
        access_key_id,
        secret_access_key,
    };
    S3Store::open(&format!("s3:{}/{BUCKET}", minio.endpoint()), Some(creds)).unwrap()
}

fn make_tree(root: &Path, seed: u8) {
    let mut rng = ChaCha20Rng::from_seed([seed; 32]);
    let mut big = vec![0u8; 6 << 20];
    rng.fill_bytes(&mut big);
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("big.bin"), &big).unwrap();
    fs::write(root.join("sub/small.txt"), format!("hello {seed}")).unwrap();
}

fn assert_same_tree(a: &Path, b: &Path) {
    for rel in ["big.bin", "sub/small.txt"] {
        assert!(
            fs::read(a.join(rel)).unwrap() == fs::read(b.join(rel)).unwrap(),
            "{rel} differs"
        );
    }
}

struct Fixture {
    _minio: MinioServer,
    root: S3Store,
    writer_store: S3Store,
}

/// A bucket initialised with the root credentials, one snapshot already in it,
/// and a second store for a user that may only add objects.
async fn fixture() -> (Fixture, tempfile::TempDir) {
    let minio = MinioServer::start().await;
    minio.create_bucket(BUCKET).await;
    let (key, secret) = minio.root_credentials();
    let root = open_store(&minio, key, secret);
    let mut entropy = Entropy::os();
    let options = InitOptions {
        kdf: LOW_KDF,
        ..Default::default()
    };
    PendingInit::generate(&mut entropy, options)
        .unwrap()
        .commit(&root, b"pw", &mut entropy)
        .await
        .unwrap();
    let (key, secret) = minio.create_write_only_user(BUCKET).await;
    let writer_store = open_store(&minio, key, secret);

    let src = tempfile::tempdir().unwrap();
    make_tree(src.path(), 1);
    backup_as_writer(&root, src.path()).await;
    (
        Fixture {
            _minio: minio,
            root,
            writer_store,
        },
        src,
    )
}

async fn backup_as_writer(store: &S3Store, src: &Path) -> BackupOutcome {
    let repo = Repository::open(store, b"pw").await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    let mut writer = Writer::new(store, keys, repo.config()).await.unwrap();
    let outcome = writer
        .backup(&[src.to_path_buf()], BackupOptions::default(), &|_| {})
        .await
        .unwrap();
    assert!(!outcome.incomplete);
    outcome
}

async fn pack_count(store: &S3Store) -> usize {
    store.list("packs/").await.unwrap().len()
}

#[tokio::test]
async fn s3_write_only_user_backup_ok() {
    let (fx, _first_src) = fixture().await;
    let repo = Repository::open(&fx.root, b"pw").await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());

    let src = tempfile::tempdir().unwrap();
    make_tree(src.path(), 2);
    let mut writer = Writer::new(&fx.writer_store, keys, repo.config())
        .await
        .unwrap();
    let outcome = writer
        .backup(
            &[src.path().to_path_buf()],
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert!(!outcome.incomplete);

    let mut reader = Reader::open(&fx.root, repo.read_keys().duplicate(), repo.config()).unwrap();
    let out = tempfile::tempdir().unwrap();
    reader
        .restore(
            &outcome.snapshot_id,
            out.path(),
            &RestoreOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    let canonical = src.path().canonicalize().unwrap();
    assert_same_tree(
        src.path(),
        &out.path().join(canonical.strip_prefix("/").unwrap()),
    );
}

#[tokio::test]
async fn s3_write_only_rebackup_dedups() {
    let (fx, first_src) = fixture().await;
    let repo = Repository::open(&fx.root, b"pw").await.unwrap();
    let packs = pack_count(&fx.root).await;
    assert!(packs > 0);

    let keys = repo.read_keys().write_keys(repo.config_hash());
    let mut writer = Writer::new(&fx.writer_store, keys, repo.config())
        .await
        .unwrap();
    let outcome = writer
        .backup(
            &[first_src.path().to_path_buf()],
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert_eq!(outcome.stats.bytes_added, 0);
    assert_eq!(pack_count(&fx.root).await, packs);
}

#[tokio::test]
async fn s3_write_only_cannot_overwrite() {
    let (fx, _src) = fixture().await;
    let pack = fx.root.list("packs/").await.unwrap().remove(0).key;
    let original = fx.root.get(&pack).await.unwrap();

    let err = fx
        .writer_store
        .put_if_absent(&pack, Bytes::from_static(b"tampered"))
        .await
        .unwrap_err();
    assert!(matches!(err, BlobError::AlreadyExists), "{err:?}");
    assert_eq!(fx.root.get(&pack).await.unwrap(), original);
}

#[tokio::test]
async fn s3_write_only_cannot_delete() {
    let (fx, _src) = fixture().await;
    let pack = fx.root.list("packs/").await.unwrap().remove(0).key;

    let err = fx.writer_store.delete(&pack).await.unwrap_err();
    let BlobError::Backend { message, .. } = &err else {
        panic!("expected a backend error, got {err:?}");
    };
    assert!(
        message.contains("AccessDenied") || message.contains("PermissionDenied"),
        "{message}"
    );
    assert!(fx.root.get(&pack).await.is_ok());
}

#[tokio::test]
async fn s3_write_only_cannot_read_packs() {
    let (fx, _src) = fixture().await;
    let pack = fx.root.list("packs/").await.unwrap().remove(0).key;

    assert!(matches!(
        fx.writer_store.get(&pack).await,
        Err(BlobError::Backend { .. })
    ));
    assert!(matches!(
        fx.writer_store.get_range(&pack, 0..16).await,
        Err(BlobError::Backend { .. })
    ));
    assert!(matches!(
        fx.writer_store.list("packs/").await,
        Err(BlobError::Backend { .. })
    ));
}
