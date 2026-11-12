mod common;

use std::time::Duration;

use common::{Api, Backend, TestServer, backends, wait_until};
use lodestore_agent::{EnrollArgs, NetOptions, Paths, connect, tls};
use lodestore_proto::PROTOCOL_VERSION;
use lodestore_proto::agent::v1::agent_client::AgentClient;
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::enrollment_client::EnrollmentClient;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{
    AgentMessage, FetchAgentUpdateRequest, GetTrustAnchorRequest, Hello, ServerMessage,
    UpdateAssign, UpdateFile, UpdateState, UpdateStatus, Versions,
};
use lodestore_server::agent_update::rollout;
use lodestore_server::db::now_ms;
use lodestore_server::grpc::dispatch::{claim_job, lock_host};
use sha2::{Digest, Sha256};
use sqlx::{AnyPool, Row};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tonic::{Code, Streaming};

const WAIT: Duration = Duration::from_secs(15);

struct Host {
    id: String,
    paths: Paths,
}

async fn enroll(ts: &TestServer, api: &Api, repo: &str, hostname: &str) -> Host {
    let token = api.join_token(repo, serde_json::json!([])).await;
    let paths = ts.agent_paths(hostname);
    let enrolled = lodestore_agent::enroll(EnrollArgs {
        token,
        paths: paths.clone(),
        hostname: Some(hostname.to_string()),
        net: NetOptions::default(),
    })
    .await
    .expect("enroll");
    Host {
        id: enrolled.agent_id,
        paths,
    }
}

async fn channel(ts: &TestServer, paths: &Paths) -> Channel {
    let address = ts.grpc_address();
    let bootstrap = connect::channel(
        &address,
        "127.0.0.1",
        tls::bootstrap_config().unwrap(),
        &NetOptions::default(),
    )
    .await
    .expect("bootstrap channel");
    let ca_pem = EnrollmentClient::new(bootstrap)
        .get_trust_anchor(GetTrustAnchorRequest {})
        .await
        .expect("trust anchor")
        .into_inner()
        .ca_pem;
    let cert = std::fs::read_to_string(paths.cert()).unwrap();
    let key = std::fs::read_to_string(paths.key()).unwrap();
    connect::channel(
        &address,
        "127.0.0.1",
        tls::pinned_config(&ca_pem, Some((cert.as_str(), key.as_str()))).unwrap(),
        &NetOptions::default(),
    )
    .await
    .expect("agent channel")
}

struct Fake {
    tx: mpsc::Sender<AgentMessage>,
    rx: Streaming<ServerMessage>,
}

impl Fake {
    async fn connect(ts: &TestServer, host: &Host, version: &str) -> Fake {
        let (tx, rx) = mpsc::channel(64);
        tx.send(AgentMessage {
            body: Some(AgentBody::Hello(Hello {
                versions: Some(Versions {
                    min: PROTOCOL_VERSION,
                    max: PROTOCOL_VERSION,
                }),
                agent_version: version.to_string(),
                jobs: vec![],
                capabilities: vec!["update_v1".to_string()],
            })),
        })
        .await
        .unwrap();
        let inbound = AgentClient::new(channel(ts, &host.paths).await)
            .session(ReceiverStream::new(rx))
            .await
            .expect("session")
            .into_inner();
        let registry = &ts.state().registry;
        let ready = wait_until(WAIT, || async { registry.live.is_ready(&host.id) }).await;
        assert!(ready, "the session of {} never became ready", host.id);
        Fake { tx, rx: inbound }
    }

    async fn report(&self, update_id: &str, state: UpdateState, reason: &str) {
        self.tx
            .send(AgentMessage {
                body: Some(AgentBody::UpdateStatus(UpdateStatus {
                    update_id: update_id.to_string(),
                    state: state as i32,
                    reason: reason.to_string(),
                })),
            })
            .await
            .unwrap();
    }

    async fn ack(&mut self, window: Duration) -> Option<String> {
        let deadline = tokio::time::Instant::now() + window;
        while let Ok(Ok(Some(message))) = tokio::time::timeout_at(deadline, self.rx.message()).await
        {
            if let Some(ServerBody::UpdateStatusAck(ack)) = message.body {
                return Some(ack.update_id);
            }
        }
        None
    }

    async fn assign(&mut self) -> UpdateAssign {
        loop {
            let message = tokio::time::timeout(WAIT, self.rx.message())
                .await
                .expect("timed out waiting for an UpdateAssign")
                .expect("stream error")
                .expect("the server closed the stream");
            if let Some(ServerBody::UpdateAssign(assign)) = message.body {
                return assign;
            }
        }
    }

    async fn no_assign(&mut self, window: Duration) {
        let deadline = tokio::time::Instant::now() + window;
        while let Ok(Ok(Some(message))) = tokio::time::timeout_at(deadline, self.rx.message()).await
        {
            assert!(
                !matches!(message.body, Some(ServerBody::UpdateAssign(_))),
                "an UpdateAssign must not be sent"
            );
        }
    }
}

async fn fetch(
    ts: &TestServer,
    host: &Host,
    update_id: &str,
    file: UpdateFile,
) -> Result<Vec<u8>, tonic::Status> {
    let mut stream = AgentClient::new(channel(ts, &host.paths).await)
        .fetch_agent_update(FetchAgentUpdateRequest {
            update_id: update_id.to_string(),
            file: file as i32,
        })
        .await?
        .into_inner();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.message().await? {
        bytes.extend(chunk.data);
    }
    Ok(bytes)
}

fn payload() -> Vec<u8> {
    (0..600 * 1024).map(|i| (i % 251) as u8).collect()
}

fn minisig(version: &str, arch: &str) -> String {
    format!(
        "untrusted comment: signature\nRUQ=\ntrusted comment: lodestore-agent {version} {arch}\nAAAA\n"
    )
}

async fn arch_of(pool: &AnyPool, host: &str) -> String {
    sqlx::query("SELECT arch FROM agents WHERE id = $1")
        .bind(host)
        .fetch_one(pool)
        .await
        .unwrap()
        .try_get("arch")
        .unwrap()
}

fn publish(ts: &TestServer, arch: &str, version: &str, bytes: &[u8]) {
    let dir = &ts.config.agent_dir;
    std::fs::create_dir_all(dir).unwrap();
    let file = format!("lodestore-agent-linux-{arch}");
    std::fs::write(dir.join(&file), bytes).unwrap();
    std::fs::write(dir.join(format!("{file}.minisig")), minisig(version, arch)).unwrap();
}

async fn insert_rollout(pool: &AnyPool, id: &str, state: &str) {
    sqlx::query(
        "INSERT INTO agent_rollouts (id, target_version, state, canary, wave_size, pause_after, \
         created_by, created_at) VALUES ($1, '99.0.0', $2, 0, 10, 0, 'test', $3)",
    )
    .bind(id)
    .bind(state)
    .bind(now_ms())
    .execute(pool)
    .await
    .unwrap();
}

#[allow(clippy::too_many_arguments)]
async fn insert_update(
    pool: &AnyPool,
    id: &str,
    rollout: &str,
    host: &str,
    arch: &str,
    from: &str,
    to: &str,
    wave: i64,
    state: &str,
) {
    sqlx::query(
        "INSERT INTO agent_updates (id, rollout_id, host_id, arch, from_version, target_version, \
         wave, state, queued_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(id)
    .bind(rollout)
    .bind(host)
    .bind(arch)
    .bind(from)
    .bind(to)
    .bind(wave)
    .bind(state)
    .bind(now_ms())
    .execute(pool)
    .await
    .unwrap();
}

async fn queue_update(pool: &AnyPool, host: &str, arch: &str, to: &str) {
    insert_rollout(pool, "rollout-1", "paused").await;
    insert_update(
        pool,
        "update-1",
        "rollout-1",
        host,
        arch,
        "0.0.1",
        to,
        0,
        "queued",
    )
    .await;
    sqlx::query("UPDATE agent_rollouts SET state = 'running' WHERE id = 'rollout-1'")
        .execute(pool)
        .await
        .unwrap();
}

async fn text(pool: &AnyPool, sql: &'static str, id: &str) -> Option<String> {
    sqlx::query(sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap()
        .and_then(|row| row.try_get::<Option<String>, _>(0).unwrap())
}

async fn wait_text(pool: &AnyPool, sql: &'static str, id: &str, want: &str) {
    let reached = wait_until(Duration::from_secs(10), || async {
        text(pool, sql, id).await.as_deref() == Some(want)
    })
    .await;
    assert!(
        reached,
        "'{sql}' is {:?}, expected '{want}'",
        text(pool, sql, id).await
    );
}

const UPDATE_STATE: &str = "SELECT state FROM agent_updates WHERE id = $1";
const ROLLOUT_STATE: &str = "SELECT state FROM agent_rollouts WHERE id = $1";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_single_host_succeeds() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let other = enroll(&ts, &api, &repo, "host-b").await;
        let pool = &ts.state().db.pool;
        let arch = arch_of(pool, &host.id).await;
        let bytes = payload();
        publish(&ts, &arch, "99.0.0", &bytes);
        queue_update(pool, &host.id, &arch, "99.0.0").await;
        insert_rollout(pool, "rollout-2", "cancelled").await;
        insert_update(
            pool,
            "update-2",
            "rollout-2",
            &other.id,
            &arch,
            "0.0.1",
            "99.0.0",
            0,
            "assigned",
        )
        .await;

        let mut agent = Fake::connect(&ts, &host, "0.0.1").await;
        let assign = agent.assign().await;
        assert_eq!(assign.update_id, "update-1", "[{}]", backend.name());
        assert_eq!(assign.target_version, "99.0.0");
        assert_eq!(assign.arch, arch);
        assert_eq!(assign.size, bytes.len() as u64);
        assert_eq!(
            assign.sha256,
            data_encoding::HEXLOWER.encode(&Sha256::digest(&bytes))
        );
        assert_eq!(
            text(pool, UPDATE_STATE, "update-1").await.as_deref(),
            Some("assigned")
        );

        let got = fetch(&ts, &host, "update-1", UpdateFile::Binary)
            .await
            .expect("fetch");
        assert_eq!(got, bytes, "[{}]", backend.name());
        let progress = sqlx::query("SELECT progress_at FROM agent_updates WHERE id = 'update-1'")
            .fetch_one(pool)
            .await
            .unwrap()
            .try_get::<Option<i64>, _>(0)
            .unwrap();
        assert!(progress.is_some(), "progress_at is recorded");

        let foreign = fetch(&ts, &host, "update-2", UpdateFile::Binary)
            .await
            .expect_err("someone else's update");
        assert_eq!(foreign.code(), Code::PermissionDenied);

        let _agent = Fake::connect(&ts, &host, "99.0.0").await;
        wait_text(pool, UPDATE_STATE, "update-1", "succeeded").await;
        wait_text(pool, ROLLOUT_STATE, "rollout-1", "done").await;
        assert_eq!(
            text(pool, "SELECT version FROM agents WHERE id = $1", &host.id)
                .await
                .as_deref(),
            Some("99.0.0")
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_signature_is_served_only_while_assigned() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let pool = &ts.state().db.pool;
        let arch = arch_of(pool, &host.id).await;
        publish(&ts, &arch, "99.0.0", &payload());
        queue_update(pool, &host.id, &arch, "99.0.0").await;

        let mut agent = Fake::connect(&ts, &host, "0.0.1").await;
        agent.assign().await;
        let signature = fetch(&ts, &host, "update-1", UpdateFile::Signature)
            .await
            .expect("fetch");
        assert_eq!(
            signature,
            minisig("99.0.0", &arch).into_bytes(),
            "[{}]",
            backend.name()
        );

        sqlx::query("UPDATE agent_updates SET state = 'succeeded' WHERE id = 'update-1'")
            .execute(pool)
            .await
            .unwrap();
        for file in [UpdateFile::Binary, UpdateFile::Signature] {
            let denied = fetch(&ts, &host, "update-1", file)
                .await
                .expect_err("not assigned any more");
            assert_eq!(denied.code(), Code::PermissionDenied);
        }
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_loop_runs_without_schedule_tick() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let pool = &ts.state().db.pool;
        let arch = arch_of(pool, &host.id).await;
        publish(&ts, &arch, "99.0.0", &payload());

        let mut agent = Fake::connect(&ts, &host, "0.0.1").await;
        agent.no_assign(Duration::from_millis(500)).await;
        queue_update(pool, &host.id, &arch, "99.0.0").await;
        let assign = agent.assign().await;
        assert_eq!(assign.update_id, "update-1", "[{}]", backend.name());
        ts.stop().await;
    }
}

async fn setup_host(pool: &AnyPool, host: &Host, version: &str, arch: Option<&str>, update: bool) {
    let caps = if update { r#"["update_v1"]"# } else { "[]" };
    sqlx::query("UPDATE agents SET version = $1, capabilities_json = $2 WHERE id = $3")
        .bind(version)
        .bind(caps)
        .bind(&host.id)
        .execute(pool)
        .await
        .unwrap();
    if let Some(arch) = arch {
        sqlx::query("UPDATE agents SET arch = $1 WHERE id = $2")
            .bind(arch)
            .bind(&host.id)
            .execute(pool)
            .await
            .unwrap();
    }
}

async fn fleet(ts: &TestServer, names: &[&str]) -> Vec<Host> {
    let api = ts.api().await;
    let repo = ts.create_local_repository("main").await;
    let mut hosts = Vec::new();
    for name in names {
        hosts.push(enroll(ts, &api, &repo, name).await);
    }
    hosts
}

async fn rest(
    ts: &TestServer,
    scopes: &[&str],
    path: &str,
    body: Option<serde_json::Value>,
) -> reqwest::Response {
    let client = common::bearer_client(&ts.issue_token(scopes).await);
    let url = format!("{}{path}", ts.rest_url());
    match body {
        Some(body) => client.post(url).json(&body).send().await.unwrap(),
        None => client.get(url).send().await.unwrap(),
    }
}

async fn start(ts: &TestServer, body: serde_json::Value) -> reqwest::Response {
    rest(ts, &["agents:update"], "/api/v1/agent-updates", Some(body)).await
}

async fn count(pool: &AnyPool, sql: &'static str) -> i64 {
    sqlx::query(sql)
        .fetch_one(pool)
        .await
        .unwrap()
        .try_get(0)
        .unwrap()
}

async fn counts(response: reqwest::Response) -> (Option<u64>, Option<u64>) {
    let body: serde_json::Value = response.json().await.unwrap();
    (body["hosts"].as_u64(), body["skipped"].as_u64())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_start_requires_the_agents_update_scope() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = fleet(&ts, &["host-a"]).await;
        let pool = &ts.state().db.pool;
        let arch = arch_of(pool, &hosts[0].id).await;
        publish(&ts, &arch, "99.0.0", b"binary");
        setup_host(pool, &hosts[0], "0.0.1", None, true).await;
        let body = serde_json::json!({ "hosts": [hosts[0].id] });

        for scopes in [
            &["hosts:write"][..],
            &["hosts:read"],
            &["hosts:read", "hosts:write"],
        ] {
            let denied = rest(&ts, scopes, "/api/v1/agent-updates", Some(body.clone())).await;
            assert_eq!(denied.status(), 403, "{scopes:?} [{}]", backend.name());
        }
        let anonymous = reqwest::Client::new()
            .post(format!("{}/api/v1/agent-updates", ts.rest_url()))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(anonymous.status(), 401);
        assert_eq!(count(pool, "SELECT COUNT(*) FROM agent_rollouts").await, 0);

        let started = start(&ts, body).await;
        assert_eq!(started.status(), 202, "[{}]", backend.name());
        let started: serde_json::Value = started.json().await.unwrap();
        assert_eq!(
            (started["hosts"].as_u64(), started["skipped"].as_u64()),
            (Some(1), Some(0))
        );
        let created_by = text(
            pool,
            "SELECT created_by FROM agent_rollouts WHERE id = $1",
            started["id"].as_str().unwrap(),
        )
        .await;
        assert_eq!(created_by.as_deref(), Some("token:test"));
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_start_and_resume_need_a_recent_password_for_sessions() {
    for backend in backends() {
        let what = backend.name();
        let ts = TestServer::start(&backend).await;
        let hosts = signed_fleet(&ts, &["host-a"]).await;
        let pool = &ts.state().db.pool;
        setup_host(pool, &hosts[0], "0.0.1", None, true).await;
        insert_rollout(pool, "held", "paused").await;
        let arch = arch_of(pool, &hosts[0].id).await;
        insert_update(
            pool,
            "held-1",
            "held",
            &hosts[0].id,
            &arch,
            "0.0.1",
            "99.0.0",
            0,
            "queued",
        )
        .await;
        let api = ts.api().await;
        sqlx::query("UPDATE sessions SET authenticated_at = $1")
            .bind(now_ms() - 16 * 60_000)
            .execute(pool)
            .await
            .unwrap();

        let refused = api
            .post(
                "/api/v1/agent-updates",
                &serde_json::json!({ "outdated": true }),
            )
            .await;
        assert_eq!(refused.status(), 403, "start [{what}]");
        let refused: serde_json::Value = refused.json().await.unwrap();
        assert_eq!(refused["code"], "step_up_required", "start [{what}]");
        let refused = api
            .post("/api/v1/agent-updates/held/resume", &serde_json::json!({}))
            .await;
        assert_eq!(refused.status(), 403, "resume [{what}]");
        let refused: serde_json::Value = refused.json().await.unwrap();
        assert_eq!(refused["code"], "step_up_required", "resume [{what}]");
        assert_eq!(
            text(pool, ROLLOUT_STATE, "held").await.as_deref(),
            Some("paused"),
            "[{what}]"
        );
        assert_eq!(count(pool, "SELECT COUNT(*) FROM agent_rollouts").await, 1);

        let paused = api
            .post("/api/v1/agent-updates/held/pause", &serde_json::json!({}))
            .await;
        assert_eq!(paused.status(), 409, "pausing needs no step-up [{what}]");
        let by_token = start(&ts, serde_json::json!({ "outdated": true })).await;
        assert_eq!(by_token.status(), 409, "a token needs no step-up [{what}]");

        let step = api
            .post(
                "/api/v1/auth/step-up",
                &serde_json::json!({ "password": common::ADMIN_PASSWORD }),
            )
            .await;
        assert_eq!(step.status(), 204, "[{what}]");
        let resumed = api
            .post("/api/v1/agent-updates/held/resume", &serde_json::json!({}))
            .await;
        assert_eq!(resumed.status(), 204, "resume after step-up [{what}]");
        let cancelled = api
            .post("/api/v1/agent-updates/held/cancel", &serde_json::json!({}))
            .await;
        assert_eq!(cancelled.status(), 204, "[{what}]");
        let started = api
            .post(
                "/api/v1/agent-updates",
                &serde_json::json!({ "outdated": true }),
            )
            .await;
        assert_eq!(started.status(), 202, "start after step-up [{what}]");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_start_assigns_the_selected_host() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = fleet(&ts, &["host-a"]).await;
        let pool = &ts.state().db.pool;
        let arch = arch_of(pool, &hosts[0].id).await;
        publish(&ts, &arch, "99.0.0", b"binary");
        let mut agent = Fake::connect(&ts, &hosts[0], "0.0.1").await;
        let started = start(&ts, serde_json::json!({ "hosts": [hosts[0].id] })).await;
        assert_eq!(started.status(), 202, "[{}]", backend.name());
        let assign = agent.assign().await;
        assert_eq!(assign.target_version, "99.0.0");
        assert_eq!(assign.arch, arch);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_start_outdated_queues_only_updatable_hosts() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = fleet(&ts, &["a-old", "b-no-cap", "c-current", "d-revoked"]).await;
        let pool = &ts.state().db.pool;
        let arch = arch_of(pool, &hosts[0].id).await;
        publish(&ts, &arch, "99.0.0", b"binary");
        setup_host(pool, &hosts[0], "0.0.1", None, true).await;
        setup_host(pool, &hosts[1], "0.0.1", None, false).await;
        setup_host(pool, &hosts[2], "99.0.0", None, true).await;
        setup_host(pool, &hosts[3], "0.0.1", None, true).await;
        sqlx::query("UPDATE agents SET status = 'revoked' WHERE id = $1")
            .bind(&hosts[3].id)
            .execute(pool)
            .await
            .unwrap();

        let started = start(&ts, serde_json::json!({ "outdated": true })).await;
        assert_eq!(started.status(), 202, "[{}]", backend.name());
        assert_eq!(counts(started).await, (Some(1), Some(3)));
        let queued = text(
            pool,
            "SELECT host_id FROM agent_updates WHERE state = $1",
            "queued",
        )
        .await;
        assert_eq!(queued.as_deref(), Some(hosts[0].id.as_str()));
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_start_skips_hosts_without_a_binary_of_their_arch() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = fleet(&ts, &["host-a", "host-b"]).await;
        let pool = &ts.state().db.pool;
        publish(&ts, "aarch64", "1.3.0", b"binary");
        publish(&ts, "x86_64", "1.2.3", b"binary");
        setup_host(pool, &hosts[0], "1.0.0", Some("x86_64"), true).await;
        setup_host(pool, &hosts[1], "1.0.0", Some("aarch64"), true).await;

        let started = start(&ts, serde_json::json!({ "outdated": true })).await;
        assert_eq!(started.status(), 202, "[{}]", backend.name());
        assert_eq!(counts(started).await, (Some(1), Some(1)));
        let rows = count(
            pool,
            "SELECT COUNT(*) FROM agent_updates WHERE arch = 'aarch64' AND state = 'queued'",
        )
        .await;
        assert_eq!(rows, 1);
        assert_eq!(count(pool, "SELECT COUNT(*) FROM agent_updates").await, 1);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_start_refuses_bad_requests() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = fleet(&ts, &["host-a", "host-b"]).await;
        let pool = &ts.state().db.pool;
        let arch = arch_of(pool, &hosts[0].id).await;
        setup_host(pool, &hosts[0], "0.0.1", None, true).await;
        setup_host(pool, &hosts[1], "0.0.1", None, true).await;

        let unsigned = start(&ts, serde_json::json!({ "outdated": true })).await;
        assert_eq!(
            unsigned.status(),
            422,
            "no signed binary [{}]",
            backend.name()
        );

        publish(&ts, &arch, "99.0.0", b"binary");
        for body in [
            serde_json::json!({}),
            serde_json::json!({ "hosts": [] }),
            serde_json::json!({ "outdated": false }),
            serde_json::json!({ "hosts": [hosts[0].id], "outdated": true }),
            serde_json::json!({ "hosts": ["nope"] }),
        ] {
            let refused = start(&ts, body.clone()).await;
            assert_eq!(refused.status(), 422, "{body} [{}]", backend.name());
        }
        assert_eq!(count(pool, "SELECT COUNT(*) FROM agent_rollouts").await, 0);

        let first = start(&ts, serde_json::json!({ "hosts": [hosts[0].id] })).await;
        assert_eq!(first.status(), 202);
        let second = start(&ts, serde_json::json!({ "hosts": [hosts[1].id] })).await;
        assert_eq!(second.status(), 409);
        let body: serde_json::Value = second.json().await.unwrap();
        assert_eq!(body["message"], "An update is already running.");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_start_splits_hosts_into_canary_and_waves() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| {
            c.agent_updates.canary = 2;
            c.agent_updates.wave = 2;
        })
        .await;
        let hosts = fleet(&ts, &["h1", "h2", "h3", "h4", "h5"]).await;
        let pool = &ts.state().db.pool;
        let arch = arch_of(pool, &hosts[0].id).await;
        publish(&ts, &arch, "99.0.0", b"binary");
        for host in &hosts {
            setup_host(pool, host, "0.0.1", None, true).await;
        }

        let started = start(&ts, serde_json::json!({ "outdated": true })).await;
        assert_eq!(started.status(), 202, "[{}]", backend.name());
        let rows = sqlx::query(
            "SELECT a.hostname, u.wave FROM agent_updates u JOIN agents a ON a.id = u.host_id \
             ORDER BY a.hostname",
        )
        .fetch_all(pool)
        .await
        .unwrap();
        let waves: Vec<i64> = rows.iter().map(|r| r.try_get("wave").unwrap()).collect();
        assert_eq!(waves, [0, 0, 1, 1, 2], "[{}]", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_start_skips_host_with_active_row() {
    for active in ["queued", "assigned"] {
        for backend in backends() {
            let ts = TestServer::start(&backend).await;
            let hosts = fleet(&ts, &["host-a", "host-b", "host-c"]).await;
            let pool = &ts.state().db.pool;
            let arch = arch_of(pool, &hosts[0].id).await;
            publish(&ts, &arch, "99.0.0", b"binary");
            for host in &hosts {
                setup_host(pool, host, "0.0.1", None, true).await;
            }
            insert_rollout(pool, "old", "cancelled").await;
            insert_update(
                pool,
                "left-over",
                "old",
                &hosts[0].id,
                &arch,
                "0.0.1",
                "99.0.0",
                0,
                active,
            )
            .await;

            let started = start(&ts, serde_json::json!({ "outdated": true })).await;
            assert_eq!(started.status(), 202, "{active} [{}]", backend.name());
            assert_eq!(counts(started).await, (Some(2), Some(1)));
            let rows = sqlx::query("SELECT COUNT(*) FROM agent_updates WHERE host_id = $1")
                .bind(&hosts[0].id)
                .fetch_one(pool)
                .await
                .unwrap()
                .try_get::<i64, _>(0)
                .unwrap();
            assert_eq!(rows, 1, "{active} [{}]", backend.name());
            ts.stop().await;
        }
    }
}

async fn status(ts: &TestServer) -> serde_json::Value {
    let response = rest(ts, &["hosts:read"], "/api/v1/agent-updates", None).await;
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

fn host_of<'a>(status: &'a serde_json::Value, hostname: &str) -> &'a serde_json::Value {
    status["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["hostname"] == hostname)
        .unwrap_or_else(|| panic!("no host {hostname}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_status_without_a_signature() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = fleet(&ts, &["a-cap", "b-plain", "c-current"]).await;
        let pool = &ts.state().db.pool;
        setup_host(pool, &hosts[0], "0.0.1", None, true).await;
        setup_host(pool, &hosts[1], "0.0.1", None, false).await;
        setup_host(pool, &hosts[2], env!("CARGO_PKG_VERSION"), None, true).await;

        let anonymous = reqwest::get(format!("{}/api/v1/agent-updates", ts.rest_url()))
            .await
            .unwrap();
        assert_eq!(anonymous.status(), 401);
        let body = status(&ts).await;
        assert_eq!(body["signed"], false, "[{}]", backend.name());
        assert!(body["target_version"].is_null());
        assert_eq!(body["settings"]["rollback_after_secs"], 120);
        assert_eq!(body["settings"]["canary"], 2);
        assert!(body["rollout"].is_null());
        let a = host_of(&body, "a-cap");
        assert_eq!(
            (a["outdated"].clone(), a["updatable"].clone()),
            (true.into(), false.into())
        );
        assert_eq!(a["blocker"], "unsigned");
        assert_eq!(host_of(&body, "b-plain")["blocker"], "no_self_update");
        let c = host_of(&body, "c-current");
        assert_eq!(c["outdated"], false);
        assert!(c["blocker"].is_null());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_status_with_a_signature() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = fleet(&ts, &["a-ok", "b-plain", "c-other-arch", "d-current"]).await;
        let pool = &ts.state().db.pool;
        let arch = arch_of(pool, &hosts[0].id).await;
        publish(&ts, &arch, "99.0.0", b"binary");
        setup_host(pool, &hosts[0], "0.0.1", None, true).await;
        setup_host(pool, &hosts[1], "0.0.1", None, false).await;
        setup_host(pool, &hosts[2], "0.0.1", Some("riscv64"), true).await;
        setup_host(pool, &hosts[3], "99.0.0", None, true).await;

        let body = status(&ts).await;
        assert_eq!(body["signed"], true, "[{}]", backend.name());
        assert_eq!(body["target_version"], "99.0.0");
        assert_eq!(body["binaries"][0]["arch"], arch.as_str());
        assert_eq!(body["binaries"][0]["signed"], true);
        let a = host_of(&body, "a-ok");
        assert_eq!(
            (a["outdated"].clone(), a["updatable"].clone()),
            (true.into(), true.into())
        );
        assert!(a["blocker"].is_null());
        assert_eq!(host_of(&body, "b-plain")["blocker"], "no_self_update");
        assert_eq!(host_of(&body, "c-other-arch")["blocker"], "no_binary");
        let d = host_of(&body, "d-current");
        assert_eq!(d["outdated"], false);
        assert!(d["blocker"].is_null());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_status_reports_the_state_of_each_host() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = fleet(
            &ts,
            &[
                "h1-queued",
                "h2-busy",
                "h3-offline",
                "h4-updating",
                "h5-rolled",
                "h6-back",
                "h7-above",
            ],
        )
        .await;
        let pool = &ts.state().db.pool;
        let arch = arch_of(pool, &hosts[0].id).await;
        publish(&ts, &arch, "99.0.0", b"binary");
        let _online = [
            Fake::connect(&ts, &hosts[0], "0.0.1").await,
            Fake::connect(&ts, &hosts[1], "0.0.1").await,
        ];
        for host in &hosts[2..] {
            setup_host(pool, host, "0.0.1", None, true).await;
        }
        setup_host(pool, &hosts[5], "99.0.0", None, true).await;
        setup_host(pool, &hosts[6], "100.0.0", None, true).await;
        sqlx::query(
            "INSERT INTO jobs (id, host_id, source, state, spec, queued_at) \
             VALUES ('job-1', $1, 'manual', 'running', $2, $3)",
        )
        .bind(&hosts[1].id)
        .bind(Vec::<u8>::new())
        .bind(now_ms())
        .execute(pool)
        .await
        .unwrap();
        insert_rollout(pool, "r1", "paused").await;
        let states = [
            "queued",
            "queued",
            "queued",
            "assigned",
            "rolled_back",
            "rolled_back",
            "rolled_back",
        ];
        for (i, (host, state)) in hosts.iter().zip(states).enumerate() {
            let id = format!("u{i}");
            insert_update(
                pool, &id, "r1", &host.id, &arch, "0.0.1", "99.0.0", 0, state,
            )
            .await;
        }
        sqlx::query("UPDATE agent_updates SET switched_at = $1 WHERE state = 'assigned'")
            .bind(now_ms())
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE agent_updates SET reason = 'did not connect' WHERE state = 'rolled_back'",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("UPDATE agent_updates SET target_version = '50.0.0' WHERE state = 'assigned'")
            .execute(pool)
            .await
            .unwrap();

        let body = status(&ts).await;
        let state_of = |name: &str| host_of(&body, name)["state"].clone();
        assert_eq!(state_of("h1-queued"), "queued", "[{}]", backend.name());
        assert_eq!(state_of("h2-busy"), "waiting_for_job");
        assert_eq!(state_of("h3-offline"), "offline_queued");
        assert_eq!(state_of("h4-updating"), "updating");
        assert_eq!(host_of(&body, "h4-updating")["target_version"], "50.0.0");
        assert_eq!(state_of("h5-rolled"), "rolled_back");
        let rolled = host_of(&body, "h5-rolled");
        assert_eq!(rolled["reason"], "did not connect");
        assert_eq!(rolled["update_id"], "u4");
        assert!(state_of("h6-back").is_null(), "version reached the target");
        assert!(state_of("h7-above").is_null(), "version passed the target");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_status_summarizes_waves_and_rollbacks() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = fleet(&ts, &["host-a", "host-b"]).await;
        let pool = &ts.state().db.pool;
        let arch = arch_of(pool, &hosts[0].id).await;
        insert_rollout(pool, "r1", "paused").await;
        let rows = [
            ("s0", 0, "succeeded"),
            ("r0", 0, "rolled_back"),
            ("r1", 0, "rolled_back"),
            ("r2", 0, "rolled_back"),
            ("r3", 1, "rolled_back"),
            ("r4", 1, "rolled_back"),
            ("r5", 1, "rolled_back"),
        ];
        for (id, wave, state) in rows {
            insert_update(
                pool,
                id,
                "r1",
                &hosts[0].id,
                &arch,
                "0.0.1",
                "99.0.0",
                wave,
                state,
            )
            .await;
        }
        insert_update(
            pool,
            "q1",
            "r1",
            &hosts[1].id,
            &arch,
            "0.0.1",
            "99.0.0",
            1,
            "queued",
        )
        .await;
        for (i, (id, ..)) in rows.iter().enumerate() {
            sqlx::query("UPDATE agent_updates SET finished_at = $1, reason = $2 WHERE id = $3")
                .bind(1000 + i as i64)
                .bind(format!("reason {id}"))
                .bind(*id)
                .execute(pool)
                .await
                .unwrap();
        }

        let body = status(&ts).await;
        let rollout = &body["rollout"];
        assert_eq!(rollout["id"], "r1", "[{}]", backend.name());
        assert_eq!(rollout["state"], "paused");
        assert_eq!(rollout["total"], 8);
        assert_eq!(rollout["updated"], 1);
        assert_eq!(rollout["rolled_back"], 6);
        let waves = rollout["waves"].as_array().unwrap();
        assert_eq!(waves.len(), 2);
        assert_eq!(
            (
                waves[0]["total"].clone(),
                waves[0]["updated"].clone(),
                waves[0]["rolled_back"].clone()
            ),
            (4.into(), 1.into(), 3.into())
        );
        assert_eq!(
            (waves[1]["rolled_back"].clone(), waves[1]["waiting"].clone()),
            (3.into(), 1.into())
        );
        let rollbacks = rollout["rollbacks"].as_array().unwrap();
        assert_eq!(rollbacks.len(), 5);
        assert_eq!(rollbacks[0]["update_id"], "r5");
        assert_eq!(rollbacks[0]["hostname"], "host-a");
        assert_eq!(rollbacks[0]["reason"], "reason r5");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_new_rollout_retires_the_finished_card() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = signed_fleet(&ts, &["h1"]).await;
        let pool = &ts.state().db.pool;
        insert_rollout(pool, "earlier", "done").await;
        sqlx::query("UPDATE agent_rollouts SET finished_at = $1 WHERE id = 'earlier'")
            .bind(now_ms() - 3_600_000)
            .execute(pool)
            .await
            .unwrap();
        setup_host(pool, &hosts[0], "0.0.1", None, true).await;

        let rollout = begin_one(&ts, &hosts[0]).await;
        assert_eq!(action(&ts, &rollout, "cancel").await, 204);
        assert_eq!(action(&ts, &rollout, "dismiss").await, 204);
        assert!(
            status(&ts).await["rollout"].is_null(),
            "[{}]",
            backend.name()
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_refused_rollout_keeps_the_finished_card() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = signed_fleet(&ts, &["h1"]).await;
        let pool = &ts.state().db.pool;
        insert_rollout(pool, "earlier", "done").await;
        sqlx::query("UPDATE agent_rollouts SET finished_at = $1 WHERE id = 'earlier'")
            .bind(now_ms() - 3_600_000)
            .execute(pool)
            .await
            .unwrap();

        let refused = start(&ts, serde_json::json!({ "hosts": [hosts[0].id] })).await;
        assert_eq!(refused.status(), 422, "[{}]", backend.name());
        assert_eq!(status(&ts).await["rollout"]["id"], "earlier");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn migration_0020_gives_channels_with_every_type_the_new_alert() {
    const ALL_FIVE: &str = r#"["backup_failed","no_recent_backup","agent_offline","check_failed","certificate_expiring"]"#;
    const FOUR: &str = r#"["backup_failed","no_recent_backup","agent_offline","check_failed"]"#;
    for backend in backends() {
        let dir = tempfile::tempdir().unwrap();
        let (url, mut migrator) = match &backend {
            Backend::Sqlite => (
                format!("sqlite://{}/old.db?mode=rwc", dir.path().display()),
                sqlx::migrate!("./migrations/sqlite"),
            ),
            Backend::Postgres(admin) => (
                common::create_postgres_database(admin).await,
                sqlx::migrate!("./migrations/postgres"),
            ),
        };
        sqlx::any::install_default_drivers();
        let pool = AnyPool::connect(&url).await.unwrap();
        let all = migrator.migrations.clone();
        migrator.migrations = all
            .iter()
            .filter(|m| m.version < 20)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        migrator.run(&pool).await.unwrap();
        for (id, source, types) in [
            ("full", "ui", ALL_FIVE),
            ("partial", "ui", FOUR),
            ("config", "config", ALL_FIVE),
        ] {
            sqlx::query(
                "INSERT INTO notification_channels (id, name, kind, source, enabled, types_json, \
                 config_json, created_at, updated_at) VALUES ($1, $1, 'webhook', $2, 1, $3, '{}', 0, 0)",
            )
            .bind(id)
            .bind(source)
            .bind(types)
            .execute(&pool)
            .await
            .unwrap();
        }

        migrator.migrations = all
            .iter()
            .filter(|m| m.version < 28)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        migrator.run(&pool).await.unwrap();

        let types = |id: &'static str| {
            let pool = pool.clone();
            async move {
                text(
                    &pool,
                    "SELECT types_json FROM notification_channels WHERE id = $1",
                    id,
                )
                .await
                .unwrap()
            }
        };
        let full: Vec<String> = serde_json::from_str(&types("full").await).unwrap();
        assert_eq!(full.len(), 6, "[{}]", backend.name());
        assert_eq!(full[5], "agent_update_failed");
        assert_eq!(types("partial").await, FOUR);
        assert_eq!(types("config").await, ALL_FIVE);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn migration_0028_gives_channels_with_every_type_the_new_alerts() {
    const SIX: &str = r#"["backup_failed","no_recent_backup","agent_offline","check_failed","certificate_expiring","agent_update_failed"]"#;
    const FOUR: &str = r#"["backup_failed","no_recent_backup","agent_offline","check_failed"]"#;
    const SEVEN: &str = r#"["backup_failed","no_recent_backup","agent_offline","check_failed","certificate_expiring","agent_update_failed","snapshot_quarantined"]"#;
    const EIGHT: &str = r#"["backup_failed","no_recent_backup","agent_offline","check_failed","certificate_expiring","agent_update_failed","snapshot_quarantined","repository_unavailable"]"#;
    for backend in backends() {
        let dir = tempfile::tempdir().unwrap();
        let (url, mut migrator) = match &backend {
            Backend::Sqlite => (
                format!("sqlite://{}/old.db?mode=rwc", dir.path().display()),
                sqlx::migrate!("./migrations/sqlite"),
            ),
            Backend::Postgres(admin) => (
                common::create_postgres_database(admin).await,
                sqlx::migrate!("./migrations/postgres"),
            ),
        };
        sqlx::any::install_default_drivers();
        let pool = AnyPool::connect(&url).await.unwrap();
        let all = migrator.migrations.clone();
        migrator.migrations = all
            .iter()
            .filter(|m| m.version < 28)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        migrator.run(&pool).await.unwrap();
        for (id, source, types) in [
            ("full", "ui", SIX),
            ("partial", "ui", FOUR),
            ("config", "config", SIX),
            ("done", "ui", EIGHT),
            ("seven", "ui", SEVEN),
        ] {
            sqlx::query(
                "INSERT INTO notification_channels (id, name, kind, source, enabled, types_json, \
                 config_json, created_at, updated_at) VALUES ($1, $1, 'webhook', $2, 1, $3, '{}', 0, 0)",
            )
            .bind(id)
            .bind(source)
            .bind(types)
            .execute(&pool)
            .await
            .unwrap();
        }

        migrator.migrations = all
            .iter()
            .filter(|m| m.version < 29)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        migrator.run(&pool).await.unwrap();

        let types = |id: &'static str| {
            let pool = pool.clone();
            async move {
                text(
                    &pool,
                    "SELECT types_json FROM notification_channels WHERE id = $1",
                    id,
                )
                .await
                .unwrap()
            }
        };
        let full: Vec<String> = serde_json::from_str(&types("full").await).unwrap();
        assert_eq!(full.len(), 8, "[{}]", backend.name());
        assert_eq!(full[6], "snapshot_quarantined");
        assert_eq!(full[7], "repository_unavailable");
        assert_eq!(types("partial").await, FOUR);
        assert_eq!(types("config").await, SIX);
        assert_eq!(types("done").await, EIGHT);
        assert_eq!(types("seven").await, SEVEN);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn migration_0029_gives_channels_with_every_type_the_security_alerts() {
    const SIX: &str = r#"["backup_failed","no_recent_backup","agent_offline","check_failed","certificate_expiring","agent_update_failed"]"#;
    const FOUR: &str = r#"["backup_failed","no_recent_backup","agent_offline","check_failed"]"#;
    const EIGHT: &str = r#"["backup_failed","no_recent_backup","agent_offline","check_failed","certificate_expiring","agent_update_failed","snapshot_quarantined","repository_unavailable"]"#;
    const TEN: &str = r#"["backup_failed","no_recent_backup","agent_offline","check_failed","certificate_expiring","agent_update_failed","snapshot_quarantined","repository_unavailable","agent_session_conflict","agent_certificate_rejected"]"#;
    for backend in backends() {
        let dir = tempfile::tempdir().unwrap();
        let (url, mut migrator) = match &backend {
            Backend::Sqlite => (
                format!("sqlite://{}/old.db?mode=rwc", dir.path().display()),
                sqlx::migrate!("./migrations/sqlite"),
            ),
            Backend::Postgres(admin) => (
                common::create_postgres_database(admin).await,
                sqlx::migrate!("./migrations/postgres"),
            ),
        };
        sqlx::any::install_default_drivers();
        let pool = AnyPool::connect(&url).await.unwrap();
        let all = migrator.migrations.clone();
        migrator.migrations = all
            .iter()
            .filter(|m| m.version < 29)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        migrator.run(&pool).await.unwrap();
        for (id, source, types) in [
            ("full", "ui", EIGHT),
            ("partial", "ui", FOUR),
            ("six", "ui", SIX),
            ("config", "config", EIGHT),
            ("done", "ui", TEN),
        ] {
            sqlx::query(
                "INSERT INTO notification_channels (id, name, kind, source, enabled, types_json, \
                 config_json, created_at, updated_at) VALUES ($1, $1, 'webhook', $2, 1, $3, '{}', 0, 0)",
            )
            .bind(id)
            .bind(source)
            .bind(types)
            .execute(&pool)
            .await
            .unwrap();
        }

        migrator.migrations = all;
        migrator.run(&pool).await.unwrap();

        let types = |id: &'static str| {
            let pool = pool.clone();
            async move {
                text(
                    &pool,
                    "SELECT types_json FROM notification_channels WHERE id = $1",
                    id,
                )
                .await
                .unwrap()
            }
        };
        let full: Vec<String> = serde_json::from_str(&types("full").await).unwrap();
        assert_eq!(full.len(), 10, "[{}]", backend.name());
        assert_eq!(full[8], "agent_session_conflict");
        assert_eq!(full[9], "agent_certificate_rejected");
        assert_eq!(types("partial").await, FOUR);
        assert_eq!(types("six").await, SIX);
        assert_eq!(types("config").await, EIGHT);
        assert_eq!(types("done").await, TEN);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_jobs_name_a_busy_host_only_when_it_is_busy() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = fleet(&ts, &["h1-idle", "h2-busy"]).await;
        let pool = &ts.state().db.pool;
        let arch = arch_of(pool, &hosts[0].id).await;
        let _online = connect_all(&ts, &hosts).await;
        sqlx::query(
            "INSERT INTO jobs (id, host_id, source, state, spec, queued_at) \
             VALUES ('job-1', $1, 'manual', 'running', $2, $3)",
        )
        .bind(&hosts[1].id)
        .bind(Vec::<u8>::new())
        .bind(now_ms())
        .execute(pool)
        .await
        .unwrap();
        insert_rollout(pool, "r1", "paused").await;
        for (id, host) in [("u-idle", &hosts[0]), ("u-busy", &hosts[1])] {
            insert_update(
                pool, id, "r1", &host.id, &arch, "0.0.1", "99.0.0", 1, "queued",
            )
            .await;
        }

        let job = |id: &'static str| {
            let ts = &ts;
            async move {
                let path = format!("/api/v1/jobs/{id}");
                let response = rest(ts, &["jobs:read"], &path, None).await;
                assert_eq!(response.status(), 200);
                response.json::<serde_json::Value>().await.unwrap()
            }
        };
        assert!(
            job("u-idle").await["queue_reason"].is_null(),
            "[{}]",
            backend.name()
        );
        assert_eq!(job("u-busy").await["queue_reason"], "host_busy");

        let page: serde_json::Value =
            rest(&ts, &["jobs:read"], "/api/v1/jobs?kind=agent_update", None)
                .await
                .json()
                .await
                .unwrap();
        let reason = |id: &str| {
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["id"] == id)
                .unwrap()["queue_reason"]
                .clone()
        };
        assert!(reason("u-idle").is_null());
        assert_eq!(reason("u-busy"), "host_busy");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_status_counts_rollbacks_since_the_last_resume() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = fleet(&ts, &["h1", "h2"]).await;
        let pool = &ts.state().db.pool;
        let arch = arch_of(pool, &hosts[0].id).await;
        insert_rollout(pool, "r1", "paused").await;
        for (id, host) in [("u1", &hosts[0]), ("u2", &hosts[1])] {
            insert_update(
                pool,
                id,
                "r1",
                &host.id,
                &arch,
                "0.0.1",
                "99.0.0",
                0,
                "rolled_back",
            )
            .await;
        }
        sqlx::query("UPDATE agent_rollouts SET pause_baseline = 1")
            .execute(pool)
            .await
            .unwrap();

        let body = status(&ts).await;
        assert_eq!(body["rollout"]["rolled_back"], 2, "[{}]", backend.name());
        assert_eq!(body["rollout"]["rollbacks_since_resume"], 1);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_status_hides_old_and_dismissed_rollouts() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        fleet(&ts, &["host-a"]).await;
        let pool = &ts.state().db.pool;
        insert_rollout(pool, "old", "done").await;
        sqlx::query("UPDATE agent_rollouts SET finished_at = $1 WHERE id = 'old'")
            .bind(now_ms() - 25 * 3_600_000)
            .execute(pool)
            .await
            .unwrap();
        assert!(
            status(&ts).await["rollout"].is_null(),
            "[{}]",
            backend.name()
        );

        insert_rollout(pool, "fresh", "done").await;
        sqlx::query("UPDATE agent_rollouts SET finished_at = $1 WHERE id = 'fresh'")
            .bind(now_ms() - 3_600_000)
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(status(&ts).await["rollout"]["id"], "fresh");

        let dismissed = rest(
            &ts,
            &["agents:update"],
            "/api/v1/agent-updates/fresh/dismiss",
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(dismissed.status(), 204);
        assert!(status(&ts).await["rollout"].is_null());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_actions_move_the_rollout() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = fleet(&ts, &["host-a"]).await;
        let pool = &ts.state().db.pool;
        let arch = arch_of(pool, &hosts[0].id).await;
        publish(&ts, &arch, "99.0.0", b"binary");
        setup_host(pool, &hosts[0], "0.0.1", None, true).await;
        let started = start(&ts, serde_json::json!({ "outdated": true })).await;
        assert_eq!(started.status(), 202, "[{}]", backend.name());
        let started: serde_json::Value = started.json().await.unwrap();
        let id = started["id"].as_str().unwrap().to_string();
        let action = |name: &str, scopes: &'static [&'static str]| {
            let path = format!("/api/v1/agent-updates/{id}/{name}");
            let ts = &ts;
            async move {
                rest(ts, scopes, &path, Some(serde_json::json!({})))
                    .await
                    .status()
                    .as_u16()
            }
        };
        let rollout_state = || text(pool, ROLLOUT_STATE, &id);

        for name in ["pause", "resume", "cancel", "dismiss"] {
            assert_eq!(action(name, &["hosts:write"]).await, 403, "{name}");
        }
        assert_eq!(rollout_state().await.as_deref(), Some("running"));

        assert_eq!(action("resume", &["agents:update"]).await, 409);
        assert_eq!(action("pause", &["agents:update"]).await, 204);
        assert_eq!(rollout_state().await.as_deref(), Some("paused"));
        assert_eq!(action("pause", &["agents:update"]).await, 409);

        insert_update(
            pool,
            "rb",
            &id,
            &hosts[0].id,
            &arch,
            "0.0.1",
            "99.0.0",
            0,
            "rolled_back",
        )
        .await;
        assert_eq!(action("resume", &["agents:update"]).await, 204);
        assert_eq!(rollout_state().await.as_deref(), Some("running"));
        let baseline = count(pool, "SELECT pause_baseline FROM agent_rollouts").await;
        assert_eq!(baseline, 1);

        assert_eq!(action("dismiss", &["agents:update"]).await, 409);
        assert_eq!(action("cancel", &["agents:update"]).await, 204);
        assert_eq!(rollout_state().await.as_deref(), Some("cancelled"));
        let queued = count(
            pool,
            "SELECT COUNT(*) FROM agent_updates WHERE state = 'queued'",
        )
        .await;
        assert_eq!(queued, 0);
        assert_eq!(action("cancel", &["agents:update"]).await, 409);
        assert_eq!(action("pause", &["agents:update"]).await, 409);
        assert_eq!(status(&ts).await["rollout"]["state"], "cancelled");
        assert_eq!(action("dismiss", &["agents:update"]).await, 204);
        assert!(status(&ts).await["rollout"].is_null());

        let unknown = rest(
            &ts,
            &["agents:update"],
            "/api/v1/agent-updates/nope/pause",
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(unknown.status(), 404);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_cancel_then_new_rollout() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = fleet(&ts, &["host-a", "host-b", "host-c"]).await;
        let pool = &ts.state().db.pool;
        let arch = arch_of(pool, &hosts[0].id).await;
        publish(&ts, &arch, "99.0.0", b"binary");
        for host in &hosts {
            setup_host(pool, host, "0.0.1", None, true).await;
        }
        let _online = Fake::connect(&ts, &hosts[0], "0.0.1").await;
        let first = start(&ts, serde_json::json!({ "outdated": true })).await;
        assert_eq!(first.status(), 202, "[{}]", backend.name());
        let first: serde_json::Value = first.json().await.unwrap();
        sqlx::query("UPDATE agent_updates SET state = 'assigned' WHERE host_id = $1")
            .bind(&hosts[0].id)
            .execute(pool)
            .await
            .unwrap();

        let path = format!(
            "/api/v1/agent-updates/{}/cancel",
            first["id"].as_str().unwrap()
        );
        let cancelled = rest(&ts, &["agents:update"], &path, Some(serde_json::json!({}))).await;
        assert_eq!(cancelled.status(), 204);

        let second = start(&ts, serde_json::json!({ "outdated": true })).await;
        assert_eq!(second.status(), 202, "[{}]", backend.name());
        assert_eq!(counts(second).await, (Some(2), Some(1)));
        let rows = count(
            pool,
            "SELECT COUNT(*) FROM agent_updates WHERE state IN ('queued', 'assigned')",
        )
        .await;
        assert_eq!(rows, 3);
        ts.stop().await;
    }
}

async fn signed_fleet(ts: &TestServer, names: &[&str]) -> Vec<Host> {
    let hosts = fleet(ts, names).await;
    let arch = arch_of(&ts.state().db.pool, &hosts[0].id).await;
    publish(ts, &arch, "99.0.0", b"binary");
    hosts
}

async fn connect_all(ts: &TestServer, hosts: &[Host]) -> Vec<Fake> {
    let mut agents = Vec::new();
    for host in hosts {
        agents.push(Fake::connect(ts, host, "0.0.1").await);
    }
    agents
}

async fn begin(ts: &TestServer, body: serde_json::Value) -> String {
    let started = start(ts, body).await;
    assert_eq!(started.status(), 202);
    let started: serde_json::Value = started.json().await.unwrap();
    started["id"].as_str().unwrap().to_string()
}

async fn begin_one(ts: &TestServer, host: &Host) -> String {
    begin(ts, serde_json::json!({ "hosts": [host.id] })).await
}

async fn action(ts: &TestServer, rollout: &str, name: &str) -> u16 {
    let path = format!("/api/v1/agent-updates/{rollout}/{name}");
    rest(ts, &["agents:update"], &path, Some(serde_json::json!({})))
        .await
        .status()
        .as_u16()
}

async fn number(pool: &AnyPool, sql: &'static str, id: &str) -> Option<i64> {
    sqlx::query(sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
        .try_get::<Option<i64>, _>(0)
        .unwrap()
}

const UPDATE_REASON: &str = "SELECT reason FROM agent_updates WHERE id = $1";

fn health_timeout(secs: u64) -> impl FnOnce(&mut lodestore_server::config::Config) {
    move |c| c.agent_updates.health_timeout = Duration::from_secs(secs)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_status_rejected_rolls_back() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = signed_fleet(&ts, &["host-a"]).await;
        let pool = &ts.state().db.pool;
        let mut agent = Fake::connect(&ts, &hosts[0], "0.0.1").await;
        let rollout = begin_one(&ts, &hosts[0]).await;
        let assign = agent.assign().await;

        let reason = "Signature check failed: key not trusted on this host.";
        agent
            .report(&assign.update_id, UpdateState::Rejected, reason)
            .await;
        wait_text(pool, UPDATE_STATE, &assign.update_id, "rolled_back").await;
        let stored = text(pool, UPDATE_REASON, &assign.update_id).await;
        assert_eq!(stored.as_deref(), Some(reason), "[{}]", backend.name());
        let finished = number(
            pool,
            "SELECT finished_at FROM agent_updates WHERE id = $1",
            &assign.update_id,
        )
        .await;
        assert!(finished.is_some());
        wait_text(pool, ROLLOUT_STATE, &rollout, "done").await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_status_busy_requeues_and_assigns_again() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = signed_fleet(&ts, &["host-a"]).await;
        let mut agent = Fake::connect(&ts, &hosts[0], "0.0.1").await;
        begin_one(&ts, &hosts[0]).await;
        let first = agent.assign().await;

        agent.report(&first.update_id, UpdateState::Busy, "").await;
        let second = agent.assign().await;
        assert_eq!(second.update_id, first.update_id, "[{}]", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_deadline_runs_from_the_switch() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, health_timeout(1)).await;
        let hosts = signed_fleet(&ts, &["host-a"]).await;
        let pool = &ts.state().db.pool;
        let mut agent = Fake::connect(&ts, &hosts[0], "0.0.1").await;
        begin_one(&ts, &hosts[0]).await;
        let assign = agent.assign().await;
        let id = assign.update_id.as_str();

        agent.report(id, UpdateState::Switching, "").await;
        wait_text(pool, UPDATE_STATE, id, "rolled_back").await;
        let reason = text(pool, UPDATE_REASON, id).await.unwrap();
        assert!(
            reason.starts_with("The new version did not connect within"),
            "{reason} [{}]",
            backend.name()
        );

        let own = "Health check failed: the new binary exited.";
        agent.report(id, UpdateState::RolledBack, own).await;
        assert_eq!(agent.ack(WAIT).await.as_deref(), Some(id));
        wait_text(pool, UPDATE_REASON, id, own).await;
        agent.report(id, UpdateState::RolledBack, own).await;
        assert_eq!(agent.ack(WAIT).await.as_deref(), Some(id));
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_slow_download_is_not_a_failure() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, health_timeout(1)).await;
        let hosts = signed_fleet(&ts, &["host-a"]).await;
        let pool = &ts.state().db.pool;
        let mut agent = Fake::connect(&ts, &hosts[0], "0.0.1").await;
        begin_one(&ts, &hosts[0]).await;
        let assign = agent.assign().await;
        let id = assign.update_id.as_str();

        for _ in 0..6 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            sqlx::query("UPDATE agent_updates SET progress_at = $1 WHERE id = $2")
                .bind(now_ms())
                .bind(id)
                .execute(pool)
                .await
                .unwrap();
        }
        assert_eq!(
            text(pool, UPDATE_STATE, id).await.as_deref(),
            Some("assigned"),
            "[{}]",
            backend.name()
        );
        fetch(&ts, &hosts[0], id, UpdateFile::Signature)
            .await
            .expect("still served");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_stalled_download_rolls_back() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, health_timeout(1)).await;
        let hosts = signed_fleet(&ts, &["host-a"]).await;
        let pool = &ts.state().db.pool;
        let mut agent = Fake::connect(&ts, &hosts[0], "0.0.1").await;
        begin_one(&ts, &hosts[0]).await;
        let assign = agent.assign().await;

        wait_text(pool, UPDATE_STATE, &assign.update_id, "rolled_back").await;
        let reason = text(pool, UPDATE_REASON, &assign.update_id).await.unwrap();
        assert!(
            reason.starts_with("The update download stopped for"),
            "{reason} [{}]",
            backend.name()
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_disconnect_requeues() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = signed_fleet(&ts, &["host-a"]).await;
        let pool = &ts.state().db.pool;
        let mut agent = Fake::connect(&ts, &hosts[0], "0.0.1").await;
        begin_one(&ts, &hosts[0]).await;
        let first = agent.assign().await;

        drop(agent);
        wait_text(pool, UPDATE_STATE, &first.update_id, "queued").await;
        let row =
            sqlx::query("SELECT assigned_at, progress_at, reason FROM agent_updates WHERE id = $1")
                .bind(&first.update_id)
                .fetch_one(pool)
                .await
                .unwrap();
        assert!(row.try_get::<Option<i64>, _>(0).unwrap().is_none());
        assert!(row.try_get::<Option<i64>, _>(1).unwrap().is_none());
        assert!(row.try_get::<Option<String>, _>(2).unwrap().is_none());

        let mut agent = Fake::connect(&ts, &hosts[0], "0.0.1").await;
        let second = agent.assign().await;
        assert_eq!(second.update_id, first.update_id, "[{}]", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_disconnect_is_not_a_rollback() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = signed_fleet(&ts, &["host-a", "host-b"]).await;
        let pool = &ts.state().db.pool;
        let mut agents = connect_all(&ts, &hosts).await;
        let rollout = begin(&ts, serde_json::json!({ "outdated": true })).await;
        for agent in &mut agents {
            agent.assign().await;
        }

        drop(agents);
        let gone = wait_until(Duration::from_secs(15), || async {
            count(
                pool,
                "SELECT COUNT(*) FROM agent_updates WHERE state = 'queued'",
            )
            .await
                == 2
        })
        .await;
        assert!(gone, "[{}]", backend.name());
        assert_eq!(
            count(
                pool,
                "SELECT COUNT(*) FROM agent_updates WHERE state = 'rolled_back'"
            )
            .await,
            0
        );
        assert_eq!(
            text(pool, ROLLOUT_STATE, &rollout).await.as_deref(),
            Some("running")
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_new_session_requeues_the_assignment() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = signed_fleet(&ts, &["host-a"]).await;
        let mut first = Fake::connect(&ts, &hosts[0], "0.0.1").await;
        begin_one(&ts, &hosts[0]).await;
        let assign = first.assign().await;

        let mut second = Fake::connect(&ts, &hosts[0], "0.0.1").await;
        let again = second.assign().await;
        assert_eq!(again.update_id, assign.update_id, "[{}]", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_rolled_back_report_wins_after_requeue() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = signed_fleet(&ts, &["host-a", "host-b"]).await;
        let pool = &ts.state().db.pool;
        let arch = arch_of(pool, &hosts[0].id).await;
        insert_rollout(pool, "r1", "paused").await;
        for (id, host) in [("u-a", &hosts[0]), ("u-b", &hosts[1])] {
            insert_update(
                pool, id, "r1", &host.id, &arch, "0.0.1", "99.0.0", 0, "queued",
            )
            .await;
        }
        let mut a = Fake::connect(&ts, &hosts[0], "0.0.1").await;

        a.report("u-a", UpdateState::RolledBack, "watchdog").await;
        assert_eq!(a.ack(WAIT).await.as_deref(), Some("u-a"));
        wait_text(pool, UPDATE_STATE, "u-a", "rolled_back").await;

        let _b = Fake::connect(&ts, &hosts[1], "99.0.0").await;
        wait_text(pool, UPDATE_STATE, "u-b", "succeeded").await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_status_of_a_foreign_update_changes_nothing() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = signed_fleet(&ts, &["host-a", "host-b"]).await;
        let pool = &ts.state().db.pool;
        let mut a = Fake::connect(&ts, &hosts[0], "0.0.1").await;
        let mut b = Fake::connect(&ts, &hosts[1], "0.0.1").await;
        begin_one(&ts, &hosts[0]).await;
        let assign = a.assign().await;
        let id = assign.update_id.as_str();

        for state in [
            UpdateState::Switching,
            UpdateState::Busy,
            UpdateState::Rejected,
            UpdateState::RolledBack,
            UpdateState::Confirmed,
        ] {
            b.report(id, state, "not mine").await;
        }
        assert_eq!(b.ack(Duration::from_millis(700)).await, None);
        assert_eq!(
            text(pool, UPDATE_STATE, id).await.as_deref(),
            Some("assigned"),
            "[{}]",
            backend.name()
        );
        let switched = number(
            pool,
            "SELECT switched_at FROM agent_updates WHERE id = $1",
            id,
        )
        .await;
        assert!(switched.is_none());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_status_confirmed_needs_the_target_version() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = signed_fleet(&ts, &["host-a"]).await;
        let pool = &ts.state().db.pool;
        let mut agent = Fake::connect(&ts, &hosts[0], "0.0.1").await;
        begin_one(&ts, &hosts[0]).await;
        let assign = agent.assign().await;
        let id = assign.update_id.as_str();

        agent.report(id, UpdateState::Confirmed, "").await;
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(
            text(pool, UPDATE_STATE, id).await.as_deref(),
            Some("assigned"),
            "[{}]",
            backend.name()
        );

        sqlx::query("UPDATE agents SET version = '99.0.0' WHERE id = $1")
            .bind(&hosts[0].id)
            .execute(pool)
            .await
            .unwrap();
        agent.report(id, UpdateState::Confirmed, "").await;
        wait_text(pool, UPDATE_STATE, id, "succeeded").await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_status_reason_is_clipped() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = signed_fleet(&ts, &["host-a"]).await;
        let pool = &ts.state().db.pool;
        let mut agent = Fake::connect(&ts, &hosts[0], "0.0.1").await;
        begin_one(&ts, &hosts[0]).await;
        let assign = agent.assign().await;

        agent
            .report(&assign.update_id, UpdateState::Rejected, &"x".repeat(600))
            .await;
        wait_text(pool, UPDATE_STATE, &assign.update_id, "rolled_back").await;
        let reason = text(pool, UPDATE_REASON, &assign.update_id).await.unwrap();
        assert_eq!(reason.len(), 500, "[{}]", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_busy_host_waits_for_its_job() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = signed_fleet(&ts, &["host-a"]).await;
        let pool = &ts.state().db.pool;
        let mut agent = Fake::connect(&ts, &hosts[0], "0.0.1").await;
        sqlx::query(
            "INSERT INTO jobs (id, host_id, source, state, spec, queued_at) \
             VALUES ('job-1', $1, 'manual', 'running', $2, $3)",
        )
        .bind(&hosts[0].id)
        .bind(Vec::<u8>::new())
        .bind(now_ms())
        .execute(pool)
        .await
        .unwrap();
        begin_one(&ts, &hosts[0]).await;

        agent.no_assign(Duration::from_millis(1500)).await;
        assert_eq!(
            count(
                pool,
                "SELECT COUNT(*) FROM agent_updates WHERE state = 'queued'"
            )
            .await,
            1,
            "[{}]",
            backend.name()
        );
        sqlx::query("UPDATE jobs SET state = 'succeeded' WHERE id = 'job-1'")
            .execute(pool)
            .await
            .unwrap();
        agent.assign().await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_offline_host_waits_for_its_return() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = signed_fleet(&ts, &["host-a"]).await;
        let pool = &ts.state().db.pool;
        setup_host(pool, &hosts[0], "0.0.1", None, true).await;
        begin_one(&ts, &hosts[0]).await;

        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(
            count(
                pool,
                "SELECT COUNT(*) FROM agent_updates WHERE state = 'queued'"
            )
            .await,
            1,
            "[{}]",
            backend.name()
        );
        let mut agent = Fake::connect(&ts, &hosts[0], "0.0.1").await;
        agent.assign().await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_cancelled_rollout_releases_a_disconnected_host() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = signed_fleet(&ts, &["host-a"]).await;
        let pool = &ts.state().db.pool;
        let mut agent = Fake::connect(&ts, &hosts[0], "0.0.1").await;
        let rollout = begin_one(&ts, &hosts[0]).await;
        let assign = agent.assign().await;

        assert_eq!(action(&ts, &rollout, "cancel").await, 204);
        assert_eq!(
            text(pool, UPDATE_STATE, &assign.update_id).await.as_deref(),
            Some("assigned")
        );
        drop(agent);
        wait_text(pool, UPDATE_STATE, &assign.update_id, "cancelled").await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_cancelled_rollout_still_times_out_a_stalled_download() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, health_timeout(1)).await;
        let hosts = signed_fleet(&ts, &["host-a"]).await;
        let pool = &ts.state().db.pool;
        let mut agent = Fake::connect(&ts, &hosts[0], "0.0.1").await;
        let rollout = begin_one(&ts, &hosts[0]).await;
        let assign = agent.assign().await;

        assert_eq!(action(&ts, &rollout, "cancel").await, 204);
        wait_text(pool, UPDATE_STATE, &assign.update_id, "rolled_back").await;
        let reason = text(pool, UPDATE_REASON, &assign.update_id).await.unwrap();
        assert!(
            reason.starts_with("The update download stopped for"),
            "[{}]",
            backend.name()
        );
        ts.stop().await;
    }
}

fn waves(
    canary: u32,
    wave: u32,
    pause_after: u32,
) -> impl FnOnce(&mut lodestore_server::config::Config) {
    move |c| {
        c.agent_updates.canary = canary;
        c.agent_updates.wave = wave;
        c.agent_updates.pause_after = pause_after;
    }
}

async fn upgraded(ts: &TestServer, host: &Host, slot: &mut Fake) {
    let fresh = Fake::connect(ts, host, "99.0.0").await;
    *slot = fresh;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_waves_start_with_the_canary() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, waves(2, 2, 1)).await;
        let hosts = signed_fleet(&ts, &["h1", "h2", "h3", "h4", "h5"]).await;
        let mut agents = connect_all(&ts, &hosts).await;
        begin(&ts, serde_json::json!({ "outdated": true })).await;

        agents[0].assign().await;
        agents[1].assign().await;
        for agent in &mut agents[2..] {
            agent.no_assign(Duration::from_millis(300)).await;
        }

        upgraded(&ts, &hosts[0], &mut agents[0]).await;
        agents[2].no_assign(Duration::from_millis(500)).await;
        upgraded(&ts, &hosts[1], &mut agents[1]).await;
        agents[2].assign().await;
        agents[3].assign().await;
        agents[4].no_assign(Duration::from_millis(500)).await;

        upgraded(&ts, &hosts[2], &mut agents[2]).await;
        upgraded(&ts, &hosts[3], &mut agents[3]).await;
        agents[4].assign().await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_wave_offline_canary_host_does_not_hold_the_next_wave() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, waves(2, 2, 1)).await;
        let hosts = signed_fleet(&ts, &["h1", "h2", "h3", "h4"]).await;
        let pool = &ts.state().db.pool;
        for host in &hosts[1..] {
            setup_host(pool, host, "0.0.1", None, true).await;
        }
        let mut first = Fake::connect(&ts, &hosts[0], "0.0.1").await;
        let rollout = begin(&ts, serde_json::json!({ "outdated": true })).await;

        first.assign().await;
        upgraded(&ts, &hosts[0], &mut first).await;
        let mut third = Fake::connect(&ts, &hosts[2], "0.0.1").await;
        let mut fourth = Fake::connect(&ts, &hosts[3], "0.0.1").await;
        third.assign().await;
        fourth.assign().await;

        let mut late = Fake::connect(&ts, &hosts[1], "0.0.1").await;
        late.assign().await;
        assert_eq!(
            text(pool, ROLLOUT_STATE, &rollout).await.as_deref(),
            Some("running"),
            "[{}]",
            backend.name()
        );
        ts.stop().await;
    }
}

async fn reject(agent: &mut Fake) {
    let assign = agent.assign().await;
    agent
        .report(&assign.update_id, UpdateState::Rejected, "nope")
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_pause_only_when_rollbacks_exceed_the_threshold() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, waves(2, 3, 2)).await;
        let hosts = signed_fleet(&ts, &["h1", "h2", "h3", "h4", "h5", "h6"]).await;
        let pool = &ts.state().db.pool;
        let mut agents = connect_all(&ts, &hosts).await;
        let rollout = begin(&ts, serde_json::json!({ "outdated": true })).await;

        agents[0].assign().await;
        upgraded(&ts, &hosts[0], &mut agents[0]).await;
        reject(&mut agents[1]).await;
        let third = agents[2].assign().await;
        let fourth = agents[3].assign().await;
        agents[4].assign().await;
        agents[2]
            .report(&third.update_id, UpdateState::Rejected, "nope")
            .await;
        wait_text(pool, UPDATE_STATE, &third.update_id, "rolled_back").await;
        assert_eq!(
            text(pool, ROLLOUT_STATE, &rollout).await.as_deref(),
            Some("running"),
            "two rollbacks do not exceed two [{}]",
            backend.name()
        );

        agents[3]
            .report(&fourth.update_id, UpdateState::Rejected, "nope")
            .await;
        wait_text(pool, ROLLOUT_STATE, &rollout, "paused").await;
        upgraded(&ts, &hosts[4], &mut agents[4]).await;
        agents[5].no_assign(Duration::from_secs(1)).await;
        assert_eq!(
            text(pool, ROLLOUT_STATE, &rollout).await.as_deref(),
            Some("paused")
        );

        assert_eq!(action(&ts, &rollout, "resume").await, 204);
        reject(&mut agents[5]).await;
        wait_text(pool, ROLLOUT_STATE, &rollout, "done").await;
        let baseline = count(pool, "SELECT pause_baseline FROM agent_rollouts").await;
        assert_eq!(baseline, 3);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_pause_after_one_stops_on_the_second_rollback() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, waves(3, 2, 1)).await;
        let hosts = signed_fleet(&ts, &["h1", "h2", "h3", "h4"]).await;
        let pool = &ts.state().db.pool;
        let mut agents = connect_all(&ts, &hosts).await;
        let rollout = begin(&ts, serde_json::json!({ "outdated": true })).await;

        let third = agents[2].assign().await;
        reject(&mut agents[0]).await;
        wait_until(Duration::from_secs(10), || async {
            count(
                pool,
                "SELECT COUNT(*) FROM agent_updates WHERE state = 'rolled_back'",
            )
            .await
                == 1
        })
        .await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(
            text(pool, ROLLOUT_STATE, &rollout).await.as_deref(),
            Some("running"),
            "[{}]",
            backend.name()
        );

        reject(&mut agents[1]).await;
        wait_text(pool, ROLLOUT_STATE, &rollout, "paused").await;
        assert_eq!(
            text(pool, UPDATE_STATE, &third.update_id).await.as_deref(),
            Some("assigned")
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_pause_defaults_stop_before_the_first_wave() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = signed_fleet(&ts, &["h1", "h2", "h3", "h4"]).await;
        let pool = &ts.state().db.pool;
        let mut agents = connect_all(&ts, &hosts).await;
        let rollout = begin(&ts, serde_json::json!({ "outdated": true })).await;

        reject(&mut agents[0]).await;
        reject(&mut agents[1]).await;
        wait_text(pool, ROLLOUT_STATE, &rollout, "paused").await;
        agents[2].no_assign(Duration::from_secs(1)).await;
        agents[3].no_assign(Duration::from_millis(300)).await;
        assert_eq!(
            count(
                pool,
                "SELECT COUNT(*) FROM agent_updates WHERE state = 'queued'"
            )
            .await,
            2,
            "[{}]",
            backend.name()
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_revoked_host_leaves_the_rollout() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, waves(2, 2, 1)).await;
        let hosts = signed_fleet(&ts, &["h1", "h2", "h3"]).await;
        let pool = &ts.state().db.pool;
        setup_host(pool, &hosts[1], "0.0.1", None, true).await;
        setup_host(pool, &hosts[2], "0.0.1", None, true).await;
        let mut agent = Fake::connect(&ts, &hosts[0], "0.0.1").await;
        let rollout = begin(
            &ts,
            serde_json::json!({ "hosts": [hosts[0].id, hosts[1].id] }),
        )
        .await;
        let assign = agent.assign().await;

        for host in &hosts[..2] {
            let path = format!("/api/v1/hosts/{}/revoke", host.id);
            let revoked = rest(&ts, &["hosts:write"], &path, Some(serde_json::json!({}))).await;
            assert_eq!(revoked.status(), 204, "[{}]", backend.name());
        }
        assert_eq!(
            text(pool, UPDATE_REASON, &assign.update_id)
                .await
                .as_deref(),
            Some("The host was revoked.")
        );
        assert_eq!(
            count(
                pool,
                "SELECT COUNT(*) FROM agent_updates WHERE state = 'cancelled'"
            )
            .await,
            2
        );
        wait_text(pool, ROLLOUT_STATE, &rollout, "done").await;
        begin_one(&ts, &hosts[2]).await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_database_allows_one_active_rollout() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = signed_fleet(&ts, &["h1", "h2", "h3", "h4"]).await;
        let pool = &ts.state().db.pool;
        for host in &hosts {
            setup_host(pool, host, "0.0.1", None, true).await;
        }
        let one = |host: &Host| serde_json::json!({ "hosts": [host.id] });
        let answers = tokio::join!(
            start(&ts, one(&hosts[0])),
            start(&ts, one(&hosts[1])),
            start(&ts, one(&hosts[2])),
            start(&ts, one(&hosts[3])),
        );
        let mut codes = [answers.0, answers.1, answers.2, answers.3].map(|r| r.status().as_u16());
        codes.sort();
        assert_eq!(codes, [202, 409, 409, 409], "[{}]", backend.name());

        for state in ["running", "paused"] {
            let second = sqlx::query(
                "INSERT INTO agent_rollouts (id, target_version, state, canary, wave_size, \
                 pause_after, created_by, created_at) VALUES ($1, '99.0.0', $2, 1, 1, 1, 't', 1)",
            )
            .bind(state)
            .bind(state)
            .execute(pool)
            .await;
            assert!(second.is_err(), "{state} [{}]", backend.name());
        }
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_host_past_the_target_is_cancelled() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = signed_fleet(&ts, &["host-a"]).await;
        let pool = &ts.state().db.pool;
        setup_host(pool, &hosts[0], "0.0.1", None, true).await;
        let rollout = begin_one(&ts, &hosts[0]).await;

        let mut agent = Fake::connect(&ts, &hosts[0], "100.0.0").await;
        let id = text(
            pool,
            "SELECT id FROM agent_updates WHERE host_id = $1",
            &hosts[0].id,
        )
        .await
        .unwrap();
        wait_text(pool, UPDATE_STATE, &id, "cancelled").await;
        agent.no_assign(Duration::from_millis(500)).await;
        assert_eq!(
            text(pool, UPDATE_REASON, &id).await.as_deref(),
            Some("The host already runs 100.0.0."),
            "[{}]",
            backend.name()
        );
        wait_text(pool, ROLLOUT_STATE, &rollout, "done").await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_missing_binary_pauses_until_it_is_back() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, waves(1, 1, 1)).await;
        let hosts = signed_fleet(&ts, &["h1", "h2", "h3"]).await;
        let pool = &ts.state().db.pool;
        let arch = arch_of(pool, &hosts[0].id).await;
        let mut agents = connect_all(&ts, &hosts).await;
        let rollout = begin(&ts, serde_json::json!({ "outdated": true })).await;

        agents[0].assign().await;
        publish(&ts, &arch, "100.0.0", b"binary");
        upgraded(&ts, &hosts[0], &mut agents[0]).await;
        wait_text(pool, ROLLOUT_STATE, &rollout, "paused").await;
        agents[1].no_assign(Duration::from_millis(500)).await;
        let body = status(&ts).await;
        assert_eq!(
            body["rollout"]["pause_reason"],
            format!("No signed agent 99.0.0 for {arch} on this server."),
            "[{}]",
            backend.name()
        );
        assert_eq!(body["rollout"]["rolled_back"], 0);
        assert_eq!(
            count(
                pool,
                "SELECT COUNT(*) FROM agent_updates WHERE state = 'queued'"
            )
            .await,
            2
        );

        publish(&ts, &arch, "99.0.0", b"binary");
        assert_eq!(action(&ts, &rollout, "resume").await, 204);
        agents[1].assign().await;
        assert!(status(&ts).await["rollout"]["pause_reason"].is_null());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_later_waves_wait_for_a_canary_success() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = signed_fleet(&ts, &["h1", "h2", "h3"]).await;
        let pool = &ts.state().db.pool;
        for host in &hosts {
            setup_host(pool, host, "0.0.1", None, true).await;
        }
        let rollout = begin(&ts, serde_json::json!({ "outdated": true })).await;
        let mut third = Fake::connect(&ts, &hosts[2], "0.0.1").await;

        third.no_assign(Duration::from_millis(700)).await;

        let mut second = Fake::connect(&ts, &hosts[1], "0.0.1").await;
        reject(&mut second).await;
        third.no_assign(Duration::from_millis(700)).await;
        assert_eq!(
            text(pool, ROLLOUT_STATE, &rollout).await.as_deref(),
            Some("running"),
            "a canary host is still due [{}]",
            backend.name()
        );

        let mut first = Fake::connect(&ts, &hosts[0], "0.0.1").await;
        first.assign().await;
        upgraded(&ts, &hosts[0], &mut first).await;
        third.assign().await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_failed_canary_pauses_until_resumed() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, waves(1, 2, 1)).await;
        let hosts = signed_fleet(&ts, &["h1", "h2", "h3"]).await;
        let pool = &ts.state().db.pool;
        let mut agents = connect_all(&ts, &hosts).await;
        let rollout = begin(&ts, serde_json::json!({ "outdated": true })).await;

        let canary = agents[0].assign().await;
        assert_eq!(action(&ts, &rollout, "pause").await, 204);
        assert_eq!(action(&ts, &rollout, "resume").await, 204);
        agents[0]
            .report(&canary.update_id, UpdateState::Rejected, "nope")
            .await;
        wait_text(pool, ROLLOUT_STATE, &rollout, "paused").await;
        agents[1].no_assign(Duration::from_millis(700)).await;
        assert_eq!(
            status(&ts).await["rollout"]["pause_reason"],
            "No canary host updated. Resume to go on without one.",
            "[{}]",
            backend.name()
        );

        assert_eq!(action(&ts, &rollout, "resume").await, 204);
        agents[1].assign().await;
        agents[2].assign().await;
        assert_eq!(
            text(pool, ROLLOUT_STATE, &rollout).await.as_deref(),
            Some("running")
        );
        assert!(status(&ts).await["rollout"]["pause_reason"].is_null());
        ts.stop().await;
    }
}

const WAVE_OF_HOST: &str = "SELECT wave FROM agent_updates WHERE host_id = $1";
const CREATED_AT: &str = "SELECT created_at FROM agent_rollouts WHERE id = $1";
const RESUMED_AT: &str = "SELECT resumed_at FROM agent_rollouts WHERE id = $1";
const HEALTH_MS: i64 = 150_000;

async fn offline_rollout(ts: &TestServer, names: &[&str]) -> (Vec<Host>, String) {
    let hosts = signed_fleet(ts, names).await;
    for host in &hosts {
        setup_host(&ts.state().db.pool, host, "0.0.1", None, true).await;
    }
    let rollout = begin(ts, serde_json::json!({ "outdated": true })).await;
    (hosts, rollout)
}

async fn assert_rollout(ts: &TestServer, rollout: &str, want: &str) {
    assert_eq!(
        text(&ts.state().db.pool, ROLLOUT_STATE, rollout)
            .await
            .as_deref(),
        Some(want)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_start_picks_ready_hosts_as_canaries() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, waves(1, 2, 1)).await;
        let hosts = signed_fleet(&ts, &["h1", "h2", "h3"]).await;
        let pool = &ts.state().db.pool;
        setup_host(pool, &hosts[0], "0.0.1", None, true).await;
        setup_host(pool, &hosts[1], "0.0.1", None, true).await;
        let mut ready = Fake::connect(&ts, &hosts[2], "0.0.1").await;
        begin(&ts, serde_json::json!({ "outdated": true })).await;

        ready.assign().await;
        assert_eq!(number(pool, WAVE_OF_HOST, &hosts[2].id).await, Some(0));
        assert_eq!(number(pool, WAVE_OF_HOST, &hosts[0].id).await, Some(1));
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_offline_canary_pauses_after_the_health_timeout() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, waves(1, 2, 1)).await;
        let (hosts, rollout) = offline_rollout(&ts, &["h1", "h2"]).await;
        let created = number(&ts.state().db.pool, CREATED_AT, &rollout)
            .await
            .unwrap();
        let mut waiting = Fake::connect(&ts, &hosts[1], "0.0.1").await;

        rollout::tick_once(ts.state(), created + HEALTH_MS - 1)
            .await
            .unwrap();
        assert_rollout(&ts, &rollout, "running").await;
        rollout::tick_once(ts.state(), created + HEALTH_MS + 1)
            .await
            .unwrap();
        assert_rollout(&ts, &rollout, "paused").await;
        assert_eq!(
            status(&ts).await["rollout"]["pause_reason"],
            "No canary host updated. Resume to go on without one.",
            "[{}]",
            backend.name()
        );

        assert_eq!(action(&ts, &rollout, "resume").await, 204);
        waiting.assign().await;
        let mut late = Fake::connect(&ts, &hosts[0], "0.0.1").await;
        late.assign().await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_stuck_canary_clock_restarts_on_resume() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, waves(1, 2, 1)).await;
        let (_hosts, rollout) = offline_rollout(&ts, &["h1", "h2"]).await;
        assert_eq!(action(&ts, &rollout, "pause").await, 204);
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert_eq!(action(&ts, &rollout, "resume").await, 204);
        let resumed = number(&ts.state().db.pool, RESUMED_AT, &rollout)
            .await
            .unwrap();

        rollout::tick_once(ts.state(), resumed + HEALTH_MS - 1)
            .await
            .unwrap();
        assert_rollout(&ts, &rollout, "running").await;
        rollout::tick_once(ts.state(), resumed + HEALTH_MS + 1)
            .await
            .unwrap();
        assert_rollout(&ts, &rollout, "paused").await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_stuck_canary_clock_starts_with_the_server() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, waves(1, 2, 1)).await;
        let (_hosts, rollout) = offline_rollout(&ts, &["h1", "h2"]).await;
        let started = ts.state().started_at_ms;
        sqlx::query("UPDATE agent_rollouts SET created_at = $1 WHERE id = $2")
            .bind(started - 2 * HEALTH_MS)
            .bind(&rollout)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();

        rollout::tick_once(ts.state(), started + HEALTH_MS - 1)
            .await
            .unwrap();
        assert_rollout(&ts, &rollout, "running").await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_busy_canary_is_not_stuck() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, waves(1, 2, 1)).await;
        let hosts = signed_fleet(&ts, &["h1", "h2"]).await;
        let pool = &ts.state().db.pool;
        setup_host(pool, &hosts[1], "0.0.1", None, true).await;
        let _canary = Fake::connect(&ts, &hosts[0], "0.0.1").await;
        sqlx::query(
            "INSERT INTO jobs (id, host_id, source, state, spec, queued_at) \
             VALUES ('job-1', $1, 'manual', 'running', $2, $3)",
        )
        .bind(&hosts[0].id)
        .bind(Vec::<u8>::new())
        .bind(now_ms())
        .execute(pool)
        .await
        .unwrap();
        let rollout = begin(&ts, serde_json::json!({ "outdated": true })).await;

        rollout::tick_once(ts.state(), now_ms() + 2 * HEALTH_MS)
            .await
            .unwrap();
        assert_rollout(&ts, &rollout, "running").await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_offline_host_without_a_later_wave_keeps_waiting() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, waves(1, 2, 1)).await;
        let (hosts, rollout) = offline_rollout(&ts, &["h1"]).await;

        rollout::tick_once(ts.state(), now_ms() + 2 * HEALTH_MS)
            .await
            .unwrap();
        assert_rollout(&ts, &rollout, "running").await;
        let mut back = Fake::connect(&ts, &hosts[0], "0.0.1").await;
        back.assign().await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_pause_does_not_count_cancelled_updates() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = fleet(&ts, &["h1", "h2", "h3"]).await;
        let pool = &ts.state().db.pool;
        let arch = arch_of(pool, &hosts[0].id).await;
        insert_rollout(pool, "r1", "paused").await;
        sqlx::query("UPDATE agent_rollouts SET pause_after = 1 WHERE id = 'r1'")
            .execute(pool)
            .await
            .unwrap();
        let rows = [
            ("u1", &hosts[0], "cancelled"),
            ("u2", &hosts[1], "rolled_back"),
            ("u3", &hosts[2], "queued"),
        ];
        for (id, host, state) in rows {
            insert_update(pool, id, "r1", &host.id, &arch, "0.0.1", "99.0.0", 0, state).await;
        }
        sqlx::query("UPDATE agent_rollouts SET state = 'running' WHERE id = 'r1'")
            .execute(pool)
            .await
            .unwrap();

        rollout::tick_once(ts.state(), now_ms()).await.unwrap();
        assert_eq!(
            text(pool, ROLLOUT_STATE, "r1").await.as_deref(),
            Some("running"),
            "[{}]",
            backend.name()
        );

        insert_update(
            pool,
            "u4",
            "r1",
            &hosts[1].id,
            &arch,
            "0.0.1",
            "99.0.0",
            0,
            "rolled_back",
        )
        .await;
        rollout::tick_once(ts.state(), now_ms()).await.unwrap();
        assert_eq!(
            text(pool, ROLLOUT_STATE, "r1").await.as_deref(),
            Some("paused")
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_update_race_claims_serialize() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let hosts = fleet(&ts, &["host-a"]).await;
        let pool = ts.state().db.pool.clone();
        let host = hosts[0].id.clone();
        let arch = arch_of(&pool, &host).await;
        insert_rollout(&pool, "r1", "paused").await;
        insert_update(
            &pool, "update-1", "r1", &host, &arch, "0.0.1", "99.0.0", 0, "queued",
        )
        .await;
        sqlx::query("UPDATE agent_rollouts SET state = 'running' WHERE id = 'r1'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO jobs (id, host_id, source, state, spec, queued_at) \
             VALUES ('job-1', $1, 'manual', 'queued', $2, $3)",
        )
        .bind(&host)
        .bind(Vec::<u8>::new())
        .bind(now_ms())
        .execute(&pool)
        .await
        .unwrap();

        let mut first = pool.begin().await.unwrap();
        lock_host(&mut first, &host).await.unwrap();
        assert!(
            claim_job(&mut first, "job-1", &host, now_ms())
                .await
                .unwrap()
        );

        let mut second = tokio::spawn({
            let pool = pool.clone();
            let host = host.clone();
            async move {
                let mut tx = pool.begin().await.unwrap();
                lock_host(&mut tx, &host).await.unwrap();
                let won = rollout::claim_update(&mut tx, "update-1", &host, now_ms())
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
                won
            }
        });
        let waiting = tokio::time::timeout(Duration::from_millis(500), &mut second).await;
        assert!(
            waiting.is_err(),
            "the second claim must wait for the host lock [{}]",
            backend.name()
        );

        first.commit().await.unwrap();
        assert!(!second.await.unwrap(), "the update claim sees the job");
        assert_eq!(
            text(&pool, UPDATE_STATE, "update-1").await.as_deref(),
            Some("queued")
        );
        let job = text(&pool, "SELECT state FROM jobs WHERE id = $1", "job-1").await;
        assert_eq!(job.as_deref(), Some("assigned"));
        ts.stop().await;
    }
}
