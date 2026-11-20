//! Self-tests of the emulator harness. They need Docker, Node and network
//! access for the first download, so they run through `scripts/test-emulators.sh`.

use lodestore_testkit::fault_proxy::{FaultPlan, FaultProxy};
use lodestore_testkit::gcs::FakeGcsServer;
use lodestore_testkit::minio::free_port;

async fn resumable_put(
    client: &reqwest::Client,
    base: &str,
    bucket: &str,
    key: &str,
    body: &'static str,
) -> reqwest::StatusCode {
    let init = client
        .post(format!(
            "{base}/upload/storage/v1/b/{bucket}/o?uploadType=resumable&name={key}&ifGenerationMatch=0"
        ))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .expect("initiate the upload");
    assert!(init.status().is_success(), "initiate: {}", init.status());
    let location = init
        .headers()
        .get("location")
        .expect("a resumable session has a Location")
        .to_str()
        .expect("ascii location")
        .to_owned();
    client
        .put(location)
        .body(body)
        .send()
        .await
        .expect("complete the upload")
        .status()
}

#[tokio::test]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn gcs_harness_records_a_resumable_conditional_upload() {
    let gcs_port = free_port();
    let proxy = FaultProxy::start(&format!("http://127.0.0.1:{gcs_port}"), FaultPlan::none()).await;
    let proxy_host = proxy.endpoint().trim_start_matches("http://").to_owned();
    let gcs = FakeGcsServer::start_with_public_host(gcs_port, &proxy_host).await;
    gcs.create_bucket("bucket").await;

    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let base = proxy.endpoint();
    let first = resumable_put(&client, &base, "bucket", "k", "payload").await;
    assert!(first.is_success(), "first write: {first}");
    let second = resumable_put(&client, &base, "bucket", "k", "payload").await;
    assert_eq!(second, reqwest::StatusCode::PRECONDITION_FAILED);

    let requests = proxy.requests();
    let with_query = |method: &str, needle: &str| {
        requests
            .iter()
            .filter(|r| r.method == method)
            .filter(|r| r.query.as_deref().is_some_and(|q| q.contains(needle)))
            .count()
    };
    assert_eq!(
        with_query("POST", "ifGenerationMatch=0"),
        2,
        "both conditional initiations are recorded: {requests:?}"
    );
    assert_eq!(
        with_query("PUT", "upload_id="),
        2,
        "both completions went through the proxy: {requests:?}"
    );
}
