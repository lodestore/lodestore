use std::time::Duration;

use crate::agent_material::{create, sftp_draft};
use crate::common::{Api, Backend, TestServer, WAIT, wait_until};
use lodestore_agent::{EnrollArgs, NetOptions, Paths, RunArgs, connect, tls};
use lodestore_proto::PROTOCOL_VERSION;
use lodestore_proto::agent::v1::agent_client::AgentClient;
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{AgentMessage, Hello, ServerMessage, Versions};
use lodestore_testkit::sshd::SshdServer;
use serde_json::{Value, json};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

const OTHER_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPXNmebOc3wEWwOMVZdkvtvsvLmchHYR1uQeOBlJtQWZ";
const THIRD_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBPSPxzrVHcF2zQSv3p5NxFPivXZiF0zvk7gjPiCjLBd";

pub(crate) async fn enroll_agent(
    ts: &TestServer,
    api: &Api,
    repository_id: &str,
    name: &str,
) -> Paths {
    let token = api.join_token(repository_id, json!([])).await;
    let paths = ts.agent_paths(name);
    lodestore_agent::enroll(EnrollArgs {
        token,
        paths: paths.clone(),
        hostname: Some(format!("host-{name}")),
        net: NetOptions::default(),
    })
    .await
    .unwrap();
    paths
}

pub(crate) fn agent_id(paths: &Paths) -> String {
    let identity: Value =
        serde_json::from_slice(&std::fs::read(paths.identity_json()).unwrap()).unwrap();
    identity["agent_id"].as_str().unwrap().to_string()
}

pub(crate) async fn open_session(
    ts: &TestServer,
    paths: &Paths,
    capabilities: &[&str],
) -> tonic::Streaming<ServerMessage> {
    open_session_at(
        ts.grpc_address(),
        paths.clone(),
        capabilities_vec(capabilities),
    )
    .await
}

fn capabilities_vec(capabilities: &[&str]) -> Vec<String> {
    capabilities.iter().map(|c| c.to_string()).collect()
}

async fn open_session_at(
    grpc_address: String,
    paths: Paths,
    capabilities: Vec<String>,
) -> tonic::Streaming<ServerMessage> {
    let read = |p: std::path::PathBuf| std::fs::read_to_string(p).unwrap();
    let (ca, cert, key) = (read(paths.ca()), read(paths.cert()), read(paths.key()));
    let channel = connect::channel(
        &grpc_address,
        "127.0.0.1",
        tls::pinned_config(&ca, Some((&cert, &key))).unwrap(),
        &NetOptions::default(),
    )
    .await
    .unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tx.send(AgentMessage {
        body: Some(AgentBody::Hello(Hello {
            versions: Some(Versions {
                min: PROTOCOL_VERSION,
                max: PROTOCOL_VERSION,
            }),
            agent_version: "test".into(),
            capabilities,
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    tokio::spawn(async move {
        let _tx = tx;
        std::future::pending::<()>().await
    });
    let mut stream = AgentClient::new(channel)
        .session(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    let first = stream.message().await.unwrap().unwrap();
    assert!(matches!(first.body, Some(ServerBody::HelloAck(_))));
    stream
}

async fn set_host_key(ts: &TestServer, repository_id: &str, host_key: &str) {
    let row: (String,) = sqlx::query_as("SELECT location_json FROM repositories WHERE id = $1")
        .bind(repository_id)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap();
    let mut options: Value = serde_json::from_str(&row.0).unwrap();
    options["host_key"] = json!(host_key);
    sqlx::query("UPDATE repositories SET location_json = $1 WHERE id = $2")
        .bind(options.to_string())
        .bind(repository_id)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
}

async fn setup(sshd: &SshdServer) -> (TestServer, Api, String) {
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;
    let id = create(&api, &sftp_draft(sshd, "sftp-repo")).await;
    (ts, api, id)
}

fn host_key_of(message: ServerMessage) -> String {
    match message.body {
        Some(ServerBody::UpdateHostKey(update)) => update.host_key,
        other => panic!("expected UpdateHostKey, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn host_key_push_reaches_an_agent_at_session_start() {
    let sshd = SshdServer::start().await;
    let (ts, api, id) = setup(&sshd).await;
    let paths = enroll_agent(&ts, &api, &id, "sftp-agent").await;

    let mut with = open_session(&ts, &paths, &["host_key_v1"]).await;
    let next = tokio::time::timeout(Duration::from_secs(10), with.message())
        .await
        .expect("the pinned key follows HelloAck")
        .unwrap()
        .unwrap();
    assert_eq!(host_key_of(next), sshd.host_key());
    drop(with);

    let mut without = open_session(&ts, &paths, &[]).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while let Ok(Ok(Some(message))) = tokio::time::timeout_at(deadline, without.message()).await {
        assert!(
            !matches!(message.body, Some(ServerBody::UpdateHostKey(_))),
            "an agent without host_key_v1 is not sent the key"
        );
    }
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn host_key_push_is_stored_by_a_running_agent() {
    let sshd = SshdServer::start().await;
    let (ts, api, id) = setup(&sshd).await;
    let paths = enroll_agent(&ts, &api, &id, "sftp-agent").await;
    let agent = agent_id(&paths);
    let stored = |paths: &Paths| -> Value {
        serde_json::from_slice(&std::fs::read(paths.repository_json()).unwrap()).unwrap()
    };
    let before = stored(&paths);
    assert_eq!(before["options"]["host_key"], sshd.host_key());

    let run = |shutdown: CancellationToken| {
        tokio::spawn(lodestore_agent::run(RunArgs {
            paths: paths.clone(),
            net: NetOptions::default(),
            heartbeat: Duration::from_secs(1),
            shutdown,
        }))
    };
    let shutdown = CancellationToken::new();
    let first = run(shutdown.clone());
    assert!(
        wait_until(WAIT, || async { ts.state().registry.is_online(&agent) }).await,
        "the agent did not come online"
    );
    set_host_key(&ts, &id, OTHER_KEY).await;
    shutdown.cancel();
    let _ = first.await;

    let shutdown = CancellationToken::new();
    let second = run(shutdown.clone());
    assert!(
        wait_until(WAIT, || async {
            stored(&paths)["options"]["host_key"] == OTHER_KEY
        })
        .await,
        "the agent did not store the pushed key"
    );
    let mut after = stored(&paths);
    after["options"]["host_key"] = before["options"]["host_key"].clone();
    assert_eq!(after, before);
    shutdown.cancel();
    let _ = second.await;
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn host_key_push_reads_the_key_under_the_repository_lock() {
    let sshd = SshdServer::start().await;
    let (ts, api, id) = setup(&sshd).await;
    let paths = enroll_agent(&ts, &api, &id, "sftp-agent").await;
    let agent = agent_id(&paths);

    let access = ts.state().repos.access_lock(&id);
    let held = access.lock().await;
    let opening = tokio::spawn(open_session_at(
        ts.grpc_address(),
        paths.clone(),
        capabilities_vec(&["host_key_v1"]),
    ));
    assert!(
        wait_until(WAIT, || async { ts.state().registry.is_online(&agent) }).await,
        "the session did not register"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !opening.is_finished(),
        "the session answered while the repository lock was held"
    );
    set_host_key(&ts, &id, THIRD_KEY).await;
    drop(held);
    let mut stream = opening.await.unwrap();

    let next = tokio::time::timeout(Duration::from_secs(10), stream.message())
        .await
        .expect("the key follows once the lock is released")
        .unwrap()
        .unwrap();
    assert_eq!(host_key_of(next), THIRD_KEY);
    ts.stop().await;
}
