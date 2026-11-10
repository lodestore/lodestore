mod common;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Api, Backend, TestServer, backends, wait_until};
use lodestore_agent::{AgentError, EnrollArgs, NetOptions, Paths, RunArgs, connect, tls};
use lodestore_proto::PROTOCOL_VERSION;
use lodestore_proto::agent::v1::agent_client::AgentClient;
use lodestore_proto::agent::v1::agent_message::Body;
use lodestore_proto::agent::v1::enrollment_client::EnrollmentClient;
use lodestore_proto::agent::v1::{
    AgentMessage, EnrollRequest, GetTrustAnchorRequest, Hello, RenewRequest, Versions,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::Channel;

type AgentTask = JoinHandle<Result<(), AgentError>>;

struct Running {
    id: String,
    paths: Paths,
    task: AgentTask,
    shutdown: CancellationToken,
}

fn run_agent(paths: &Paths) -> (AgentTask, CancellationToken) {
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(lodestore_agent::run(RunArgs {
        paths: paths.clone(),
        net: NetOptions::default(),
        heartbeat: Duration::from_secs(1),
        shutdown: shutdown.clone(),
    }));
    (task, shutdown)
}

async fn enroll_only(
    ts: &TestServer,
    api: &Api,
    repo: &str,
    name: &str,
    labels: Value,
) -> (String, Paths) {
    let token = api.join_token(repo, labels).await;
    let paths = ts.agent_paths(name);
    let enrolled = lodestore_agent::enroll(EnrollArgs {
        token,
        paths: paths.clone(),
        hostname: Some(format!("host-{name}")),
        net: NetOptions::default(),
    })
    .await
    .expect("enroll");
    (enrolled.agent_id, paths)
}

async fn start_agent(ts: &TestServer, api: &Api, repo: &str, name: &str) -> Running {
    let (id, paths) = enroll_only(ts, api, repo, name, json!([])).await;
    let (task, shutdown) = run_agent(&paths);
    let id_for_wait = id.clone();
    let online = wait_until(Duration::from_secs(10), || async {
        api.hosts()
            .await
            .iter()
            .any(|h| h["id"] == id_for_wait && h["status"] == "online")
    })
    .await;
    assert!(online, "the host never became online");
    Running {
        id,
        paths,
        task,
        shutdown,
    }
}

impl Running {
    async fn stop(self) {
        self.shutdown.cancel();
        let _ = self.task.await;
    }
}

fn snapshot(paths: &Paths) -> BTreeMap<String, Vec<u8>> {
    fn walk(dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                let digest = Sha256::digest(std::fs::read(&path).unwrap()).to_vec();
                out.insert(path.display().to_string(), digest);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(&paths.config_dir, &mut out);
    walk(&paths.state_dir, &mut out);
    out
}

async fn agent_channel(ts: &TestServer, paths: Option<&Paths>) -> Channel {
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
    let identity = paths.map(|p| {
        (
            std::fs::read_to_string(p.cert()).unwrap(),
            std::fs::read_to_string(p.key()).unwrap(),
        )
    });
    connect::channel(
        &address,
        "127.0.0.1",
        tls::pinned_config(
            &ca_pem,
            identity.as_ref().map(|(c, k)| (c.as_str(), k.as_str())),
        )
        .unwrap(),
        &NetOptions::default(),
    )
    .await
    .expect("agent channel")
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn new_csr() -> Vec<u8> {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let csr = rcgen::CertificateParams::default()
        .serialize_request(&key)
        .unwrap();
    csr.der().to_vec()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoke_disconnects_agent_immediately() {
    for backend in backends() {
        revoke_case(&backend).await;
    }
}

async fn revoke_case(backend: &Backend) {
    let ts = TestServer::start(backend).await;
    let api = ts.api().await;
    let repo = ts.create_local_repository("main").await;
    let agent = start_agent(&ts, &api, &repo, "revoke").await;
    // The agent records the server's distrusted-shard list right after it connects.
    let state = agent.paths.state_dir.clone();
    assert!(
        wait_until(Duration::from_secs(10), || async {
            state.join("distrusted-shards").exists()
                && !state.join(".distrusted-shards.tmp").exists()
        })
        .await,
        "[{}] the agent did not record the distrusted-shard list",
        backend.name()
    );
    let before = snapshot(&agent.paths);
    assert!(!before.is_empty());

    let response = api
        .post(&format!("/api/v1/hosts/{}/revoke", agent.id), &json!({}))
        .await;
    assert_eq!(response.status(), 204);

    let result = tokio::time::timeout(Duration::from_secs(3), agent.task)
        .await
        .unwrap_or_else(|_| {
            panic!(
                "[{}] the agent kept running after the revoke",
                backend.name()
            )
        })
        .unwrap();
    let error = result.expect_err("a revoked agent stops with an error");
    assert!(matches!(error, AgentError::Revoked), "{error}");
    assert_eq!(error.exit_code(), 77);

    let hosts = api.hosts().await;
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0]["status"], "revoked");
    assert_eq!(
        snapshot(&agent.paths),
        before,
        "agent files must not change"
    );

    let again = api
        .post(&format!("/api/v1/hosts/{}/revoke", agent.id), &json!({}))
        .await;
    assert_eq!(again.status(), 204);
    let missing = api.post("/api/v1/hosts/unknown/revoke", &json!({})).await;
    assert_eq!(missing.status(), 404);

    let (task, _shutdown) = run_agent(&agent.paths);
    let second = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the restarted agent exits at once")
        .unwrap();
    assert!(matches!(second, Err(AgentError::Revoked)));
    assert_eq!(snapshot(&agent.paths), before);

    let channel = agent_channel(&ts, Some(&agent.paths)).await;
    let hello = AgentMessage {
        body: Some(Body::Hello(Hello {
            versions: Some(Versions {
                min: PROTOCOL_VERSION,
                max: PROTOCOL_VERSION,
            }),
            agent_version: "test".to_string(),
            ..Default::default()
        })),
    };
    let status = AgentClient::new(channel.clone())
        .session(tokio_stream::iter(vec![hello]))
        .await
        .expect_err("a revoked agent cannot open a session");
    assert_eq!(status.code(), tonic::Code::PermissionDenied);
    assert_eq!(status.metadata().get("lodestore-error").unwrap(), "revoked");

    let status = AgentClient::new(channel)
        .renew(RenewRequest { csr_der: new_csr() })
        .await
        .expect_err("a revoked agent cannot renew");
    assert_eq!(status.code(), tonic::Code::PermissionDenied);
    assert_eq!(status.metadata().get("lodestore-error").unwrap(), "revoked");
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

async fn cert_serial(ts: &TestServer, id: &str) -> String {
    sqlx::query("SELECT cert_serial FROM agents WHERE id = $1")
        .bind(id)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .try_get("cert_serial")
        .unwrap()
}

fn read_identity(paths: &Paths) -> Value {
    serde_json::from_slice(&std::fs::read(paths.identity_json()).unwrap()).unwrap()
}

async fn conflict_alerts(ts: &TestServer, api: &Api) -> Vec<Value> {
    lodestore_server::alerts::tick(ts.state(), lodestore_server::db::now_ms())
        .await
        .unwrap();
    let body: Value = api
        .get("/api/v1/alerts?state=active")
        .await
        .json()
        .await
        .unwrap();
    body["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["type"] == "agent_session_conflict")
        .cloned()
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_two_live_sessions_raise_conflict_alert() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let repo = ts.create_local_repository("main").await;
        let api = ts.api().await;
        let (cloned, paths) = enroll_only(&ts, &api, &repo, "clone", json!([])).await;
        let (calm, calm_paths) = enroll_only(&ts, &api, &repo, "calm", json!([])).await;
        let channel = agent_channel(&ts, Some(&paths)).await;
        let calm_channel = agent_channel(&ts, Some(&calm_paths)).await;
        let open = |channel: Channel| async move {
            let (tx, rx) = mpsc::channel(4);
            tx.send(AgentMessage {
                body: Some(Body::Hello(Hello {
                    versions: Some(Versions {
                        min: PROTOCOL_VERSION,
                        max: PROTOCOL_VERSION,
                    }),
                    agent_version: "test".to_string(),
                    ..Default::default()
                })),
            })
            .await
            .unwrap();
            let inbound = AgentClient::new(channel)
                .session(ReceiverStream::new(rx))
                .await
                .expect("session")
                .into_inner();
            (tx, inbound)
        };

        let registry = &ts.state().registry;
        let mut held = vec![open(calm_channel.clone()).await];
        let ready = wait_until(Duration::from_secs(10), || async {
            registry.live.is_ready(&calm)
        })
        .await;
        assert!(ready, "the first session never became ready");
        held.push(open(calm_channel).await);
        assert!(
            conflict_alerts(&ts, &api).await.is_empty(),
            "one restart from the same address is not a conflict [{}]",
            backend.name()
        );

        for _ in 0..6 {
            held.push(open(channel.clone()).await);
        }
        let alerts = conflict_alerts(&ts, &api).await;
        assert_eq!(alerts.len(), 1, "[{}]", backend.name());
        assert_eq!(alerts[0]["host"]["id"], cloned.as_str());
        assert_eq!(alerts[0]["facts"]["ip"], "127.0.0.1");
        assert_eq!(alerts[0]["facts"]["previous_ip"], "127.0.0.1");
        drop(held);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renew_rotates_certificate_and_reconnects() {
    let logs = LogBuffer::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("the only test that logs");

    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.agent_cert_lifetime_secs = 9).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let (id, paths) = enroll_only(&ts, &api, &repo, "renew", json!([])).await;
        let serial_before = cert_serial(&ts, &id).await;
        let expires_before = read_identity(&paths)["expires_at"].as_i64().unwrap();
        let key_before = std::fs::read(paths.key()).unwrap();

        let (task, shutdown) = run_agent(&paths);
        let rotated = wait_until(Duration::from_secs(12), || async {
            cert_serial(&ts, &id).await != serial_before
                && read_identity(&paths)["expires_at"].as_i64().unwrap() > expires_before
        })
        .await;
        assert!(
            rotated,
            "[{}] the certificate was never renewed",
            backend.name()
        );

        assert!(read_identity(&paths)["expires_at"].as_i64().unwrap() > expires_before);
        assert_ne!(std::fs::read(paths.key()).unwrap(), key_before);
        assert_eq!(mode(&paths.key()), 0o600);

        tokio::time::sleep(Duration::from_secs(20)).await;
        let online = wait_until(Duration::from_secs(5), || async {
            api.hosts()
                .await
                .iter()
                .any(|h| h["id"] == id.as_str() && h["status"] == "online")
        })
        .await;
        assert!(online, "[{}] the host dropped offline", backend.name());

        let captured = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
        let started: Vec<&str> = captured
            .lines()
            .filter(|l| l.contains("agent session started") && l.contains(&id))
            .collect();
        assert!(started.len() >= 2, "{started:?}");
        assert!(started.iter().any(|l| l.contains(&serial_before)));
        assert!(
            started.iter().any(|l| !l.contains(&serial_before)),
            "{started:?}"
        );

        shutdown.cancel();
        let _ = task.await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrong_local_clock_does_not_stop_the_agent() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let (id, paths) = enroll_only(&ts, &api, &repo, "expired", json!([])).await;

        let mut identity = read_identity(&paths);
        identity["expires_at"] = json!(1_000_000);
        std::fs::write(paths.identity_json(), identity.to_string()).unwrap();

        let (task, shutdown) = run_agent(&paths);
        let online = wait_until(Duration::from_secs(10), || async {
            api.hosts()
                .await
                .iter()
                .any(|h| h["id"] == id.as_str() && h["status"] == "online")
        })
        .await;
        assert!(online, "[{}] the agent gave up", backend.name());
        assert!(
            !task.is_finished(),
            "an agent must not exit on a clock check"
        );
        shutdown.cancel();
        task.await.unwrap().expect("agent stops cleanly");
        ts.stop().await;
    }
}

struct Rest {
    client: reqwest::Client,
    base: String,
    cookie: String,
    csrf: String,
}

impl Rest {
    async fn login(ts: &TestServer) -> Rest {
        let base = ts.rest_url();
        let client = reqwest::Client::new();
        let response = client
            .post(format!("{base}/api/v1/auth/login"))
            .json(&json!({ "username": "admin", "password": common::ADMIN_PASSWORD }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 204);
        let cookie = response.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let session: Value = client
            .get(format!("{base}/api/v1/auth/session"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let csrf = session["csrf_token"].as_str().unwrap().to_string();
        Rest {
            client,
            base,
            cookie,
            csrf,
        }
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .header("cookie", &self.cookie)
            .send()
            .await
            .unwrap()
    }

    async fn send(&self, method: reqwest::Method, path: &str, body: &Value) -> reqwest::Response {
        self.client
            .request(method, format!("{}{path}", self.base))
            .header("cookie", &self.cookie)
            .header("x-csrf-token", &self.csrf)
            .json(body)
            .send()
            .await
            .unwrap()
    }

    async fn post(&self, path: &str, body: &Value) -> reqwest::Response {
        self.send(reqwest::Method::POST, path, body).await
    }

    async fn put(&self, path: &str, body: &Value) -> reqwest::Response {
        self.send(reqwest::Method::PUT, path, body).await
    }

    async fn detail(&self, id: &str) -> Value {
        let response = self.get(&format!("/api/v1/hosts/{id}")).await;
        assert_eq!(response.status(), 200);
        response.json().await.unwrap()
    }

    async fn token_status(&self, id: &str) -> Value {
        let response = self.get(&format!("/api/v1/join-tokens/{id}")).await;
        assert_eq!(response.status(), 200);
        response.json().await.unwrap()
    }

    async fn new_token(&self, repo: &str) -> (String, String) {
        let response = self
            .post(
                "/api/v1/join-tokens",
                &json!({ "repository_id": repo, "labels": [] }),
            )
            .await;
        assert_eq!(response.status(), 201);
        let body: Value = response.json().await.unwrap();
        (
            body["id"].as_str().unwrap().to_string(),
            body["token"].as_str().unwrap().to_string(),
        )
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn heartbeat_facts_in_host_detail() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let rest = Rest::login(&ts).await;
        let repo = ts.create_local_repository("main").await;
        let agent = start_agent(&ts, &api, &repo, "facts").await;

        let seen = wait_until(Duration::from_secs(10), || async {
            rest.detail(&agent.id).await["kernel"]
                .as_str()
                .is_some_and(|k| !k.is_empty())
        })
        .await;
        let detail = rest.detail(&agent.id).await;
        assert!(seen, "[{}] no facts arrived: {detail}", backend.name());
        assert_eq!(detail["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(detail["repository_name"], "main");
        let issued = detail["cert_issued_at"].as_i64().unwrap();
        let renewal = detail["next_renewal_at"].as_i64().unwrap();
        let expires = detail["cert_not_after"].as_i64().unwrap();
        assert!(issued < renewal && renewal < expires, "{detail}");
        if cfg!(target_os = "linux") {
            let mounts = detail["mounts"].as_array().unwrap();
            assert!(mounts.iter().any(|m| m["path"] == "/"), "{detail}");
        }

        assert_eq!(rest.get("/api/v1/hosts/unknown").await.status(), 404);
        agent.stop().await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn labels_edit_rules() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let rest = Rest::login(&ts).await;
        let repo = ts.create_local_repository("main").await;
        let (id, _) = enroll_only(&ts, &api, &repo, "labels", json!([])).await;
        let path = format!("/api/v1/hosts/{id}/labels");

        let labels =
            json!([{ "key": "env", "value": "prod" }, { "key": "team", "value": "Платформа" }]);
        let response = rest.put(&path, &json!({ "labels": labels })).await;
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["labels"], labels);
        assert_eq!(api.hosts().await[0]["labels"], labels);

        let bad = rest
            .put(
                &path,
                &json!({ "labels": [{ "key": "env prod", "value": "x" }] }),
            )
            .await;
        assert_eq!(bad.status(), 422);
        assert_eq!(api.hosts().await[0]["labels"], labels);

        let unknown = rest
            .put("/api/v1/hosts/unknown/labels", &json!({ "labels": [] }))
            .await;
        assert_eq!(unknown.status(), 404);

        assert_eq!(
            rest.post(&format!("/api/v1/hosts/{id}/revoke"), &json!({}))
                .await
                .status(),
            204
        );
        let revoked = rest.put(&path, &json!({ "labels": [] })).await;
        assert_eq!(revoked.status(), 409);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_token_labels_validated() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let rest = Rest::login(&ts).await;
        let repo = ts.create_local_repository("main").await;
        let (id, _) = enroll_only(&ts, &api, &repo, "rules", json!([])).await;
        let put_path = format!("/api/v1/hosts/{id}/labels");

        let too_many: Vec<Value> = (0..21)
            .map(|i| json!({ "key": format!("k{i}"), "value": "v" }))
            .collect();
        let cases = [
            (json!([{ "key": "env prod", "value": "x" }]), 422),
            (json!([{ "key": "k".repeat(64), "value": "x" }]), 422),
            (json!(too_many), 422),
            (json!([{ "key": "ключ", "value": "x" }]), 422),
            (
                json!([{ "key": "a", "value": "1" }, { "key": "a", "value": "2" }]),
                422,
            ),
            (json!([{ "key": "team", "value": "Платформа" }]), 200),
        ];
        for (labels, expected) in cases {
            let token = rest
                .post(
                    "/api/v1/join-tokens",
                    &json!({ "repository_id": repo, "labels": labels }),
                )
                .await;
            let put = rest.put(&put_path, &json!({ "labels": labels })).await;
            let created = if expected == 200 { 201 } else { 422 };
            assert_eq!(token.status(), created, "{labels}");
            assert_eq!(put.status(), expected, "{labels}");
            if expected == 422 {
                let body: Value = token.json().await.unwrap();
                assert_eq!(body["code"], "validation");
            }
        }
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn token_status_transitions() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.join_token_ttl_secs = 3).await;
        let api = ts.api().await;
        let rest = Rest::login(&ts).await;
        let repo = ts.create_local_repository("main").await;

        let (id, token) = rest.new_token(&repo).await;
        let pending = rest.token_status(&id).await;
        assert_eq!(pending["status"], "pending");
        assert_eq!(pending["host"], Value::Null);

        let enrolled = lodestore_agent::enroll(EnrollArgs {
            token: token.clone(),
            paths: ts.agent_paths("token"),
            hostname: Some("host-token".to_string()),
            net: NetOptions::default(),
        })
        .await
        .expect("enroll");
        let response = rest.get(&format!("/api/v1/join-tokens/{id}")).await;
        let text = response.text().await.unwrap();
        assert!(!text.contains(&token) && !text.contains("lode1."));
        let used: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(used["status"], "used");
        assert_eq!(used["host"]["hostname"], "host-token");
        assert_eq!(used["host"]["id"], enrolled.agent_id);
        assert_eq!(used["host"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(used["host"]["status"], "offline");

        let (other, _) = rest.new_token(&repo).await;
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert_eq!(rest.token_status(&other).await["status"], "expired");
        assert_eq!(rest.token_status(&id).await["status"], "used");
        assert_eq!(rest.get("/api/v1/join-tokens/unknown").await.status(), 404);
        let _ = api;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enroll_token_expired() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.join_token_ttl_secs = 1).await;
        let rest = Rest::login(&ts).await;
        let repo = ts.create_local_repository("main").await;
        let (id, token) = rest.new_token(&repo).await;
        tokio::time::sleep(Duration::from_secs(2)).await;

        let payload = token.strip_prefix("lode1.").unwrap();
        let decoded: Value = serde_json::from_slice(
            &data_encoding::BASE64URL_NOPAD
                .decode(payload.as_bytes())
                .unwrap(),
        )
        .unwrap();
        let channel = agent_channel(&ts, None).await;
        let status = EnrollmentClient::new(channel)
            .enroll(EnrollRequest {
                join_secret: decoded["s"].as_str().unwrap().to_string(),
                csr_der: new_csr(),
                hostname: "late".to_string(),
                os: "linux".to_string(),
                arch: "x86_64".to_string(),
                agent_version: "test".to_string(),
                versions: Some(Versions {
                    min: PROTOCOL_VERSION,
                    max: PROTOCOL_VERSION,
                }),
                ..Default::default()
            })
            .await
            .expect_err("an expired token is refused");
        assert_eq!(status.code(), tonic::Code::PermissionDenied);
        assert_eq!(
            status.metadata().get("lodestore-error").unwrap(),
            "token_invalid"
        );
        assert_eq!(rest.token_status(&id).await["status"], "expired");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_start_pushes_current_s3_key() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let key = json!({ "access_key_id": "AKIATESTKEY", "secret_access_key": "current-secret" });
        let secret_id = ts
            .state()
            .sealer
            .put_secret("repo-agent-creds", key.to_string().as_bytes())
            .await
            .unwrap();
        sqlx::query("UPDATE repositories SET agent_creds_secret_id = $1 WHERE id = $2")
            .bind(secret_id)
            .bind(&repo)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();

        let (_, paths) = enroll_only(&ts, &api, &repo, "s3", json!([])).await;
        std::fs::write(
            paths.s3_credentials(),
            json!({ "access_key_id": "OLD", "secret_access_key": "old" }).to_string(),
        )
        .unwrap();

        let (task, shutdown) = run_agent(&paths);
        let updated = wait_until(Duration::from_secs(10), || async {
            std::fs::read(paths.s3_credentials())
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .is_some_and(|v| v == key)
        })
        .await;
        assert!(updated, "[{}] the S3 key was not replaced", backend.name());
        assert_eq!(mode(&paths.s3_credentials()), 0o600);

        shutdown.cancel();
        let _ = task.await;
        ts.stop().await;
    }
}
