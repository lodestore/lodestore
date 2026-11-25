use std::time::SystemTime;

use bytes::Bytes;
use lodestore_format::store::azure_sas::{sas_https_only, sign_container_sas};
use lodestore_format::store::probe::{AgentProbe, GuaranteeLevel, ProbeKeys, probe_keep};
use lodestore_format::store::{BlobStore, RepoLocation, Store, put_named_bytes};
use lodestore_testkit::azurite::AzuriteServer;

use super::azblob::account_store;

fn vended_store(az: &AzuriteServer, container: &str, token: &str) -> Store {
    RepoLocation::new(
        "azblob",
        [
            ("container", container),
            ("endpoint", az.endpoint().as_str()),
            ("sas_token", token),
        ],
    )
    .open(None)
    .unwrap()
}

fn vended(az: &AzuriteServer, container: &str, now: SystemTime, https_only: bool) -> String {
    sign_container_sas(
        az.account_name(),
        az.account_key(),
        container,
        now,
        https_only,
    )
    .unwrap()
    .token
    .to_string()
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn azblob_vended_sas_probe_is_create_only() {
    let az = AzuriteServer::start().await;
    az.create_container("vended").await;
    let server = account_store(&az, "vended").await;
    let token = vended(
        &az,
        "vended",
        SystemTime::now(),
        sas_https_only(&az.endpoint()),
    );
    let agent = vended_store(&az, "vended", &token);

    let report = probe_keep(&server, AgentProbe::Store(&agent), &ProbeKeys::default()).await;
    assert_eq!(
        report.guarantee,
        GuaranteeLevel::CreateOnlyEnforced,
        "{report:?}"
    );

    put_named_bytes(&agent, "packs/fresh", &Bytes::from(vec![9u8; 2048]))
        .await
        .unwrap();
    assert_eq!(agent.object_size("packs/fresh").await.unwrap(), Some(2048));
}
