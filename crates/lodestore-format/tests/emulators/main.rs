//! Format tests that need the storage emulators (Azurite, fake-gcs-server).
//! They run through `scripts/test-emulators.sh`.

use std::fs;
use std::path::Path;

use lodestore_format::crypto::Entropy;
use lodestore_format::keys::KdfCost;
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::store::{BlobStore, Store};
use lodestore_format::writer::{BackupOptions, BackupOutcome, Writer};
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{Rng, SeedableRng};

mod azblob;
mod gcs;

const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

async fn init_repo(store: &Store) {
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

async fn backup(store: &Store, src: &Path) -> BackupOutcome {
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

async fn restore_and_compare(store: &Store, outcome: &BackupOutcome, src: &Path) {
    let repo = Repository::open(store, b"pw").await.unwrap();
    let mut r = Reader::open(store, repo.read_keys().duplicate(), repo.config()).unwrap();
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
    fs::write(root.join("sub/small.txt"), b"hello emulator").unwrap();
}

async fn full_cycle(store: &Store) {
    let src = tempfile::tempdir().unwrap();
    make_tree(src.path(), 8);
    init_repo(store).await;
    let outcome = backup(store, src.path()).await;
    restore_and_compare(store, &outcome, src.path()).await;
}

async fn put_twice_is_already_exists(store: &Store) {
    use bytes::Bytes;
    use lodestore_format::store::BlobError;

    store
        .put_if_absent("packs/obj", Bytes::from_static(b"first"))
        .await
        .unwrap();
    let again = store
        .put_if_absent("packs/obj", Bytes::from_static(b"second"))
        .await;
    assert!(matches!(again, Err(BlobError::AlreadyExists)), "{again:?}");
    assert_eq!(&store.get("packs/obj").await.unwrap()[..], b"first");
}
