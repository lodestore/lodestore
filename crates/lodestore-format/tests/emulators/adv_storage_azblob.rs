//! A compromised agent against Azure Blob Storage on Azurite. `adv_client_*`
//! read the request journal of a proxy in front of the emulator;
//! `adv_service_*` attack with the agent's own SAS.

use std::collections::BTreeMap;
use std::time::Duration;

use bytes::Bytes;
use lodestore_format::store::{BlobStore, RepoLocation, Store};
use lodestore_testkit::azurite::AzuriteServer;
use lodestore_testkit::fault_proxy::{FaultAction, FaultPlan, FaultProxy, FaultRule};

use super::azblob::account_store;

async fn proxied_agent(
    az: &AzuriteServer,
    container: &str,
    plan: FaultPlan,
) -> (FaultProxy, Store) {
    let upstream = az.endpoint();
    let base = upstream
        .trim_end_matches(&format!("/{}", az.account_name()))
        .to_owned();
    let proxy = FaultProxy::start(&base, plan).await;
    let endpoint = format!("{}/{}", proxy.endpoint(), az.account_name());
    let sas = az.container_sas(container, "rcl", Duration::from_secs(3600));
    let agent = RepoLocation::new(
        "azblob",
        [
            ("container", container),
            ("endpoint", endpoint.as_str()),
            ("sas_token", sas.as_str()),
        ],
    )
    .open(None)
    .unwrap();
    (proxy, agent)
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn adv_client_azblob_backup_sends_only_conditional_puts() {
    let az = AzuriteServer::start().await;
    az.create_container("journal").await;
    let server = account_store(&az, "journal").await;
    super::init_repo(&server).await;
    let (proxy, agent) = proxied_agent(&az, "journal", FaultPlan::none()).await;

    let src = tempfile::tempdir().unwrap();
    super::make_tree(src.path(), 3);
    let outcome = super::backup(&agent, src.path()).await;
    super::restore_and_compare(&server, &outcome, src.path()).await;

    let requests = proxy.requests();
    let puts: Vec<_> = requests.iter().filter(|r| r.method == "PUT").collect();
    assert!(puts.len() >= 3, "{requests:?}");
    for r in &requests {
        if matches!(r.method.as_str(), "GET" | "HEAD") {
            continue;
        }
        assert_eq!(r.method, "PUT", "the agent sent {r:?}");
        assert!(
            r.headers
                .iter()
                .any(|(k, v)| k == "if-none-match" && v == "*"),
            "unconditional write: {r:?}"
        );
        let query = r.query.as_deref().unwrap_or("");
        assert!(
            !query.split('&').any(|p| p.starts_with("comp=")),
            "block or lease operation: {r:?}"
        );
    }
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn adv_client_azblob_lost_response_retry_succeeds() {
    let az = AzuriteServer::start().await;
    az.create_container("lost").await;
    let server = account_store(&az, "lost").await;
    super::init_repo(&server).await;
    let plan = FaultPlan::none().with(FaultRule {
        method: "PUT",
        key_prefix: "lost/packs/".into(),
        action: FaultAction::ForwardThenFail,
        times: 1,
    });
    let (proxy, agent) = proxied_agent(&az, "lost", plan).await;

    let src = tempfile::tempdir().unwrap();
    super::make_tree(src.path(), 3);
    let outcome = super::backup(&agent, src.path()).await;
    super::restore_and_compare(&server, &outcome, src.path()).await;

    let mut per_path: BTreeMap<String, usize> = BTreeMap::new();
    for r in proxy.requests().iter().filter(|r| r.method == "PUT") {
        *per_path.entry(r.path.clone()).or_default() += 1;
    }
    assert!(
        per_path.values().any(|n| *n >= 2),
        "no write was repeated: {per_path:?}"
    );
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn adv_service_azblob_rcl_sas_refuses_overwrite_delete_and_blocks() {
    let az = AzuriteServer::start().await;
    az.create_container("attack").await;
    let owner = account_store(&az, "attack").await;
    let sas = az.container_sas("attack", "rcl", Duration::from_secs(3600));
    let created = az
        .sas_request("PUT", "attack", "packs/victim", &sas, "", b"original")
        .await;
    assert_eq!(created.as_u16(), 201);

    let overwrite = az
        .sas_request("PUT", "attack", "packs/victim", &sas, "", b"forged")
        .await;
    assert_eq!(overwrite.as_u16(), 403, "unconditional overwrite");
    let delete = az
        .sas_request("DELETE", "attack", "packs/victim", &sas, "", b"")
        .await;
    assert_eq!(delete.as_u16(), 403, "delete");
    let block = az
        .sas_request(
            "PUT",
            "attack",
            "packs/victim",
            &sas,
            "comp=block&blockid=QUFBQQ%3D%3D",
            b"forged",
        )
        .await;
    assert_eq!(block.as_u16(), 403, "put block");
    let list = az
        .sas_request("PUT", "attack", "packs/victim", &sas, "comp=blocklist", b"")
        .await;
    assert_eq!(list.as_u16(), 403, "put block list");

    assert_eq!(
        owner.get("packs/victim").await.unwrap(),
        Bytes::from_static(b"original")
    );
}
