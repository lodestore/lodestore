#![allow(dead_code)]

use std::collections::BTreeMap;
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use lodestore_agent::{AgentError, EnrollArgs, NetOptions, Paths, RunArgs};
use lodestore_format::crypto::Entropy;
use lodestore_format::keys::KdfCost;
use lodestore_format::repo::{InitOptions, PendingInit};
use lodestore_format::store::LocalFsStore;
use lodestore_server::config::{Config, GrpcConfig, Secret};
use lodestore_server::repos::db::{NewRepository, RepoLocation, create_repository};
use lodestore_server::state::AppState;
use lodestore_server::{Listeners, ServerHandle, serve};
use serde_json::{Value, json};
use sqlx::{AnyConnection, Connection};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub const ADMIN_PASSWORD: &str = "test-admin-password";
const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

#[derive(Clone)]
pub enum Backend {
    Sqlite,
    Postgres(String),
}

impl Backend {
    pub fn name(&self) -> &'static str {
        match self {
            Backend::Sqlite => "sqlite",
            Backend::Postgres(_) => "postgres",
        }
    }
}

pub fn backends() -> Vec<Backend> {
    let mut all = vec![Backend::Sqlite];
    match std::env::var("LODESTORE_TEST_POSTGRES_URL") {
        Ok(url) if !url.is_empty() => all.push(Backend::Postgres(url)),
        _ => {
            if std::env::var("LODESTORE_TEST_REQUIRE_POSTGRES").as_deref() == Ok("1") {
                panic!("LODESTORE_TEST_POSTGRES_URL is required");
            }
        }
    }
    all
}

async fn create_postgres_database(admin_url: &str) -> String {
    sqlx::any::install_default_drivers();
    let name = format!(
        "t_{}",
        data_encoding::HEXLOWER.encode(&Entropy::os().bytes::<8>())
    );
    let mut conn = AnyConnection::connect(admin_url)
        .await
        .expect("connecting to the PostgreSQL admin database");
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&mut conn)
        .await
        .expect("creating a test database");
    let (base, _) = admin_url
        .rsplit_once('/')
        .expect("a database URL with a path");
    format!("{base}/{name}")
}

pub struct TestServer {
    pub dir: TempDir,
    pub config: Config,
    handle: Option<ServerHandle>,
}

async fn bind(addr: SocketAddr) -> TcpListener {
    for _ in 0..50 {
        if let Ok(l) = TcpListener::bind(addr).await {
            return l;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("could not bind {addr}");
}

impl TestServer {
    pub async fn start(backend: &Backend) -> TestServer {
        Self::start_with(backend, |_| {}).await
    }

    pub async fn start_with(backend: &Backend, tweak: impl FnOnce(&mut Config)) -> TestServer {
        let dir = tempfile::tempdir().expect("tempdir");
        let loopback = SocketAddr::from(([127, 0, 0, 1], 0));
        let rest = bind(loopback).await;
        let grpc = bind(loopback).await;
        let rest_addr = rest.local_addr().unwrap();
        let grpc_addr = grpc.local_addr().unwrap();

        let data_dir = dir.path().join("data");
        let database_url = match backend {
            Backend::Sqlite => format!("sqlite://{}/lodestore.db?mode=rwc", data_dir.display()),
            Backend::Postgres(admin) => create_postgres_database(admin).await,
        };
        let mut config = Config {
            listen: rest_addr,
            public_url: format!("http://{rest_addr}"),
            grpc: GrpcConfig {
                listen: grpc_addr,
                public_address: grpc_addr.to_string(),
                ..GrpcConfig::default()
            },
            grpc_public_address_defaulted: false,
            database_url,
            data_dir,
            unlock_key: Some(Secret::new(data_encoding::HEXLOWER.encode(&[7u8; 32]))),
            admin_password: Some(Secret::new(ADMIN_PASSWORD)),
            agent_dir: dir.path().join("agents"),
            ..Config::default()
        };
        tweak(&mut config);

        let handle = serve(config.clone(), Listeners { rest, grpc })
            .await
            .expect("starting the server");
        TestServer {
            dir,
            config,
            handle: Some(handle),
        }
    }

    pub async fn restart(&mut self) {
        self.handle
            .take()
            .expect("server is running")
            .shutdown()
            .await;
        let rest = bind(self.config.listen).await;
        let grpc = bind(self.config.grpc.listen).await;
        self.handle = Some(
            serve(self.config.clone(), Listeners { rest, grpc })
                .await
                .expect("restarting the server"),
        );
    }

    pub async fn stop(mut self) {
        if let Some(h) = self.handle.take() {
            h.shutdown().await;
        }
    }

    pub fn state(&self) -> &AppState {
        self.handle.as_ref().expect("server is running").state()
    }

    pub fn rest_url(&self) -> String {
        self.config.public_url.clone()
    }

    pub fn grpc_address(&self) -> String {
        self.config.grpc.public_address.clone()
    }

    pub async fn api(&self) -> Api {
        Api::login(&self.rest_url(), "admin", ADMIN_PASSWORD).await
    }

    pub fn agent_paths(&self, name: &str) -> Paths {
        let root = self.dir.path().join(format!("agent-{name}"));
        Paths {
            config_dir: root.join("etc"),
            state_dir: root.join("var"),
        }
    }

    pub fn local_repo_root(&self, name: &str) -> PathBuf {
        self.dir.path().join(format!("repo-{name}"))
    }

    pub async fn create_local_repository(&self, name: &str) -> String {
        let root = self.local_repo_root(name);
        let store = LocalFsStore::new(root.clone());
        let mut entropy = Entropy::os();
        let pending = PendingInit::generate(
            &mut entropy,
            InitOptions {
                kdf: LOW_KDF,
                ..InitOptions::default()
            },
        )
        .expect("generating a repository");
        let keys = pending.read_keys();
        let master = keys.master().clone();
        let format_repo_id = *keys.repo_id();
        let config_hash = lodestore_format::config::config_hash(pending.config_bytes());
        pending
            .commit(&store, b"test-password", &mut entropy)
            .await
            .expect("committing the repository");
        create_repository(
            self.state(),
            NewRepository {
                name: name.to_string(),
                location: RepoLocation {
                    backend: "local".to_string(),
                    options: BTreeMap::from([("path".to_string(), root.display().to_string())]),
                },
                experimental: false,
                format_repo_id,
                config_hash,
                master,
                server_slot_secret: None,
                server_creds: None,
                agent_creds: None,
            },
        )
        .await
        .expect("registering the repository")
    }
}

pub struct Api {
    client: reqwest::Client,
    base: String,
    cookie: String,
    pub csrf: String,
}

impl Api {
    pub async fn login(base: &str, username: &str, password: &str) -> Api {
        let client = reqwest::Client::new();
        let response = client
            .post(format!("{base}/api/v1/auth/login"))
            .json(&json!({ "username": username, "password": password }))
            .send()
            .await
            .expect("login request");
        assert_eq!(response.status(), 204, "login failed");
        let set_cookie = response
            .headers()
            .get("set-cookie")
            .expect("a session cookie")
            .to_str()
            .unwrap()
            .to_string();
        let cookie = set_cookie.split(';').next().unwrap().to_string();
        let session: Value = client
            .get(format!("{base}/api/v1/auth/session"))
            .header("cookie", &cookie)
            .send()
            .await
            .expect("session request")
            .json()
            .await
            .expect("session body");
        Api {
            client,
            base: base.to_string(),
            cookie,
            csrf: session["csrf_token"]
                .as_str()
                .expect("a csrf token")
                .to_string(),
        }
    }

    pub async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .header("cookie", &self.cookie)
            .send()
            .await
            .expect("GET request")
    }

    pub async fn post(&self, path: &str, body: &Value) -> reqwest::Response {
        self.client
            .post(format!("{}{path}", self.base))
            .header("cookie", &self.cookie)
            .header("x-csrf-token", &self.csrf)
            .json(body)
            .send()
            .await
            .expect("POST request")
    }

    pub async fn join_token(&self, repository_id: &str, labels: Value) -> String {
        let response = self
            .post(
                "/api/v1/join-tokens",
                &json!({ "repository_id": repository_id, "labels": labels }),
            )
            .await;
        assert_eq!(response.status(), 201);
        let body: Value = response.json().await.expect("token body");
        body["token"].as_str().expect("a token").to_string()
    }

    pub async fn hosts(&self) -> Vec<Value> {
        let response = self.get("/api/v1/hosts").await;
        assert_eq!(response.status(), 200);
        response.json().await.expect("hosts body")
    }
}

pub async fn wait_until<F, Fut>(timeout: Duration, mut f: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if f().await {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub const WAIT: Duration = Duration::from_secs(90);

const FINAL_STATES: [&str; 6] = [
    "succeeded",
    "warnings",
    "failed",
    "cancelled",
    "interrupted",
    "not_started",
];

type AgentTask = (CancellationToken, JoinHandle<Result<(), AgentError>>);

pub struct AgentEnv {
    server: Option<TestServer>,
    pub api: Api,
    pub host: String,
    pub hostname: String,
    pub repo: String,
    pub repo_name: String,
    pub paths: Paths,
    pub work: TempDir,
    agent: Option<AgentTask>,
    policies: AtomicUsize,
}

impl AgentEnv {
    pub async fn start(backend: &Backend) -> AgentEnv {
        let ts = TestServer::start(backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let labels = json!([
            { "key": "env", "value": "e2e" },
            { "key": "agent", "value": "host-e2e" },
        ]);
        let mut env = Self::enroll(&ts, api, &repo, "main", "host-e2e", "e2e", labels).await;
        env.server = Some(ts);
        env.start_agent().await;
        env
    }

    pub async fn start_on(
        ts: &TestServer,
        repo: &str,
        repo_name: &str,
        hostname: &str,
    ) -> AgentEnv {
        let labels = json!([{ "key": "agent", "value": hostname }]);
        let api = ts.api().await;
        let mut env = Self::enroll(ts, api, repo, repo_name, hostname, hostname, labels).await;
        env.start_agent().await;
        env
    }

    async fn enroll(
        ts: &TestServer,
        api: Api,
        repo: &str,
        repo_name: &str,
        hostname: &str,
        dir: &str,
        labels: Value,
    ) -> AgentEnv {
        let token = api.join_token(repo, labels).await;
        let paths = ts.agent_paths(dir);
        lodestore_agent::enroll(EnrollArgs {
            token,
            paths: paths.clone(),
            hostname: Some(hostname.to_string()),
            net: NetOptions::default(),
        })
        .await
        .expect("enroll");
        let host = api
            .hosts()
            .await
            .into_iter()
            .find(|h| h["hostname"] == hostname)
            .expect("the enrolled host is listed")["id"]
            .as_str()
            .unwrap()
            .to_string();
        AgentEnv {
            server: None,
            api,
            host,
            hostname: hostname.to_string(),
            repo: repo.to_string(),
            repo_name: repo_name.to_string(),
            paths,
            work: tempfile::tempdir().unwrap(),
            agent: None,
            policies: AtomicUsize::new(0),
        }
    }

    pub fn ts(&self) -> &TestServer {
        self.server
            .as_ref()
            .expect("this host does not own the server")
    }

    pub fn ts_mut(&mut self) -> &mut TestServer {
        self.server
            .as_mut()
            .expect("this host does not own the server")
    }

    pub async fn start_agent(&mut self) {
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(lodestore_agent::run(RunArgs {
            paths: self.paths.clone(),
            net: NetOptions::default(),
            heartbeat: Duration::from_secs(1),
            shutdown: shutdown.clone(),
        }));
        self.agent = Some((shutdown, handle));
        let (api, host) = (&self.api, &self.host);
        assert!(
            wait_until(WAIT, || async {
                api.hosts()
                    .await
                    .iter()
                    .any(|h| h["id"] == host.as_str() && h["status"] == "online")
            })
            .await,
            "the agent never became online"
        );
    }

    pub async fn stop_agent(&mut self) {
        let (shutdown, handle) = self.agent.take().expect("the agent is running");
        shutdown.cancel();
        tokio::time::timeout(WAIT, handle)
            .await
            .expect("the agent stops in time")
            .unwrap()
            .expect("the agent stops cleanly");
    }

    pub async fn stop(mut self) {
        if self.agent.is_some() {
            self.stop_agent().await;
        }
        if let Some(ts) = self.server.take() {
            ts.stop().await;
        }
    }

    pub async fn json(&self, path: &str) -> Value {
        let response = self.api.get(path).await;
        assert_eq!(response.status(), 200, "GET {path}");
        response.json().await.unwrap()
    }

    pub async fn wait_for(&self, id: &str, states: &[&str]) -> Value {
        let deadline = Instant::now() + WAIT;
        loop {
            let job = self.json(&format!("/api/v1/jobs/{id}")).await;
            if states.contains(&job["state"].as_str().unwrap()) {
                return job;
            }
            assert!(Instant::now() < deadline, "job {id} stuck: {job}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn wait_job(&self, id: &str) -> Value {
        self.wait_for(id, &FINAL_STATES).await
    }

    pub async fn events(&self, id: &str, query: &str) -> Vec<Value> {
        self.json(&format!("/api/v1/jobs/{id}/events{query}")).await["events"]
            .as_array()
            .unwrap()
            .clone()
    }

    pub async fn backup(&self, paths: &[&Path]) -> String {
        let n = self.policies.fetch_add(1, Ordering::Relaxed);
        let name = format!("backup-{}-{n}", self.hostname);
        let listed: String = paths
            .iter()
            .map(|p| format!("    - {}\n", p.display()))
            .collect();
        let yaml = format!(
            "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: {name}\n\
             spec:\n  repository: {}\n  selector:\n    matchLabels:\n      agent: {}\n  \
             paths:\n{listed}",
            self.repo_name, self.hostname
        );
        let response = self
            .api
            .post("/api/v1/policies", &json!({ "yaml": yaml }))
            .await;
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        assert_eq!(status, 201, "{body}");
        let response = self
            .api
            .post(&format!("/api/v1/policies/{name}/run"), &json!({}))
            .await;
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["jobs"][0]["created"], true, "{body}");
        let job = self.wait_job(body["jobs"][0]["id"].as_str().unwrap()).await;
        assert!(
            job["state"] == "succeeded" || job["state"] == "warnings",
            "{job}"
        );
        job["snapshot_id"]
            .as_str()
            .expect("a snapshot id")
            .to_string()
    }
}
