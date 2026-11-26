use std::time::Duration;

use bytes::Bytes;
use lodestore_format::Error;
use lodestore_format::store::{BlobError, BlobStore, RepoLocation, Store, put_named};
use lodestore_testkit::azurite::AzuriteServer;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{Rng, SeedableRng};

pub(super) async fn account_store(az: &AzuriteServer, container: &str) -> Store {
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

pub(super) fn sas_store(az: &AzuriteServer, container: &str, permissions: &str) -> Store {
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
async fn azblob_rcl_sas_repeat_of_other_bytes_of_the_same_length_fails() {
    let az = AzuriteServer::start().await;
    az.create_container("samelen").await;
    let server = account_store(&az, "samelen").await;
    let agent = sas_store(&az, "samelen", "rcl");
    let data = b"a pack the server already holds".to_vec();
    let hex = blake3::hash(&data).to_hex().to_string();
    let key = format!("packs/{}/{hex}", &hex[..2]);
    server
        .put_if_absent(&key, Bytes::from(data.clone()))
        .await
        .unwrap();

    put_named(&agent, &key, data.clone()).await.unwrap();

    let mut other = data.clone();
    other[0] ^= 1;
    let err = put_named(&agent, &key, other).await.unwrap_err();
    assert!(matches!(err, Error::NameBurned { .. }), "{err:?}");
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

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn azblob_rcl_retry_after_lost_response_is_ok() {
    let az = AzuriteServer::start().await;
    az.create_container("retry").await;
    let agent = sas_store(&az, "retry", "rcl");
    let bytes = Bytes::from(vec![7u8; 4096]);
    lodestore_format::store::put_named_bytes(&agent, "packs/p1", &bytes)
        .await
        .unwrap();
    let raw = agent.put_if_absent("packs/p1", bytes.clone()).await;
    assert!(matches!(raw, Err(BlobError::Denied { .. })), "{raw:?}");
    lodestore_format::store::put_named_bytes(&agent, "packs/p1", &bytes)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn azblob_rcl_retry_with_short_object_is_name_burned() {
    let az = AzuriteServer::start().await;
    az.create_container("burned").await;
    let owner = account_store(&az, "burned").await;
    owner
        .put_if_absent("packs/p2", Bytes::from(vec![1u8; 100]))
        .await
        .unwrap();
    let agent = sas_store(&az, "burned", "rcl");
    let err = put_named(&agent, "packs/p2", vec![1u8; 4096])
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NameBurned { .. }), "{err:?}");
}
