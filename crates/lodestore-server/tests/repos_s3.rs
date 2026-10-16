mod common;

use std::collections::BTreeSet;
use std::path::Path;

use std::time::Duration;

use common::{Backend, TestServer, backends};
use lodestore_agent::{EnrollArgs, NetOptions, Paths, connect, tls};
use lodestore_format::crypto::Entropy;
use lodestore_format::keys::{KdfCost, WriteKeys};
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::store::policy::{agent_iam_policy_json, agent_minio_policy_json};
use lodestore_format::store::{BlobStore, RepoLocation, StaticCredentials, Store};
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_proto::PROTOCOL_VERSION;
use lodestore_proto::agent::v1::agent_client::AgentClient;
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{AgentMessage, Hello, Versions};
use lodestore_server::repos::db::{server_access, writer_material};
use lodestore_testkit::minio::MinioServer;
use serde_json::{Value, json};
use tokio_stream::wrappers::ReceiverStream;

const BUCKET: &str = "repo";
const PASSWORD: &str = "correct horse battery";
const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

struct Http {
    client: reqwest::Client,
    base: String,
    cookie: String,
    csrf: String,
}

impl Http {
    async fn login(ts: &TestServer) -> Http {
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
        Http {
            client,
            base,
            cookie,
            csrf: session["csrf_token"].as_str().unwrap().to_string(),
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
}

async fn body(response: reqwest::Response) -> Value {
    response.json().await.expect("a JSON body")
}

fn creds(pair: &(String, String)) -> Value {
    json!({ "access_key_id": pair.0, "secret_access_key": pair.1 })
}

fn s3_draft(
    minio: &MinioServer,
    bucket: &str,
    name: &str,
    server: &(String, String),
    agent: &(String, String),
) -> Value {
    json!({
        "name": name,
        "location": {
            "backend": "s3",
            "options": { "bucket": bucket, "endpoint": minio.endpoint(), "path_style": "true" },
        },
        "server_credentials": creds(server),
        "agent_credentials": creds(agent),
    })
}

async fn agent_user(minio: &MinioServer, bucket: &str) -> (String, String) {
    minio
        .create_user_with_policy(bucket, &agent_minio_policy_json(bucket, ""))
        .await
}

fn statuses(result: &Value) -> Vec<String> {
    result["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["status"].as_str().unwrap().to_string())
        .collect()
}

async fn create_s3(http: &Http, draft: &Value) -> Value {
    let started = http.post("/api/v1/repositories/wizard", draft).await;
    assert_eq!(started.status(), 201);
    let started = body(started).await;
    let key = started["recovery_key"].as_str().unwrap();
    let committed = http
        .post(
            &format!(
                "/api/v1/repositories/wizard/{}/commit",
                started["wizard_id"].as_str().unwrap()
            ),
            &json!({
                "password": PASSWORD,
                "recovery_key_last_group": key.rsplit('-').next().unwrap(),
            }),
        )
        .await;
    assert_eq!(committed.status(), 201);
    body(committed).await
}

async fn each_backend<F, Fut>(test: F)
where
    F: Fn(Backend) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    for backend in backends() {
        test(backend).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wizard_s3_write_only_key_can_overwrite_on_minio() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let draft = s3_draft(&minio, BUCKET, "cloud", &server, &agent);

        let checked = body(http.post("/api/v1/repositories/check", &draft).await).await;
        assert_eq!(
            statuses(&checked),
            ["pass", "pass", "pass", "warn", "neutral"],
            "[{}]",
            backend.name()
        );
        assert_eq!(checked["agent_key_state"], "can_overwrite");
        assert_eq!(checked["blocking"], false);

        let created = create_s3(&http, &draft).await;
        assert_eq!(created["agent_key_state"], "can_overwrite");
        let id = created["id"].as_str().unwrap();
        let detail = body(http.get(&format!("/api/v1/repositories/{id}")).await).await;
        assert_eq!(detail["agent_key_state"], "can_overwrite");
        assert_eq!(detail["unlocked"], true);
        assert_eq!(detail["location"]["options"]["bucket"], BUCKET);
        let text = detail.to_string();
        assert!(!text.contains(&server.1) && !text.contains(&agent.1));
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wizard_s3_agent_can_delete_persisted() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_bucket(BUCKET).await;
        let root = minio.root_credentials();
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let draft = s3_draft(&minio, BUCKET, "cloud", &root, &root);

        let checked = body(http.post("/api/v1/repositories/check", &draft).await).await;
        assert_eq!(
            statuses(&checked),
            ["pass", "pass", "pass", "warn", "neutral"]
        );
        assert_eq!(checked["blocking"], false);
        assert_eq!(checked["agent_key_state"], "can_delete");

        let created = create_s3(&http, &draft).await;
        assert_eq!(created["agent_key_state"], "can_delete");
        let id = created["id"].as_str().unwrap();
        let list = body(http.get("/api/v1/repositories").await).await;
        assert_eq!(list[0]["agent_key_state"], "can_delete");
        let detail = body(http.get(&format!("/api/v1/repositories/{id}")).await).await;
        assert_eq!(detail["agent_key_state"], "can_delete");
        assert_eq!(detail["checks"][3]["status"], "warn");
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wizard_s3_bad_server_key_blocks() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_bucket(BUCKET).await;
        let (user, _) = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let draft = s3_draft(
            &minio,
            BUCKET,
            "cloud",
            &(user, "not-the-secret".to_string()),
            &agent,
        );

        let checked = body(http.post("/api/v1/repositories/check", &draft).await).await;
        assert_eq!(checked["blocking"], true);
        assert_eq!(checked["checks"][0]["id"], "write");
        assert_eq!(checked["checks"][0]["status"], "fail");
        assert!(checked["checks"][0]["error"].is_string());

        let started = http.post("/api/v1/repositories/wizard", &draft).await;
        assert_eq!(started.status(), 409);
        assert_eq!(body(started).await["check"]["blocking"], true);
        let list = body(http.get("/api/v1/repositories").await).await;
        assert_eq!(list.as_array().unwrap().len(), 0);
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wizard_s3_object_lock_detected() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_locked_bucket("locked").await;
        minio.create_bucket("plain").await;
        let server = minio.root_credentials();
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;

        for (bucket, expected) in [("locked", "pass"), ("plain", "neutral")] {
            let agent = agent_user(&minio, bucket).await;
            let draft = s3_draft(&minio, bucket, "cloud", &server, &agent);
            let checked = body(http.post("/api/v1/repositories/check", &draft).await).await;
            assert_eq!(checked["checks"][4]["id"], "object_lock");
            assert_eq!(checked["checks"][4]["status"], expected, "{bucket}");
            assert_eq!(checked["blocking"], false, "{bucket}");
        }
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_policy_endpoint() {
    each_backend(|backend| async move {
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;

        let policy = body(http.get("/api/v1/agent-policy?bucket=b").await).await;
        assert_eq!(policy["iam_json"], agent_iam_policy_json("b", ""));
        let minio_commands = policy["minio_commands"].as_str().unwrap();
        assert!(minio_commands.contains("mc admin policy attach"));
        assert!(!minio_commands.contains("if-none-match"));
        assert!(
            policy["iam_json"]
                .as_str()
                .unwrap()
                .contains("s3:if-none-match")
        );
        let prefixed = body(
            http.get("/api/v1/agent-policy?bucket=b&prefix=team/a")
                .await,
        )
        .await;
        assert_eq!(prefixed["iam_json"], agent_iam_policy_json("b", "team/a"));
        assert_eq!(http.get("/api/v1/agent-policy?bucket=").await.status(), 422);
        ts.stop().await;
    })
    .await;
}

async fn listing(store: &Store) -> BTreeSet<String> {
    store
        .list("")
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.key)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connect_existing_repository() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let location = RepoLocation::new(
            "s3",
            [
                ("bucket", BUCKET),
                ("endpoint", minio.endpoint()),
                ("path_style", "true"),
            ],
        );
        let store = location
            .open(Some(StaticCredentials {
                access_key_id: server.0.clone(),
                secret_access_key: server.1.clone(),
            }))
            .unwrap();
        let mut entropy = Entropy::os();
        let pending = PendingInit::generate(
            &mut entropy,
            InitOptions {
                kdf: LOW_KDF,
                ..InitOptions::default()
            },
        )
        .unwrap();
        let recovery_key = pending.recovery_key().encode().to_string();
        pending
            .commit(&store, b"old-password-123", &mut entropy)
            .await
            .unwrap();
        let before = listing(&store).await;
        let key_slots = |set: &BTreeSet<String>| set.iter().filter(|k| k.starts_with("keys/")).count();
        assert_eq!(key_slots(&before), 2);

        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let draft = s3_draft(&minio, BUCKET, "legacy", &server, &agent);

        let checked = body(http.post("/api/v1/repositories/check", &draft).await).await;
        assert_eq!(checked["existing_repository"], true);
        let started = http.post("/api/v1/repositories/wizard", &draft).await;
        assert_eq!(started.status(), 409);

        let connect = |password: Option<&str>, key: Option<&str>| {
            json!({ "draft": draft, "password": password, "recovery_key": key })
        };
        let wrong = http
            .post("/api/v1/repositories/connect", &connect(Some("wrong-password-1"), None))
            .await;
        assert_eq!(wrong.status(), 422);
        let both = http
            .post(
                "/api/v1/repositories/connect",
                &connect(Some("old-password-123"), Some(&recovery_key)),
            )
            .await;
        assert_eq!(both.status(), 422);
        assert_eq!(listing(&store).await, before, "a refused connect writes nothing");

        let connected = http
            .post("/api/v1/repositories/connect", &connect(None, Some(&recovery_key)))
            .await;
        assert_eq!(connected.status(), 201);
        let connected = body(connected).await;
        assert_eq!(connected["name"], "legacy");

        let after = listing(&store).await;
        assert!(before.is_subset(&after), "no existing object may disappear");
        assert_eq!(after.len(), before.len() + 1);
        assert_eq!(key_slots(&after), 3);

        let id = connected["id"].as_str().unwrap();
        let detail = body(http.get(&format!("/api/v1/repositories/{id}")).await).await;
        assert_eq!(detail["unlocked"], true);

        let token = {
            let response = http
                .post("/api/v1/join-tokens", &json!({ "repository_id": id, "labels": [] }))
                .await;
            assert_eq!(response.status(), 201);
            body(response).await["token"].as_str().unwrap().to_string()
        };
        let paths = ts.agent_paths("legacy");
        lodestore_agent::enroll(EnrollArgs {
            token,
            paths: paths.clone(),
            hostname: Some("legacy-host".into()),
            net: NetOptions::default(),
        })
        .await
        .unwrap();
        let keys =
            WriteKeys::from_file_text(&std::fs::read_to_string(paths.writer_keys()).unwrap())
                .unwrap();
        keys.verify_config(&store.get("config").await.unwrap())
            .expect("the agent's keys fit the connected repository");
        ts.stop().await;
    })
    .await;
}

fn write_tree(root: &Path) {
    std::fs::create_dir_all(root.join("sub")).unwrap();
    let mut big = vec![0u8; 700_000];
    getrandom::fill(&mut big).unwrap();
    std::fs::write(root.join("big.bin"), &big).unwrap();
    std::fs::write(root.join("sub/small.txt"), "hello from the agent").unwrap();
}

#[derive(Clone)]
struct AgentState {
    paths: lodestore_agent::Paths,
}

fn backup_as_agent_and_restore(
    state: AgentState,
    root: (String, String),
    src: std::path::PathBuf,
    out: std::path::PathBuf,
) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let repository: Value =
            serde_json::from_slice(&std::fs::read(state.paths.repository_json()).unwrap()).unwrap();
        let options = repository["options"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
            .collect();
        let location = RepoLocation {
            backend: repository["backend"].as_str().unwrap().to_string(),
            options,
        };
        let agent_creds: Value =
            serde_json::from_slice(&std::fs::read(state.paths.s3_credentials()).unwrap()).unwrap();
        let agent_store = location
            .open(Some(StaticCredentials {
                access_key_id: agent_creds["access_key_id"].as_str().unwrap().to_string(),
                secret_access_key: agent_creds["secret_access_key"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            }))
            .unwrap();
        let config =
            lodestore_format::config::decode(&agent_store.get("config").await.unwrap()).unwrap();
        let keys =
            WriteKeys::from_file_text(&std::fs::read_to_string(state.paths.writer_keys()).unwrap())
                .unwrap();
        let mut writer = Writer::new(&agent_store, keys, &config).await.unwrap();
        let outcome = writer
            .backup(
                std::slice::from_ref(&src),
                BackupOptions::default(),
                &|_| {},
            )
            .await
            .unwrap();
        assert!(!outcome.incomplete);

        let root_store = location
            .open(Some(StaticCredentials {
                access_key_id: root.0,
                secret_access_key: root.1,
            }))
            .unwrap();
        let repo = Repository::open(&root_store, PASSWORD.as_bytes())
            .await
            .expect("the repository password opens what the agent wrote");
        let mut reader =
            Reader::open(&root_store, repo.read_keys().duplicate(), repo.config()).unwrap();
        reader
            .restore(
                &outcome.snapshot_id,
                &out,
                &RestoreOptions::default(),
                &|_| {},
            )
            .await
            .unwrap();
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wizard_s3_enroll_receives_working_keys() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let created = create_s3(&http, &s3_draft(&minio, BUCKET, "cloud", &server, &agent)).await;
        let id = created["id"].as_str().unwrap();

        let token = {
            let response = http
                .post(
                    "/api/v1/join-tokens",
                    &json!({ "repository_id": id, "labels": [] }),
                )
                .await;
            body(response).await["token"].as_str().unwrap().to_string()
        };
        let paths = ts.agent_paths("s3");
        lodestore_agent::enroll(EnrollArgs {
            token,
            paths: paths.clone(),
            hostname: Some("s3-host".into()),
            net: NetOptions::default(),
        })
        .await
        .unwrap();
        let delivered: Value =
            serde_json::from_slice(&std::fs::read(paths.s3_credentials()).unwrap()).unwrap();
        assert_eq!(delivered["access_key_id"], agent.0);

        let src = tempfile::tempdir().unwrap();
        write_tree(src.path());
        let out = tempfile::tempdir().unwrap();
        let (state, root, src_path, out_path) = (
            AgentState { paths },
            server.clone(),
            src.path().to_path_buf(),
            out.path().to_path_buf(),
        );
        tokio::task::spawn_blocking(move || {
            backup_as_agent_and_restore(state, root, src_path, out_path)
        })
        .await
        .unwrap();

        let canonical = src.path().canonicalize().unwrap();
        let restored = out.path().join(canonical.strip_prefix("/").unwrap());
        for rel in ["big.bin", "sub/small.txt"] {
            assert!(
                std::fs::read(src.path().join(rel)).unwrap()
                    == std::fs::read(restored.join(rel)).unwrap(),
                "{rel} differs after restore"
            );
        }
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wizard_experimental_backend_reports_error() {
    each_backend(|backend| async move {
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;

        let webdav = json!({
            "name": "dav",
            "location": { "backend": "webdav", "options": { "endpoint": ts.rest_url() } },
        });
        let checked = http.post("/api/v1/repositories/check", &webdav).await;
        assert_eq!(checked.status(), 200);
        let checked = body(checked).await;
        assert_eq!(checked["checks"][0]["id"], "write");
        assert_eq!(checked["checks"][0]["status"], "fail");
        assert!(
            !checked["checks"][0]["error"]
                .as_str()
                .unwrap_or_default()
                .is_empty()
        );
        assert_eq!(checked["blocking"], true);
        assert_eq!(
            http.post("/api/v1/repositories/wizard", &webdav)
                .await
                .status(),
            409
        );

        let azblob = json!({
            "name": "az",
            "location": { "backend": "azblob", "options": {} },
        });
        assert_eq!(
            http.post("/api/v1/repositories/check", &azblob)
                .await
                .status(),
            422
        );
        ts.stop().await;
    })
    .await;
}

async fn enroll_agent(ts: &TestServer, http: &Http, repository_id: &str, name: &str) -> Paths {
    let response = http
        .post(
            "/api/v1/join-tokens",
            &json!({ "repository_id": repository_id, "labels": [] }),
        )
        .await;
    assert_eq!(response.status(), 201);
    let token = body(response).await["token"].as_str().unwrap().to_string();
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

fn delivered_key(paths: &Paths) -> String {
    let creds: Value =
        serde_json::from_slice(&std::fs::read(paths.s3_credentials()).unwrap()).unwrap();
    creds["access_key_id"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_key_replace_rejects_unverified() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let created = create_s3(&http, &s3_draft(&minio, BUCKET, "cloud", &server, &agent)).await;
        let id = created["id"].as_str().unwrap();

        let read_config_only = json!({
            "Version": "2012-10-17",
            "Statement": [{
                "Effect": "Allow",
                "Action": ["s3:GetObject"],
                "Resource": [format!("arn:aws:s3:::{BUCKET}/config")],
            }],
        });
        let no_write = minio
            .create_user_with_policy(BUCKET, &read_config_only.to_string())
            .await;

        let verified = http
            .post(
                &format!("/api/v1/repositories/{id}/agent-key/verify"),
                &creds(&no_write),
            )
            .await;
        assert_eq!(verified.status(), 200);
        let verified = body(verified).await;
        assert_eq!(verified["check"]["status"], "fail");
        assert_eq!(verified["agent_key_state"], "unchecked");

        let replaced = http
            .put(
                &format!("/api/v1/repositories/{id}/agent-key"),
                &creds(&no_write),
            )
            .await;
        assert_eq!(replaced.status(), 422);
        let message = body(replaced).await["message"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            message.starts_with("The new key could not be verified:"),
            "{message}"
        );
        assert!(
            message.ends_with("The current key is unchanged."),
            "{message}"
        );

        let kept = server_access(ts.state(), id)
            .await
            .unwrap()
            .agent_creds
            .unwrap();
        assert_eq!(kept.access_key_id, agent.0);
        let material = writer_material(ts.state(), id).await.unwrap();
        assert_eq!(material.agent_creds.unwrap().access_key_id, agent.0);
        let detail = body(http.get(&format!("/api/v1/repositories/{id}")).await).await;
        assert_eq!(detail["agent_key_state"], "can_overwrite");
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_key_replace_pushes_to_online_agents() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let created = create_s3(&http, &s3_draft(&minio, BUCKET, "cloud", &server, &agent)).await;
        let id = created["id"].as_str().unwrap();

        let online = enroll_agent(&ts, &http, id, "online").await;
        let _offline = enroll_agent(&ts, &http, id, "offline").await;
        assert_eq!(delivered_key(&online), agent.0);

        let read = |p: std::path::PathBuf| std::fs::read_to_string(p).unwrap();
        let (ca, cert, key) = (read(online.ca()), read(online.cert()), read(online.key()));
        let channel = connect::channel(
            &ts.grpc_address(),
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
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        let mut stream = AgentClient::new(channel)
            .session(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();
        let first = stream.message().await.unwrap().unwrap();
        assert!(matches!(first.body, Some(ServerBody::HelloAck(_))));
        let current = stream.message().await.unwrap().unwrap();
        let Some(ServerBody::UpdateS3Credentials(current)) = current.body else {
            panic!("expected the current key on session start, got {current:?}");
        };
        assert_eq!(current.credentials.unwrap().access_key_id, agent.0);

        let new_key = agent_user(&minio, BUCKET).await;
        let replaced = http
            .put(
                &format!("/api/v1/repositories/{id}/agent-key"),
                &creds(&new_key),
            )
            .await;
        assert_eq!(replaced.status(), 200);
        let replaced = body(replaced).await;
        assert_eq!(replaced["online_notified"], 1);
        assert_eq!(replaced["offline"], 1);
        assert_eq!(replaced["agent_key_state"], "can_overwrite");

        let pushed = tokio::time::timeout(Duration::from_secs(10), stream.message())
            .await
            .expect("the online agent is told about the new key")
            .unwrap()
            .unwrap();
        let Some(ServerBody::UpdateS3Credentials(update)) = pushed.body else {
            panic!("expected UpdateS3Credentials, got {pushed:?}");
        };
        assert_eq!(update.credentials.unwrap().access_key_id, new_key.0);

        let later = enroll_agent(&ts, &http, id, "later").await;
        assert_eq!(delivered_key(&later), new_key.0);
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repos_recheck_updates_state() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_bucket(BUCKET).await;
        let root = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let created = create_s3(&http, &s3_draft(&minio, BUCKET, "cloud", &root, &agent)).await;
        let id = created["id"].as_str().unwrap();
        let path = format!("/api/v1/repositories/{id}");

        let rechecked = body(http.post(&format!("{path}/check"), &json!({})).await).await;
        assert_eq!(rechecked["agent_key_state"], "can_overwrite");

        let before = body(http.get(&path).await).await["checked_at"]
            .as_i64()
            .unwrap();
        let swapped = http.put(&format!("{path}/agent-key"), &creds(&root)).await;
        assert_eq!(swapped.status(), 200);
        assert_eq!(body(swapped).await["agent_key_state"], "can_delete");

        tokio::time::sleep(Duration::from_millis(20)).await;
        let rechecked = body(http.post(&format!("{path}/check"), &json!({})).await).await;
        assert_eq!(rechecked["agent_key_state"], "can_delete");
        assert_eq!(rechecked["checks"][3]["status"], "warn");
        let detail = body(http.get(&path).await).await;
        assert_eq!(detail["agent_key_state"], "can_delete");
        assert!(detail["checked_at"].as_i64().unwrap() > before);
        ts.stop().await;
    })
    .await;
}
