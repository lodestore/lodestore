use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use bytes::Bytes;
use lodestore_format::crypto::Entropy;
use lodestore_format::index::{open_shard, parse_hex32};
use lodestore_format::keys::KdfCost;
use lodestore_format::pack::pack_object_key;
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::store::{BlobError, BlobStore, S3Store, StaticCredentials};
use lodestore_format::writer::{BackupOptions, BackupOutcome, Writer};
use lodestore_testkit::fault_proxy::{FaultAction, FaultPlan, FaultProxy, FaultRule};
use lodestore_testkit::minio::MinioServer;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{Rng, SeedableRng};

const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

fn creds(minio: &MinioServer) -> StaticCredentials {
    let (access_key_id, secret_access_key) = minio.root_credentials();
    StaticCredentials {
        access_key_id,
        secret_access_key,
    }
}

fn open_store(endpoint: &str, bucket: &str, creds: StaticCredentials) -> S3Store {
    S3Store::open(&format!("s3:{endpoint}/{bucket}"), Some(creds)).unwrap()
}

async fn init_repo(store: &S3Store) {
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

async fn backup(store: &S3Store, src: &Path) -> BackupOutcome {
    let repo = Repository::open(store, b"pw").await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    let mut w = Writer::new(store, keys, repo.config()).await.unwrap();
    let outcome = w
        .backup(&[src.to_path_buf()], BackupOptions::default(), &|_| {})
        .await
        .unwrap();
    assert!(!outcome.incomplete);
    outcome
}

async fn restore_and_compare<S: BlobStore>(store: S, outcome: &BackupOutcome, src: &Path) {
    let repo = Repository::open(&store, b"pw").await.unwrap();
    let mut r = Reader::open(&store, repo.read_keys().duplicate(), repo.config()).unwrap();
    let out = tempfile::tempdir().unwrap();
    let report = r
        .restore(
            &outcome.snapshot_id,
            out.path(),
            &RestoreOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert_eq!(report.files, outcome.stats.files_new);
    let canonical = src.canonicalize().unwrap();
    assert_same_tree(src, &out.path().join(canonical.strip_prefix("/").unwrap()));
}

fn assert_same_tree(a: &Path, b: &Path) {
    let names = |p: &Path| {
        let mut v: Vec<_> = fs::read_dir(p)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        v.sort();
        v
    };
    let listing = names(a);
    assert_eq!(listing, names(b), "entries of {}", a.display());
    for n in listing {
        let (pa, pb) = (a.join(&n), b.join(&n));
        if pa.is_dir() {
            assert_same_tree(&pa, &pb);
        } else {
            assert!(
                fs::read(&pa).unwrap() == fs::read(&pb).unwrap(),
                "{} differs",
                pa.display()
            );
        }
    }
}

/// A small tree with one `big_mib` MiB file of random bytes.
fn make_tree(root: &Path, big_mib: usize) {
    let mut rng = ChaCha20Rng::from_seed([3; 32]);
    let mut big = vec![0u8; big_mib << 20];
    rng.fill_bytes(&mut big);
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("big.bin"), &big).unwrap();
    fs::write(root.join("sub/small.txt"), b"hello s3").unwrap();
}

#[tokio::test]
async fn s3_roundtrip_minio() {
    let minio = MinioServer::start().await;
    minio.create_bucket("roundtrip").await;
    let store = open_store(minio.endpoint(), "roundtrip", creds(&minio));

    let src = tempfile::tempdir().unwrap();
    make_tree(src.path(), 40);

    init_repo(&store).await;
    let outcome = backup(&store, src.path()).await;
    restore_and_compare(&store, &outcome, src.path()).await;
}

#[tokio::test]
async fn store_create_only_s3() {
    let minio = MinioServer::start().await;
    minio.create_bucket("create-only").await;
    let store = open_store(minio.endpoint(), "create-only", creds(&minio));

    store
        .put_if_absent("packs/ab/object", Bytes::from_static(b"first"))
        .await
        .unwrap();
    let second = store
        .put_if_absent("packs/ab/object", Bytes::from_static(b"second"))
        .await;
    assert!(
        matches!(second, Err(BlobError::AlreadyExists)),
        "{second:?}"
    );
    assert_eq!(&store.get("packs/ab/object").await.unwrap()[..], b"first");
    assert_eq!(
        &store.get_range("packs/ab/object", 1..3).await.unwrap()[..],
        b"ir"
    );
    assert!(matches!(
        store.get("packs/ab/missing").await,
        Err(BlobError::NotFound)
    ));
    let listed = store.list("packs/").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (listed[0].key.as_str(), listed[0].size),
        ("packs/ab/object", 5)
    );
}

/// Pack object keys named by the index shards in the bucket.
async fn indexed_pack_keys(store: &S3Store) -> BTreeSet<String> {
    let repo = Repository::open(store, b"pw").await.unwrap();
    let keys = repo.read_keys();
    let mut packs = BTreeSet::new();
    for shard in store.list("index/").await.unwrap() {
        let shard_id = parse_hex32(shard.key.strip_prefix("index/").unwrap()).unwrap();
        let bytes = store.get(&shard.key).await.unwrap();
        let opened = open_shard(keys.k_idx(), keys.repo_id(), &shard_id, &bytes).unwrap();
        for pack in opened.packs {
            packs.insert(pack_object_key(&pack.pack_id.try_into().unwrap()));
        }
    }
    packs
}

fn put_rule(action: FaultAction, times: u32) -> FaultPlan {
    FaultPlan::none().with(FaultRule {
        method: "PUT",
        key_prefix: "packs/".into(),
        action,
        times,
    })
}

/// A bucket behind a fault proxy plus a direct view of the same bucket.
async fn proxied(bucket: &str, plan: FaultPlan) -> (MinioServer, FaultProxy, S3Store, S3Store) {
    let minio = MinioServer::start().await;
    minio.create_bucket(bucket).await;
    let proxy = FaultProxy::start(minio.endpoint(), plan).await;
    let via_proxy = open_store(&proxy.endpoint(), bucket, creds(&minio));
    let direct = open_store(minio.endpoint(), bucket, creds(&minio));
    (minio, proxy, via_proxy, direct)
}

#[tokio::test]
#[ignore = "slow; run with --run-ignored only"]
async fn s3_fault_slowdown_retried() {
    let (_minio, proxy, store, direct) =
        proxied("slowdown", put_rule(FaultAction::SlowDown, 3)).await;
    let src = tempfile::tempdir().unwrap();
    make_tree(src.path(), 6);

    init_repo(&store).await;
    let outcome = backup(&store, src.path()).await;

    let c = proxy.counters();
    assert_eq!(
        c.accepted("PUT", "packs"),
        c.forwarded("PUT", "packs") + 3,
        "three PUTs were refused and retried"
    );
    restore_and_compare(&direct, &outcome, src.path()).await;
}

#[tokio::test]
#[ignore = "slow; run with --run-ignored only"]
async fn s3_fault_ambiguous_put_is_success() {
    let (_minio, proxy, store, direct) =
        proxied("ambiguous", put_rule(FaultAction::ForwardThenFail, 1)).await;
    let src = tempfile::tempdir().unwrap();
    make_tree(src.path(), 6);

    init_repo(&store).await;
    let outcome = backup(&store, src.path()).await;

    let in_bucket: BTreeSet<String> = direct
        .list("packs/")
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.key)
        .collect();
    assert_eq!(in_bucket, indexed_pack_keys(&direct).await);
    assert_eq!(
        proxy.counters().accepted("PUT", "packs"),
        in_bucket.len() as u64 + 1,
        "the PUT whose answer was lost was sent once more and refused"
    );
    restore_and_compare(&direct, &outcome, src.path()).await;
}

#[tokio::test]
#[ignore = "slow; run with --run-ignored only"]
async fn s3_object_lock_version_delete_refused() {
    let minio = MinioServer::start().await;
    minio.create_locked_bucket("locked").await;
    let store = open_store(minio.endpoint(), "locked", creds(&minio));

    let data = Bytes::from_static(b"object under compliance retention");
    let hex = blake3::hash(&data).to_hex().to_string();
    let key = format!("packs/{}/{hex}", &hex[..2]);
    store.put_if_absent(&key, data.clone()).await.unwrap();

    let versions = minio.object_versions("locked", &key).await;
    assert_eq!(versions.len(), 1, "{versions:?}");
    let err = minio
        .delete_version("locked", &key, &versions[0])
        .await
        .expect_err("a locked version must not be deletable");
    assert!(err.contains("WORM"), "{err}");
    assert_eq!(store.get(&key).await.unwrap(), data);
}

#[tokio::test]
#[ignore = "slow; run with --run-ignored only"]
async fn s3_single_put_per_pack() {
    let (_minio, proxy, store, direct) = proxied("single-put", FaultPlan::none()).await;
    let src = tempfile::tempdir().unwrap();
    make_tree(src.path(), 20);

    init_repo(&store).await;
    backup(&store, src.path()).await;

    let c = proxy.counters();
    let packs = direct.list("packs/").await.unwrap().len() as u64;
    assert!(packs >= 1);
    assert_eq!(c.multipart(), 0, "no multipart upload was started");
    assert_eq!(c.forwarded("PUT", "packs"), packs);
}
