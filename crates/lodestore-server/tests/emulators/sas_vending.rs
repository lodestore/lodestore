use std::time::Duration;

use crate::agent_material::{azure_draft, create};
use crate::common::{Api, Backend, TestServer};
use crate::host_key_push::{agent_id, enroll_agent, open_session};
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
