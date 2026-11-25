//! A compromised agent against Google Cloud Storage on fake-gcs-server.
//! `adv_client_*` read the request journal of a proxy in front of the
//! emulator. fake-gcs-server has no IAM, so protection on the service side
//! cannot be tested on it: `adv_service_gcs_*` exists for a run against the
//! real service and is excluded from `scripts/test-emulators.sh`.

use std::collections::BTreeMap;

use bytes::Bytes;
use lodestore_format::store::{BlobStore, RepoLocation, Store, put_named};
use lodestore_testkit::fault_proxy::{FaultAction, FaultPlan, FaultProxy, FaultRule};
use lodestore_testkit::gcs::FakeGcsServer;
use lodestore_testkit::minio::free_port;

fn gcs_store(endpoint: &str, bucket: &str) -> Store {
    RepoLocation::new(
        "gcs",
        [
            ("bucket", bucket),
            ("endpoint", endpoint),
            ("skip_signature", "true"),
            ("disable_vm_metadata", "true"),
            ("disable_config_load", "true"),
        ],
    )
    .open(None)
    .unwrap()
}

/// The emulator hands out resumable-session URLs on the proxy's address, so
/// every step of an upload passes through the journal.
async fn behind_proxy(bucket: &str, plan: FaultPlan) -> (FakeGcsServer, FaultProxy, Store, Store) {
    let port = free_port();
    let proxy = FaultProxy::start(&format!("http://127.0.0.1:{port}"), plan).await;
    let host = proxy.endpoint().trim_start_matches("http://").to_owned();
    let gcs = FakeGcsServer::start_with_public_host(port, &host).await;
    gcs.create_bucket(bucket).await;
    let server = gcs_store(&gcs.endpoint(), bucket);
    let agent = gcs_store(&proxy.endpoint(), bucket);
    (gcs, proxy, server, agent)
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn adv_client_gcs_backup_sends_only_generation_zero_uploads() {
    let (_gcs, proxy, server, agent) = behind_proxy("journal", FaultPlan::none()).await;
    super::init_repo(&server).await;

    let src = tempfile::tempdir().unwrap();
    super::make_tree(src.path(), 3);
    let outcome = super::backup(&agent, src.path()).await;
    super::restore_and_compare(&server, &outcome, src.path()).await;

    let requests = proxy.requests();
    let mut initiations = 0;
    for r in &requests {
        let query = r.query.as_deref().unwrap_or("");
        match r.method.as_str() {
            "GET" | "HEAD" => {}
            "POST" => {
                assert!(r.path.starts_with("/upload/"), "not an upload: {r:?}");
                assert!(
                    query.split('&').any(|p| p == "ifGenerationMatch=0"),
                    "unconditional upload: {r:?}"
                );
                initiations += 1;
            }
            "PUT" => assert!(
                query.contains("upload_id="),
                "a write outside an upload session: {r:?}"
            ),
            other => panic!("the agent sent {other}: {r:?}"),
        }
    }
    assert!(initiations >= 3, "{requests:?}");
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn adv_client_gcs_lost_response_retry_succeeds() {
    let plan = FaultPlan::none().with(FaultRule {
        method: "POST",
        key_prefix: String::new(),
        action: FaultAction::ForwardThenFail,
        times: 1,
    });
    let (_gcs, proxy, server, agent) = behind_proxy("lost", plan).await;
    super::init_repo(&server).await;

    let src = tempfile::tempdir().unwrap();
    super::make_tree(src.path(), 3);
    let outcome = super::backup(&agent, src.path()).await;
    super::restore_and_compare(&server, &outcome, src.path()).await;

    let mut initiations: BTreeMap<String, usize> = BTreeMap::new();
    for r in proxy.requests().iter().filter(|r| r.method == "POST") {
        *initiations
            .entry(r.query.clone().unwrap_or_default())
            .or_default() += 1;
    }
    assert!(
        initiations.values().any(|n| *n >= 2),
        "no upload was repeated: {initiations:?}"
    );
}

const FORBIDDEN: &str = r#"{"error":{"code":403,"message":"forbidden"}}"#;

/// The server wrote `data`; the agent's upload of the same name is refused
/// with 403, as a role without the right to overwrite may be.
async fn refused_repeat(
    bucket: &str,
    data: &[u8],
    repeat: Vec<u8>,
) -> lodestore_format::Result<()> {
    let plan = FaultPlan::none().with(FaultRule {
        method: "POST",
        key_prefix: String::new(),
        action: FaultAction::Respond {
            status: 403,
            body: FORBIDDEN,
        },
        times: 1,
    });
    let (_gcs, _proxy, server, agent) = behind_proxy(bucket, plan).await;
    let hex = blake3::hash(data).to_hex().to_string();
    let key = format!("packs/{}/{hex}", &hex[..2]);
    server
        .put_if_absent(&key, Bytes::copy_from_slice(data))
        .await
        .unwrap();
    put_named(&agent, &key, repeat).await
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn adv_client_gcs_refused_repeat_upload_is_present() {
    let data = b"a pack the server already holds";
    refused_repeat("refused-same", data, data.to_vec())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn adv_client_gcs_refused_upload_of_other_bytes_fails() {
    let data = b"a pack the server already holds";
    let mut other = data.to_vec();
    other[0] ^= 1;
    let err = refused_repeat("refused-other", data, other)
        .await
        .unwrap_err();
    assert!(
        matches!(err, lodestore_format::Error::NameBurned { .. }),
        "{err:?}"
    );
}

#[tokio::test]
#[ignore = "fake-gcs-server has no IAM: service-side protection is unverified (999.6)"]
async fn adv_service_gcs_overwrite_is_refused() {
    unimplemented!(
        "needs a real bucket and an agent role without storage.objects.delete: an unconditional \
         overwrite and a delete with that role must both be refused"
    );
}
