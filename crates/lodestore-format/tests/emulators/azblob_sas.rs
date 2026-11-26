use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use lodestore_format::store::azure_sas::{sas_https_only, sign_container_sas};
use lodestore_format::store::opendal::SasSource;
use lodestore_format::store::probe::{AgentProbe, GuaranteeLevel, ProbeKeys, probe_keep};
use lodestore_format::store::{BlobStore, RepoLocation, Store, put_named_bytes};
use lodestore_testkit::azurite::AzuriteServer;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{Rng, SeedableRng};
use zeroize::Zeroizing;

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

fn now_token(az: &AzuriteServer, container: &str) -> String {
    vended(az, container, SystemTime::now(), false)
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn azblob_vended_sas_repeat_put_is_already_exists() {
    let az = AzuriteServer::start().await;
    az.create_container("repeat").await;
    let agent = vended_store(&az, "repeat", &now_token(&az, "repeat"));
    let bytes = Bytes::from(vec![3u8; 4096]);
    put_named_bytes(&agent, "packs/p", &bytes).await.unwrap();
    put_named_bytes(&agent, "packs/p", &bytes).await.unwrap();
    let longer = Bytes::from(vec![3u8; 8192]);
    assert!(put_named_bytes(&agent, "packs/p", &longer).await.is_err());
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn azblob_vended_sas_refuses_delete() {
    let az = AzuriteServer::start().await;
    az.create_container("nodelete").await;
    let token = now_token(&az, "nodelete");
    let agent = vended_store(&az, "nodelete", &token);
    put_named_bytes(&agent, "packs/d", &Bytes::from(vec![1u8; 64]))
        .await
        .unwrap();
    let status = az
        .sas_request("DELETE", "nodelete", "packs/d", &token, "", b"")
        .await;
    assert_eq!(status.as_u16(), 403);
    assert_eq!(agent.object_size("packs/d").await.unwrap(), Some(64));
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn azblob_vended_sas_refuses_put_block() {
    let az = AzuriteServer::start().await;
    az.create_container("noblock").await;
    let token = now_token(&az, "noblock");
    let status = az
        .sas_request(
            "PUT",
            "noblock",
            "packs/b",
            &token,
            "comp=block&blockid=AAAA",
            b"x",
        )
        .await;
    assert_eq!(status.as_u16(), 403);
    let control = az
        .sas_request("PUT", "noblock", "packs/ok", &token, "", b"x")
        .await;
    assert!(control.is_success(), "{control}");
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn azblob_vended_sas_refuses_a_tampered_signature() {
    let az = AzuriteServer::start().await;
    az.create_container("tamper").await;
    let token = now_token(&az, "tamper");
    let at = token.find("sig=").unwrap() + 4;
    let flipped = if token.as_bytes()[at] == b'A' {
        'B'
    } else {
        'A'
    };
    let tampered = format!("{}{flipped}{}", &token[..at], &token[at + 1..]);
    let status = az
        .sas_request("PUT", "tamper", "packs/t", &tampered, "", b"x")
        .await;
    assert_eq!(status.as_u16(), 403);
    let control = az
        .sas_request("PUT", "tamper", "packs/t", &token, "", b"x")
        .await;
    assert!(control.is_success(), "{control}");
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn azblob_vended_sas_refuses_an_expired_token() {
    let az = AzuriteServer::start().await;
    az.create_container("expired").await;
    let old = SystemTime::now() - Duration::from_secs(2 * 24 * 3600);
    let status = az
        .sas_request(
            "PUT",
            "expired",
            "packs/e",
            &vended(&az, "expired", old, false),
            "",
            b"x",
        )
        .await;
    assert_eq!(status.as_u16(), 403);
    let control = az
        .sas_request(
            "PUT",
            "expired",
            "packs/e",
            &now_token(&az, "expired"),
            "",
            b"x",
        )
        .await;
    assert!(control.is_success(), "{control}");
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn azblob_vended_sas_https_only_is_refused_over_http() {
    let az = AzuriteServer::start().await;
    az.create_container("httpsonly").await;
    let https = vended(&az, "httpsonly", SystemTime::now(), true);
    let status = az
        .sas_request("PUT", "httpsonly", "packs/h", &https, "", b"x")
        .await;
    assert_eq!(status.as_u16(), 403);
    let control = az
        .sas_request(
            "PUT",
            "httpsonly",
            "packs/h",
            &now_token(&az, "httpsonly"),
            "",
            b"x",
        )
        .await;
    assert!(control.is_success(), "{control}");
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn azblob_vended_sas_writes_a_max_size_pack_in_one_put_blob() {
    let az = AzuriteServer::start().await;
    az.create_container("vmaxpack").await;
    let agent = vended_store(&az, "vmaxpack", &now_token(&az, "vmaxpack"));
    let mut data = vec![0u8; lodestore_format::pack::PACK_MAX_SIZE];
    ChaCha20Rng::from_seed([6; 32]).fill_bytes(&mut data);
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

fn refreshing_store(az: &AzuriteServer, container: &str, source: SasSource) -> Store {
    RepoLocation::new(
        "azblob",
        [
            ("container", container),
            ("endpoint", az.endpoint().as_str()),
            ("sas_token", "placeholder"),
        ],
    )
    .open_refreshing(source, Duration::from_secs(1))
    .unwrap()
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn azblob_refreshing_store_picks_up_a_new_sas() {
    let az = AzuriteServer::start().await;
    az.create_container("refresh").await;
    let server = account_store(&az, "refresh").await;
    let good = now_token(&az, "refresh");
    let at = good.find("sig=").unwrap() + 4;
    let flipped = if good.as_bytes()[at] == b'A' {
        'B'
    } else {
        'A'
    };
    let bad = format!("{}{flipped}{}", &good[..at], &good[at + 1..]);

    let current = Arc::new(Mutex::new(Zeroizing::new(good.clone())));
    let asked = Arc::new(AtomicUsize::new(0));
    let source: SasSource = {
        let (current, asked) = (current.clone(), asked.clone());
        Arc::new(move || {
            asked.fetch_add(1, Ordering::SeqCst);
            Some(current.lock().unwrap().clone())
        })
    };
    let agent = refreshing_store(&az, "refresh", source);
    let put = |name: &'static str| {
        let agent = &agent;
        async move {
            put_named_bytes(agent, name, &Bytes::from(vec![5u8; 128]))
                .await
                .is_ok()
        }
    };

    assert!(put("packs/first").await);
    let after_first = asked.load(Ordering::SeqCst);
    assert!(after_first >= 1);

    *current.lock().unwrap() = Zeroizing::new(bad);
    assert!(!put("packs/second").await);
    let after_bad = asked.load(Ordering::SeqCst);
    assert!(after_bad > after_first);

    *current.lock().unwrap() = Zeroizing::new(good);
    assert!(put("packs/third").await);
    assert!(asked.load(Ordering::SeqCst) > after_bad);

    assert_eq!(server.object_size("packs/first").await.unwrap(), Some(128));
    assert_eq!(server.object_size("packs/second").await.unwrap(), None);
    assert_eq!(server.object_size("packs/third").await.unwrap(), Some(128));
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn azblob_refreshing_store_without_a_token_writes_nothing() {
    let az = AzuriteServer::start().await;
    az.create_container("notoken").await;
    let server = account_store(&az, "notoken").await;
    let agent = refreshing_store(&az, "notoken", Arc::new(|| None));
    assert!(
        put_named_bytes(&agent, "packs/n", &Bytes::from(vec![1u8; 64]))
            .await
            .is_err()
    );
    assert_eq!(server.object_size("packs/n").await.unwrap(), None);
}
