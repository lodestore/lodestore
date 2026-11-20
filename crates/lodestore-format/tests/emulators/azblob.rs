use std::time::Duration;

use bytes::Bytes;
use lodestore_format::store::{BlobStore, RepoLocation, Store};
use lodestore_testkit::azurite::AzuriteServer;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{Rng, SeedableRng};

async fn account_store(az: &AzuriteServer, container: &str) -> Store {
    RepoLocation::new(
        "azblob",
        [
            ("container", container),
            ("endpoint", az.endpoint().as_str()),
            ("account_name", az.account_name()),
            ("account_key", az.account_key()),
        ],
    )
    .open(None)
    .unwrap()
}

fn sas_store(az: &AzuriteServer, container: &str, permissions: &str) -> Store {
    let sas = az.container_sas(container, permissions, Duration::from_secs(3600));
    RepoLocation::new(
        "azblob",
        [
            ("container", container),
            ("endpoint", az.endpoint().as_str()),
            ("sas_token", sas.as_str()),
        ],
    )
    .open(None)
    .unwrap()
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn azblob_put_twice_is_already_exists() {
    let az = AzuriteServer::start().await;
    az.create_container("twice").await;
    super::put_twice_is_already_exists(&account_store(&az, "twice").await).await;
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn azblob_backup_and_restore() {
    let az = AzuriteServer::start().await;
    az.create_container("cycle").await;
    super::full_cycle(&account_store(&az, "cycle").await).await;
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn azblob_pack_of_max_size_is_one_put_blob() {
    let az = AzuriteServer::start().await;
    az.create_container("maxpack").await;
    let agent = sas_store(&az, "maxpack", "rcl");
    let mut data = vec![0u8; lodestore_format::pack::PACK_MAX_SIZE];
    ChaCha20Rng::from_seed([5; 32]).fill_bytes(&mut data);
    agent
        .put_if_absent("packs/max", Bytes::from(data.clone()))
        .await
        .unwrap();
    assert_eq!(
        agent.object_size("packs/max").await.unwrap(),
        Some(data.len() as u64)
    );
}
