mod adv_support;
mod common;
#[path = "../../lodestore-format/tests/adv_support/mod.rs"]
mod planted;

use common::PASSWORD;
use lodestore_format::attest::{MARKER_LEGACY_SHARDS, ensure_upgraded, read_attestations};
use lodestore_format::chunker::blob_id;
use lodestore_format::crypto::{BLOB_TYPE_DATA, Entropy};
use lodestore_format::keys::KdfCost;
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::store::{BlobStore, LocalFsStore};

const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

/// Writes `tests/adv_support/upgraded-repo`. The committed files are frozen:
/// regenerating changes every key and id in them.
#[tokio::test]
#[ignore = "writes the committed fixture"]
async fn write_upgraded_fixture() {
    let dir = adv_support::fixture_dir();
    assert!(!dir.exists(), "{} already exists", dir.display());
    std::fs::create_dir_all(&dir).unwrap();
    let store = LocalFsStore::new(dir);
    let mut entropy = Entropy::os();
    PendingInit::generate(
        &mut entropy,
        InitOptions {
            kdf: LOW_KDF,
            ..InitOptions::default()
        },
    )
    .unwrap()
    .commit(&store, PASSWORD.as_bytes(), &mut entropy)
    .await
    .unwrap();
    let repo = Repository::open(&store, PASSWORD.as_bytes()).await.unwrap();
    let attacker = planted::attacker_keys(&repo);
    let lying = blob_id(&attacker.k_id, &adv_support::lying_content());
    planted::plant_blobs(&store, &attacker, &[(lying, BLOB_TYPE_DATA)]).await;
    ensure_upgraded(&store, repo.read_keys(), &mut entropy)
        .await
        .unwrap();
}

#[tokio::test]
async fn upgraded_fixture_is_upgraded() {
    let tmp = tempfile::tempdir().unwrap();
    let path = adv_support::upgraded_repo(tmp.path());
    let store = LocalFsStore::new(path);
    let repo = Repository::open(&store, PASSWORD.as_bytes()).await.unwrap();
    let attested = read_attestations(&store, repo.read_keys()).await.unwrap();
    assert!(attested.markers.contains(&MARKER_LEGACY_SHARDS));

    let keys = planted::attacker_keys(&repo);
    let shards = planted::open_all_shards(&store, &keys).await;
    assert_eq!(shards.len(), 1);
    assert!(attested.shards.contains(&shards[0].0));
    let lying = blob_id(&keys.k_id, &adv_support::lying_content());
    assert!(shards[0].1.packs[0].entries.starts_with(&lying));
    assert!(store.list("snapshots/").await.unwrap().is_empty());
}
