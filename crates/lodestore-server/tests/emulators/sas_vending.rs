use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::agent_material::{azure_draft, create};
use crate::common::{Api, Backend, TestServer, WAIT, wait_until};
use crate::host_key_push::{
    agent_id, capabilities_vec, enroll_agent, open_session, open_session_at,
};
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{RepoLocation, ServerMessage};
use lodestore_server::grpc::sas_reissue::reissue_once;
use lodestore_testkit::azurite::AzuriteServer;
use sqlx::Row;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const CAPS: [&str; 3] = ["host_key_v1", "location_v1", "sas_refresh_v1"];

async fn vending_repo(ts: &TestServer, az: &AzuriteServer, container: &str) -> (Api, String) {
    az.create_container(container).await;
    let api = ts.api().await;
    let id = create(&api, &azure_draft(az, container)).await;
    (api, id)
}

fn field<'a>(sas: &'a str, name: &str) -> &'a str {
    sas.split('&')
        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
        .unwrap_or_else(|| panic!("{name} is missing from the SAS"))
}

fn time_of(sas: &str, name: &str) -> i64 {
    let text = field(sas, name).replace("%3A", ":");
    OffsetDateTime::parse(&text, &Rfc3339)
        .unwrap()
        .unix_timestamp()
}

async fn next_location(stream: &mut tonic::Streaming<ServerMessage>) -> RepoLocation {
    let next = tokio::time::timeout(Duration::from_secs(10), stream.message())
        .await
        .expect("an UpdateLocation arrives")
        .unwrap()
        .unwrap();
    match next.body {
        Some(ServerBody::UpdateLocation(update)) => update.location.expect("a location"),
        other => panic!("expected UpdateLocation, got {other:?}"),
    }
}

async fn expires_at(ts: &TestServer, agent: &str) -> Option<i64> {
    sqlx::query("SELECT sas_expires_at FROM agents WHERE id = $1")
        .bind(agent)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .get("sas_expires_at")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sas_reissue_reaches_an_online_agent() {
    let az = AzuriteServer::start().await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let (api, id) = vending_repo(&ts, &az, "reissue").await;
    let paths = enroll_agent(&ts, &api, &id, "reissue-agent").await;
    let agent = agent_id(&paths);

    let mut stream = open_session(&ts, &paths, &CAPS).await;
    let first = next_location(&mut stream).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(reissue_once(ts.state()).await.unwrap(), 1);
    let second = next_location(&mut stream).await;

    let (first, second) = (&first.options["sas_token"], &second.options["sas_token"]);
    assert!(second.contains("sp=rcl"), "{second}");
    assert!(!second.contains("account_key"));
    assert!(time_of(second, "st") > time_of(first, "st"));
    assert_eq!(
        expires_at(&ts, &agent).await,
        Some(time_of(second, "se") * 1000)
    );
    ts.stop().await;
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogBuffer {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

fn capture_logs() -> LogBuffer {
    let logs = LogBuffer::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("the only test that logs");
    logs
}

fn stored_options(paths: &lodestore_agent::Paths) -> serde_json::Value {
    let file: serde_json::Value =
        serde_json::from_slice(&std::fs::read(paths.repository_json()).unwrap()).unwrap();
    file["options"].clone()
}

async fn wait_for_waiters(lock: &Arc<tokio::sync::Mutex<()>>, count: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while Arc::strong_count(lock) != count {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the lock has {} holders, expected {count}",
            Arc::strong_count(lock)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn locations_within(
    stream: &mut tonic::Streaming<ServerMessage>,
    window: Duration,
) -> Vec<RepoLocation> {
    let deadline = tokio::time::Instant::now() + window;
    let mut found = Vec::new();
    while let Ok(Ok(Some(message))) = tokio::time::timeout_at(deadline, stream.message()).await {
        if let Some(ServerBody::UpdateLocation(update)) = message.body {
            found.extend(update.location);
        }
    }
    found
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sas_vending_session_and_enroll_send_a_sas_and_no_account_key() {
    let az = AzuriteServer::start().await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let (api, id) = vending_repo(&ts, &az, "delivery").await;
    let paths = enroll_agent(&ts, &api, &id, "delivery-agent").await;

    let enrolled = stored_options(&paths);
    assert!(enrolled["sas_token"].as_str().unwrap().contains("sp=rcl"));
    assert!(
        !enrolled.to_string().contains(az.account_key()),
        "{enrolled}"
    );
    assert!(enrolled.get("account_key").is_none(), "{enrolled}");

    let mut stream = open_session(&ts, &paths, &CAPS).await;
    let location = next_location(&mut stream).await;
    assert!(location.options["sas_token"].contains("sp=rcl"));
    assert!(!location.options.contains_key("account_key"));
    assert!(!format!("{location:?}").contains(az.account_key()));
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sas_vending_enroll_and_session_record_the_issued_expiry() {
    let az = AzuriteServer::start().await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let (api, id) = vending_repo(&ts, &az, "expiry").await;
    let paths = enroll_agent(&ts, &api, &id, "expiry-agent").await;
    let agent = agent_id(&paths);

    let enrolled = stored_options(&paths);
    let first = time_of(enrolled["sas_token"].as_str().unwrap(), "se") * 1000;
    assert_eq!(expires_at(&ts, &agent).await, Some(first));

    tokio::time::sleep(Duration::from_millis(1100)).await;
    let mut stream = open_session(&ts, &paths, &CAPS).await;
    let location = next_location(&mut stream).await;
    let second = time_of(&location.options["sas_token"], "se") * 1000;
    assert!(second > first);
    assert_eq!(expires_at(&ts, &agent).await, Some(second));
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sas_vending_session_of_a_revoked_agent_gets_no_sas() {
    let az = AzuriteServer::start().await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let (api, id) = vending_repo(&ts, &az, "revoked").await;
    let paths = enroll_agent(&ts, &api, &id, "revoked-agent").await;
    let agent = agent_id(&paths);
    let recorded = expires_at(&ts, &agent).await;
    assert!(recorded.is_some());

    let lock = ts.state().repos.access_lock(&id);
    let held = lock.lock().await;
    let opening = tokio::spawn(open_session_at(
        ts.grpc_address(),
        paths.clone(),
        capabilities_vec(&CAPS),
    ));
    wait_for_waiters(&lock, 3).await;
    ts.state().registry.revoke(&agent);
    drop(held);

    // The session RPC is refused, so the helper's own unwrap of the opening call panics.
    let refused = opening.await.unwrap_err().into_panic();
    let refused = refused.downcast_ref::<String>().expect("a panic message");
    assert!(refused.contains("revoked"), "{refused}");
    assert_eq!(expires_at(&ts, &agent).await, recorded);
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sas_vending_agent_without_location_support_is_warned() {
    let logs = capture_logs();
    let az = AzuriteServer::start().await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let (api, id) = vending_repo(&ts, &az, "oldagent").await;
    let paths = enroll_agent(&ts, &api, &id, "old-agent").await;
    let agent = agent_id(&paths);

    let mut stream = open_session(&ts, &paths, &["host_key_v1"]).await;
    assert!(
        locations_within(&mut stream, Duration::from_secs(1))
            .await
            .is_empty()
    );
    assert!(
        wait_until(WAIT, || async {
            logs.text()
                .contains("cannot receive renewed storage tokens")
        })
        .await
    );
    let text = logs.text();
    assert!(text.contains(&agent), "{text}");
    assert!(!text.contains(az.account_key()), "{text}");
    assert!(!text.contains("sig="), "{text}");
    ts.stop().await;
}
