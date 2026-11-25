use crate::common;

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
use lodestore_proto::agent::v1::{AgentMessage, Hello, ServerMessage, Versions};
use lodestore_server::repos::db::{server_access, writer_material};
use lodestore_testkit::fault_proxy::{FaultAction, FaultPlan, FaultProxy, FaultRule};
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
        "accept_weaker_guarantee": true,
    })
}

fn without_consent(draft: &Value) -> Value {
    let mut draft = draft.clone();
    draft["accept_weaker_guarantee"] = json!(false);
    draft
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

        let refused = http
            .post("/api/v1/repositories/wizard", &without_consent(&draft))
            .await;
        assert_eq!(refused.status(), 409, "[{}]", backend.name());
        let refused = body(refused).await;
        assert_eq!(refused["code"], "weaker_guarantee");
        assert_eq!(
            refused["check"]["guarantee_level"],
            "host_can_overwrite_or_delete"
        );
        assert!(
            refused["check"]["guarantee_notes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n == "agent_can_overwrite")
        );
        assert!(
            refused["message"]
                .as_str()
                .unwrap()
                .contains("A host can replace backups"),
            "{refused}"
        );
        let list = body(http.get("/api/v1/repositories").await).await;
        assert_eq!(list.as_array().unwrap().len(), 0);

        let created = create_s3(&http, &draft).await;
        assert_eq!(created["agent_key_state"], "can_overwrite");
        assert_eq!(created["guarantee_level"], "host_can_overwrite_or_delete");
        assert_eq!(
            created["guarantee_accepted"],
            "host_can_overwrite_or_delete"
        );
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
async fn wizard_s3_versioned_bucket_needs_no_consent() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_versioned_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let draft = without_consent(&s3_draft(&minio, BUCKET, "cloud", &server, &agent));

        let checked = body(http.post("/api/v1/repositories/check", &draft).await).await;
        assert_eq!(
            checked["guarantee_level"],
            "overwrite_keeps_old_version",
            "[{}] {checked}",
            backend.name()
        );

        let created = create_s3(&http, &draft).await;
        assert_eq!(created["guarantee_level"], "overwrite_keeps_old_version");
        assert_eq!(created["guarantee_accepted"], "overwrite_keeps_old_version");
        ts.stop().await;
    })
    .await;
}

async fn refused_for_weaker_guarantee(http: &Http, draft: &Value) -> Value {
    let refused = http
        .post("/api/v1/repositories/wizard", &without_consent(draft))
        .await;
    assert_eq!(refused.status(), 409);
    let refused = body(refused).await;
    assert_eq!(refused["code"], "weaker_guarantee");
    refused
}

fn notes(result: &Value) -> Vec<&str> {
    result["guarantee_notes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wizard_s3_lifecycle_rule_caps_the_level() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_versioned_bucket(BUCKET).await;
        minio
            .mc(&[
                "ilm",
                "rule",
                "add",
                &format!("lode/{BUCKET}"),
                "--noncurrent-expire-days",
                "1",
            ])
            .await
            .expect("mc ilm rule add");
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let draft = s3_draft(&minio, BUCKET, "cloud", &server, &agent);

        let checked = body(http.post("/api/v1/repositories/check", &draft).await).await;
        assert_eq!(
            checked["guarantee_level"],
            "host_can_overwrite_or_delete",
            "[{}] {checked}",
            backend.name()
        );
        assert!(notes(&checked).contains(&"lifecycle_deletes"), "{checked}");
        let refused = refused_for_weaker_guarantee(&http, &draft).await;
        assert!(notes(&refused["check"]).contains(&"lifecycle_deletes"));

        let created = create_s3(&http, &draft).await;
        assert_eq!(created["guarantee_level"], "host_can_overwrite_or_delete");
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wizard_s3_unreadable_lifecycle_downgrades_old_version() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_versioned_bucket(BUCKET).await;
        let policy = json!({
            "Version": "2012-10-17",
            "Statement": [
                { "Effect": "Allow", "Action": ["s3:*"], "Resource": ["arn:aws:s3:::*"] },
                {
                    "Effect": "Deny",
                    "Action": ["s3:GetLifecycleConfiguration"],
                    "Resource": ["arn:aws:s3:::*"],
                },
            ],
        });
        let server = minio
            .create_user_with_policy(BUCKET, &policy.to_string())
            .await;
        let agent = agent_user(&minio, BUCKET).await;
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let draft = s3_draft(&minio, BUCKET, "cloud", &server, &agent);

        let checked = body(http.post("/api/v1/repositories/check", &draft).await).await;
        assert_eq!(
            checked["guarantee_level"],
            "host_can_overwrite_or_delete",
            "[{}] {checked}",
            backend.name()
        );
        assert!(
            notes(&checked).contains(&"lifecycle_unverified"),
            "{checked}"
        );
        let refused = refused_for_weaker_guarantee(&http, &draft).await;
        assert!(notes(&refused["check"]).contains(&"lifecycle_unverified"));
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wizard_unchecked_needs_consent() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let plan = FaultPlan::none().with(FaultRule {
            method: "DELETE",
            key_prefix: String::new(),
            action: FaultAction::NotImplemented,
            times: u32::MAX,
        });
        let proxy = FaultProxy::start(minio.endpoint(), plan).await;
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let mut draft = s3_draft(&minio, BUCKET, "cloud", &server, &agent);
        draft["location"]["options"]["endpoint"] = json!(proxy.endpoint());

        let refused = http
            .post("/api/v1/repositories/wizard", &without_consent(&draft))
            .await;
        assert_eq!(refused.status(), 409, "[{}]", backend.name());
        let refused = body(refused).await;
        assert_eq!(refused["code"], "weaker_guarantee", "{refused}");
        assert_eq!(refused["check"]["guarantee_level"], "unchecked");
        assert!(
            refused["message"]
                .as_str()
                .unwrap()
                .contains("Run the check again"),
            "{refused}"
        );
        let list = body(http.get("/api/v1/repositories").await).await;
        assert_eq!(list.as_array().unwrap().len(), 0);

        let created = create_s3(&http, &draft).await;
        assert_eq!(created["guarantee_level"], "unchecked");
        assert_eq!(created["guarantee_accepted"], "unchecked");
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
        assert_eq!(after.len(), before.len() + 2);
        assert_eq!(key_slots(&after), 3);
        assert_eq!(
            after
                .difference(&before)
                .filter(|key| key.starts_with("locks/attest-"))
                .count(),
            1,
            "connecting writes one upgrade marker"
        );

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
async fn repositories_refuse_backends_agents_cannot_write() {
    each_backend(|backend| async move {
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;

        for name in ["azblob", "gcs", "sftp", "webdav"] {
            let draft = json!({
                "name": "hidden",
                "location": { "backend": name, "options": { "endpoint": ts.rest_url() } },
            });
            let connect = json!({ "draft": draft, "password": "correct horse battery" });
            for (path, payload) in [
                ("/api/v1/repositories/check", &draft),
                ("/api/v1/repositories/wizard", &draft),
                ("/api/v1/repositories/connect", &connect),
            ] {
                let response = http.post(path, payload).await;
                assert_eq!(response.status(), 422, "{name} at {path}");
                let message = body(response).await["message"]
                    .as_str()
                    .unwrap()
                    .to_string();
                assert!(message.contains(name), "{name} at {path}: {message}");
            }
        }
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

async fn open_raw_session(grpc_address: String, paths: Paths) -> tonic::Streaming<ServerMessage> {
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

async fn next_s3_key(stream: &mut tonic::Streaming<ServerMessage>) -> String {
    let message = tokio::time::timeout(Duration::from_secs(10), stream.message())
        .await
        .expect("a storage key message")
        .unwrap()
        .unwrap();
    let Some(ServerBody::UpdateS3Credentials(update)) = message.body else {
        panic!("expected UpdateS3Credentials, got {message:?}");
    };
    update.credentials.unwrap().access_key_id
}

async fn wait_for_holders(lock: &std::sync::Arc<tokio::sync::Mutex<()>>, count: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while std::sync::Arc::strong_count(lock) != count {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the lock has {} holders, expected {count}",
            std::sync::Arc::strong_count(lock)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_key_replace_and_session_start_do_not_interleave() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let ts = TestServer::start(&backend).await;
        let http = std::sync::Arc::new(Http::login(&ts).await);
        let created = create_s3(&http, &s3_draft(&minio, BUCKET, "cloud", &server, &agent)).await;
        let id = created["id"].as_str().unwrap().to_string();
        let paths = enroll_agent(&ts, &http, &id, "online").await;
        let new_key = agent_user(&minio, BUCKET).await;
        let lock = ts.state().repos.access_lock(&id);

        let held = lock.lock().await;
        wait_for_holders(&lock, 2).await;
        let put = {
            let (http, path, key) = (
                std::sync::Arc::clone(&http),
                format!("/api/v1/repositories/{id}/agent-key"),
                creds(&new_key),
            );
            tokio::spawn(async move {
                let response = http.put(&path, &key).await;
                (response.status().as_u16(), body(response).await)
            })
        };
        wait_for_holders(&lock, 3).await;
        let stored = server_access(ts.state(), &id).await.unwrap();
        assert_eq!(stored.agent_creds.unwrap().access_key_id, agent.0);
        let opening = tokio::spawn(open_raw_session(ts.grpc_address(), paths.clone()));
        wait_for_holders(&lock, 4).await;
        assert!(!put.is_finished());
        drop(held);

        let (status, replaced) = put.await.unwrap();
        assert_eq!(status, 200, "{replaced}");
        assert_eq!(replaced["online_notified"], 1, "{replaced}");
        let mut stream = opening.await.unwrap();
        assert_eq!(next_s3_key(&mut stream).await, new_key.0);
        assert_eq!(next_s3_key(&mut stream).await, new_key.0);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), stream.message())
                .await
                .is_err(),
            "the old key must not follow the new one"
        );
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_key_replace_withholds_key_while_unavailable() {
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
        for _ in 0..2 {
            stream.message().await.unwrap().unwrap();
        }

        let good_hash = server_access(ts.state(), id)
            .await
            .unwrap()
            .config_hash
            .to_vec();
        sqlx::query("UPDATE repositories SET config_hash = $1 WHERE id = $2")
            .bind(vec![9u8; 32])
            .bind(id)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        let new_key = agent_user(&minio, BUCKET).await;
        let replaced = http
            .put(
                &format!("/api/v1/repositories/{id}/agent-key"),
                &creds(&new_key),
            )
            .await;
        assert_eq!(replaced.status(), 200);
        let replaced = body(replaced).await;
        assert_eq!(replaced["online_notified"], 0);
        assert_eq!(replaced["offline"], 1);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), stream.message())
                .await
                .is_err(),
            "an agent of an unavailable repository gets no storage key"
        );
        let stored = server_access(ts.state(), id).await.unwrap();
        assert_eq!(stored.agent_creds.unwrap().access_key_id, new_key.0);

        sqlx::query("UPDATE repositories SET config_hash = $1 WHERE id = $2")
            .bind(good_hash)
            .bind(id)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        let rechecked = http
            .post(&format!("/api/v1/repositories/{id}/check"), &json!({}))
            .await;
        assert_eq!(rechecked.status(), 200);
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

fn delete_not_implemented() -> FaultPlan {
    FaultPlan::none().with(FaultRule {
        method: "DELETE",
        key_prefix: String::new(),
        action: FaultAction::NotImplemented,
        times: u32::MAX,
    })
}

async fn point_at(ts: &TestServer, id: &str, endpoint: &str) {
    let pool = ts.state().db.pool.clone();
    let row = sqlx::query("SELECT location_json FROM repositories WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut options: Value =
        serde_json::from_str(&sqlx::Row::get::<String, _>(&row, "location_json")).unwrap();
    options["endpoint"] = json!(endpoint);
    sqlx::query("UPDATE repositories SET location_json = $1 WHERE id = $2")
        .bind(options.to_string())
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
}

async fn detail(http: &Http, id: &str) -> Value {
    body(http.get(&format!("/api/v1/repositories/{id}")).await).await
}

async fn recheck(http: &Http, id: &str) -> Value {
    let response = http
        .post(&format!("/api/v1/repositories/{id}/check"), &json!({}))
        .await;
    assert_eq!(response.status(), 200);
    body(response).await
}

fn bucket_path() -> String {
    format!("lode/{BUCKET}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recheck_raises_and_keeps_accepted() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_versioned_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let created = create_s3(&http, &s3_draft(&minio, BUCKET, "cloud", &server, &agent)).await;
        let id = created["id"].as_str().unwrap();
        assert_eq!(created["guarantee_level"], "overwrite_keeps_old_version");

        minio
            .mc(&["version", "suspend", &bucket_path()])
            .await
            .expect("mc version suspend");
        let lowered = recheck(&http, id).await;
        assert_eq!(lowered["guarantee_level"], "host_can_overwrite_or_delete");
        let shown = detail(&http, id).await;
        assert_eq!(shown["guarantee_level"], "host_can_overwrite_or_delete");
        assert_eq!(
            shown["guarantee_accepted"],
            "overwrite_keeps_old_version",
            "[{}]",
            backend.name()
        );

        minio
            .mc(&["version", "enable", &bucket_path()])
            .await
            .expect("mc version enable");
        recheck(&http, id).await;
        let shown = detail(&http, id).await;
        assert_eq!(shown["guarantee_level"], "overwrite_keeps_old_version");
        assert_eq!(shown["guarantee_accepted"], "overwrite_keeps_old_version");
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recheck_inconclusive_keeps_the_level() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_versioned_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let created = create_s3(&http, &s3_draft(&minio, BUCKET, "cloud", &server, &agent)).await;
        let id = created["id"].as_str().unwrap();
        let before = detail(&http, id).await;

        let proxy = FaultProxy::start(minio.endpoint(), delete_not_implemented()).await;
        point_at(&ts, id, &proxy.endpoint()).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let result = recheck(&http, id).await;
        assert_eq!(result["guarantee_level"], "unchecked", "{result}");

        let after = detail(&http, id).await;
        assert_eq!(after["guarantee_level"], "overwrite_keeps_old_version");
        assert_eq!(after["guarantee_accepted"], "overwrite_keeps_old_version");
        assert_eq!(
            after["guarantee_checked_at"],
            before["guarantee_checked_at"]
        );
        assert_eq!(after["guarantee_notes"], before["guarantee_notes"]);
        assert!(
            after["checked_at"].as_i64() > before["checked_at"].as_i64(),
            "[{}]",
            backend.name()
        );
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recheck_from_unchecked_does_not_accept_host_can() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let proxy = FaultProxy::start(minio.endpoint(), delete_not_implemented()).await;
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let mut draft = s3_draft(&minio, BUCKET, "cloud", &server, &agent);
        draft["location"]["options"]["endpoint"] = json!(proxy.endpoint());
        let created = create_s3(&http, &draft).await;
        let id = created["id"].as_str().unwrap();
        assert_eq!(created["guarantee_accepted"], "unchecked");

        point_at(&ts, id, minio.endpoint()).await;
        let result = recheck(&http, id).await;
        assert_eq!(result["guarantee_level"], "host_can_overwrite_or_delete");
        let shown = detail(&http, id).await;
        assert_eq!(shown["guarantee_level"], "host_can_overwrite_or_delete");
        assert_eq!(
            shown["guarantee_accepted"],
            "unchecked",
            "[{}]",
            backend.name()
        );
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replace_key_runs_the_full_probe() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_versioned_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let created = create_s3(&http, &s3_draft(&minio, BUCKET, "cloud", &server, &agent)).await;
        let id = created["id"].as_str().unwrap();
        assert_eq!(created["guarantee_accepted"], "overwrite_keeps_old_version");
        minio
            .mc(&[
                "ilm",
                "rule",
                "add",
                &bucket_path(),
                "--noncurrent-expire-days",
                "1",
            ])
            .await
            .expect("mc ilm rule add");

        let fresh = agent_user(&minio, BUCKET).await;
        let replaced = http
            .put(
                &format!("/api/v1/repositories/{id}/agent-key"),
                &creds(&fresh),
            )
            .await;
        assert_eq!(replaced.status(), 200, "[{}]", backend.name());

        let shown = detail(&http, id).await;
        assert_eq!(
            shown["guarantee_level"],
            "host_can_overwrite_or_delete",
            "[{}] {shown}",
            backend.name()
        );
        assert!(notes(&shown).contains(&"lifecycle_deletes"), "{shown}");
        assert_eq!(shown["guarantee_accepted"], "overwrite_keeps_old_version");
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replace_key_inconclusive_probe_stores_unchecked() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_versioned_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let created = create_s3(&http, &s3_draft(&minio, BUCKET, "cloud", &server, &agent)).await;
        let id = created["id"].as_str().unwrap();
        assert_eq!(created["guarantee_accepted"], "overwrite_keeps_old_version");

        let proxy = FaultProxy::start(minio.endpoint(), delete_not_implemented()).await;
        point_at(&ts, id, &proxy.endpoint()).await;
        let fresh = agent_user(&minio, BUCKET).await;
        let replaced = http
            .put(
                &format!("/api/v1/repositories/{id}/agent-key"),
                &creds(&fresh),
            )
            .await;
        assert_eq!(replaced.status(), 200, "[{}]", backend.name());

        let shown = detail(&http, id).await;
        assert_eq!(shown["guarantee_level"], "unchecked", "{shown}");
        assert!(notes(&shown).is_empty(), "{shown}");
        assert_eq!(shown["guarantee_accepted"], "overwrite_keeps_old_version");
        ts.stop().await;
    })
    .await;
}

async fn lowered_repository(minio: &MinioServer, http: &Http) -> String {
    let server = minio.root_credentials();
    let agent = agent_user(minio, BUCKET).await;
    let created = create_s3(http, &s3_draft(minio, BUCKET, "cloud", &server, &agent)).await;
    let id = created["id"].as_str().unwrap().to_string();
    minio
        .mc(&["version", "suspend", &bucket_path()])
        .await
        .expect("mc version suspend");
    recheck(http, &id).await;
    let shown = detail(http, &id).await;
    assert_eq!(shown["guarantee_level"], "host_can_overwrite_or_delete");
    assert_eq!(shown["guarantee_accepted"], "overwrite_keeps_old_version");
    id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guarantee_accept_sets_accepted() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_versioned_bucket(BUCKET).await;
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let id = lowered_repository(&minio, &http).await;

        let accepted = http
            .post(
                &format!("/api/v1/repositories/{id}/guarantee/accept"),
                &json!({ "expected_level": "host_can_overwrite_or_delete" }),
            )
            .await;
        assert_eq!(accepted.status(), 200, "[{}]", backend.name());
        let accepted = body(accepted).await;
        assert_eq!(accepted["guarantee_level"], "host_can_overwrite_or_delete");
        assert_eq!(
            accepted["guarantee_accepted"],
            "host_can_overwrite_or_delete"
        );
        assert!(accepted["guarantee_checked_at"].is_i64());
        let shown = detail(&http, &id).await;
        assert_eq!(shown["guarantee_accepted"], "host_can_overwrite_or_delete");
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guarantee_accept_of_a_level_no_longer_measured_is_conflict() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_versioned_bucket(BUCKET).await;
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let id = lowered_repository(&minio, &http).await;
        let url = format!("/api/v1/repositories/{id}/guarantee/accept");

        let stale = http
            .post(
                &url,
                &json!({ "expected_level": "overwrite_keeps_old_version" }),
            )
            .await;
        assert_eq!(stale.status(), 409, "[{}]", backend.name());
        let shown = detail(&http, &id).await;
        assert_eq!(shown["guarantee_accepted"], "overwrite_keeps_old_version");
        let missing = http.post(&url, &json!({})).await;
        assert_eq!(missing.status(), 422);
        let shown = detail(&http, &id).await;
        assert_eq!(shown["guarantee_accepted"], "overwrite_keeps_old_version");
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guarantee_accept_unchecked_is_conflict() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let proxy = FaultProxy::start(minio.endpoint(), delete_not_implemented()).await;
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let mut draft = s3_draft(&minio, BUCKET, "cloud", &server, &agent);
        draft["location"]["options"]["endpoint"] = json!(proxy.endpoint());
        let created = create_s3(&http, &draft).await;
        let id = created["id"].as_str().unwrap();
        assert_eq!(created["guarantee_level"], "unchecked");

        let refused = http
            .post(
                &format!("/api/v1/repositories/{id}/guarantee/accept"),
                &json!({ "expected_level": "unchecked" }),
            )
            .await;
        assert_eq!(refused.status(), 409, "[{}]", backend.name());
        let unknown = http
            .post(
                "/api/v1/repositories/no-such-repository/guarantee/accept",
                &json!({ "expected_level": "unchecked" }),
            )
            .await;
        assert_eq!(unknown.status(), 404);
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guarantee_accept_needs_session_and_csrf() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_versioned_bucket(BUCKET).await;
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let id = lowered_repository(&minio, &http).await;
        let url = format!("{}/api/v1/repositories/{id}/guarantee/accept", http.base);

        let no_session = http
            .client
            .post(&url)
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(no_session.status(), 401, "[{}]", backend.name());
        let no_csrf = http
            .client
            .post(&url)
            .header("cookie", &http.cookie)
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(no_csrf.status(), 403);
        let shown = detail(&http, &id).await;
        assert_eq!(shown["guarantee_accepted"], "overwrite_keeps_old_version");
        ts.stop().await;
    })
    .await;
}

const HOUR: i64 = 3_600_000;

async fn quiet_server(backend: &Backend) -> TestServer {
    TestServer::start_with(backend, |c| c.schedule_tick_secs = 0).await
}

async fn daily_tick(ts: &TestServer, at_ms: i64) -> Vec<String> {
    lodestore_server::maintenance::guarantee::tick(
        ts.state(),
        at_ms,
        &tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap()
}

async fn guarantee_alerts(
    ts: &TestServer,
    at_ms: i64,
    state: &str,
) -> Vec<(String, String, Value)> {
    lodestore_server::alerts::tick(ts.state(), at_ms)
        .await
        .unwrap();
    sqlx::query(
        "SELECT key, severity, facts_json FROM alerts \
         WHERE type = 'guarantee_downgraded' AND state = $1 ORDER BY key",
    )
    .bind(state)
    .fetch_all(&ts.state().db.pool)
    .await
    .unwrap()
    .iter()
    .map(|row| {
        (
            sqlx::Row::get::<String, _>(row, "key"),
            sqlx::Row::get::<String, _>(row, "severity"),
            serde_json::from_str(&sqlx::Row::get::<String, _>(row, "facts_json")).unwrap(),
        )
    })
    .collect()
}

async fn repo_marks(ts: &TestServer, id: &str) -> sqlx::any::AnyRow {
    sqlx::query(
        "SELECT checked_at, guarantee_checked_at, guarantee_tick_at, guarantee_level, \
         guarantee_accepted, guarantee_unchecked_runs, guarantee_failed_runs, access_generation, \
         agent_key_state FROM repositories WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&ts.state().db.pool)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guarantee_downgrade_raises_an_alert_and_accept_clears_it() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_versioned_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let ts = quiet_server(&backend).await;
        let http = Http::login(&ts).await;
        let created = create_s3(&http, &s3_draft(&minio, BUCKET, "cloud", &server, &agent)).await;
        let id = created["id"].as_str().unwrap();
        assert_eq!(created["guarantee_accepted"], "overwrite_keeps_old_version");
        let now = lodestore_server::db::now_ms();

        minio
            .mc(&["version", "suspend", &bucket_path()])
            .await
            .expect("mc version suspend");
        assert_eq!(daily_tick(&ts, now + 25 * HOUR).await, [id]);
        assert!(daily_tick(&ts, now + 26 * HOUR).await.is_empty());
        let marks = repo_marks(&ts, id).await;
        for column in ["checked_at", "guarantee_checked_at", "guarantee_tick_at"] {
            assert_eq!(
                sqlx::Row::get::<Option<i64>, _>(&marks, column),
                Some(now + 25 * HOUR),
                "{column} [{}]",
                backend.name()
            );
        }

        let firing = guarantee_alerts(&ts, now + 25 * HOUR, "firing").await;
        assert_eq!(firing.len(), 1, "[{}]", backend.name());
        let (key, severity, facts) = &firing[0];
        assert_eq!(key, &format!("guarantee_downgraded:{id}"));
        assert_eq!(severity, "warning");
        assert_eq!(facts["guarantee_level"], "host_can_overwrite_or_delete");
        assert_eq!(facts["guarantee_accepted"], "overwrite_keeps_old_version");
        assert_eq!(facts["guarantee_reason"], "lower");

        let accepted = http
            .post(
                &format!("/api/v1/repositories/{id}/guarantee/accept"),
                &json!({ "expected_level": "host_can_overwrite_or_delete" }),
            )
            .await;
        assert_eq!(accepted.status(), 200);
        assert!(
            guarantee_alerts(&ts, now + 25 * HOUR, "firing")
                .await
                .is_empty()
        );
        assert_eq!(
            guarantee_alerts(&ts, now + 25 * HOUR, "resolved")
                .await
                .len(),
            1
        );
        ts.stop().await;
    })
    .await;
}

async fn location_json(ts: &TestServer, id: &str) -> String {
    sqlx::Row::get(
        &sqlx::query("SELECT location_json FROM repositories WHERE id = $1")
            .bind(id)
            .fetch_one(&ts.state().db.pool)
            .await
            .unwrap(),
        "location_json",
    )
}

async fn set_location_json(ts: &TestServer, id: &str, location: &str) {
    sqlx::query("UPDATE repositories SET location_json = $1 WHERE id = $2")
        .bind(location)
        .bind(id)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
}

fn counter(marks: &sqlx::any::AnyRow, column: &str) -> i64 {
    sqlx::Row::get(marks, column)
}

fn reason(facts: &Value) -> &str {
    facts["guarantee_reason"].as_str().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guarantee_tick_skips_recent_checks() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_versioned_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let ts = quiet_server(&backend).await;
        let http = Http::login(&ts).await;
        let created = create_s3(&http, &s3_draft(&minio, BUCKET, "cloud", &server, &agent)).await;
        let id = created["id"].as_str().unwrap();
        let now = lodestore_server::db::now_ms();

        assert!(daily_tick(&ts, now + HOUR).await.is_empty());
        assert_eq!(daily_tick(&ts, now + 25 * HOUR).await, [id]);
        assert!(
            daily_tick(&ts, now + 26 * HOUR).await.is_empty(),
            "[{}]",
            backend.name()
        );
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guarantee_tick_one_inconclusive_check_is_not_a_downgrade() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_versioned_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let ts = quiet_server(&backend).await;
        let http = Http::login(&ts).await;
        let created = create_s3(&http, &s3_draft(&minio, BUCKET, "cloud", &server, &agent)).await;
        let id = created["id"].as_str().unwrap();
        let now = lodestore_server::db::now_ms();
        let before = repo_marks(&ts, id).await;

        let proxy = FaultProxy::start(minio.endpoint(), delete_not_implemented()).await;
        point_at(&ts, id, &proxy.endpoint()).await;
        assert_eq!(daily_tick(&ts, now + 25 * HOUR).await, [id]);

        let after = repo_marks(&ts, id).await;
        assert_eq!(counter(&after, "guarantee_unchecked_runs"), 1);
        assert_eq!(counter(&after, "guarantee_failed_runs"), 0);
        for column in ["guarantee_level", "guarantee_accepted"] {
            assert_eq!(
                sqlx::Row::get::<String, _>(&after, column),
                sqlx::Row::get::<String, _>(&before, column)
            );
        }
        assert_eq!(
            sqlx::Row::get::<Option<i64>, _>(&after, "guarantee_checked_at"),
            sqlx::Row::get::<Option<i64>, _>(&before, "guarantee_checked_at")
        );
        assert!(
            guarantee_alerts(&ts, now + 25 * HOUR, "firing")
                .await
                .is_empty(),
            "[{}]",
            backend.name()
        );
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guarantee_inconclusive_three_days_raises_an_alert() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_versioned_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let ts = quiet_server(&backend).await;
        let http = Http::login(&ts).await;
        let created = create_s3(&http, &s3_draft(&minio, BUCKET, "cloud", &server, &agent)).await;
        let id = created["id"].as_str().unwrap();
        let now = lodestore_server::db::now_ms();

        let proxy = FaultProxy::start(minio.endpoint(), delete_not_implemented()).await;
        point_at(&ts, id, &proxy.endpoint()).await;
        for days in [1, 2] {
            let at = now + (24 * days + 1) * HOUR;
            assert_eq!(daily_tick(&ts, at).await, [id]);
            assert!(guarantee_alerts(&ts, at, "firing").await.is_empty());
        }
        assert_eq!(
            counter(&repo_marks(&ts, id).await, "guarantee_unchecked_runs"),
            2
        );

        lodestore_server::repos::agent_key::recheck_repository_at(
            ts.state(),
            id,
            now + 62 * HOUR,
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            counter(&repo_marks(&ts, id).await, "guarantee_unchecked_runs"),
            2,
            "a manual check does not count"
        );

        let at = now + 75 * HOUR;
        assert_eq!(daily_tick(&ts, at).await, [id]);
        let firing = guarantee_alerts(&ts, at, "firing").await;
        assert_eq!(firing.len(), 1, "[{}]", backend.name());
        assert_eq!(reason(&firing[0].2), "inconclusive");
        assert_eq!(firing[0].2["unchecked_runs"], 3);

        point_at(&ts, id, minio.endpoint()).await;
        let at = now + 100 * HOUR;
        assert_eq!(daily_tick(&ts, at).await, [id]);
        assert_eq!(
            counter(&repo_marks(&ts, id).await, "guarantee_unchecked_runs"),
            0
        );
        assert!(guarantee_alerts(&ts, at, "firing").await.is_empty());
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guarantee_recheck_failing_raises_an_alert_after_three_days() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_versioned_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let ts = quiet_server(&backend).await;
        let http = Http::login(&ts).await;
        let created = create_s3(&http, &s3_draft(&minio, BUCKET, "cloud", &server, &agent)).await;
        let id = created["id"].as_str().unwrap();
        let now = lodestore_server::db::now_ms();
        let location = location_json(&ts, id).await;
        let before = repo_marks(&ts, id).await;

        set_location_json(&ts, id, "{}").await;
        for (days, failed) in [(1, 1), (2, 2)] {
            let at = now + (24 * days + 1) * HOUR;
            assert_eq!(daily_tick(&ts, at).await, [id]);
            let marks = repo_marks(&ts, id).await;
            assert_eq!(counter(&marks, "guarantee_failed_runs"), failed);
            assert_eq!(counter(&marks, "guarantee_unchecked_runs"), 0);
            assert_eq!(
                sqlx::Row::get::<String, _>(&marks, "guarantee_level"),
                sqlx::Row::get::<String, _>(&before, "guarantee_level")
            );
            assert!(guarantee_alerts(&ts, at, "firing").await.is_empty());
        }
        let at = now + 75 * HOUR;
        assert_eq!(daily_tick(&ts, at).await, [id]);
        let firing = guarantee_alerts(&ts, at, "firing").await;
        assert_eq!(firing.len(), 1, "[{}]", backend.name());
        assert_eq!(reason(&firing[0].2), "recheck_failing");
        assert_eq!(firing[0].2["failed_runs"], 3);

        set_location_json(&ts, id, &location).await;
        let at = now + 100 * HOUR;
        assert_eq!(daily_tick(&ts, at).await, [id]);
        assert_eq!(
            counter(&repo_marks(&ts, id).await, "guarantee_failed_runs"),
            0
        );
        assert!(guarantee_alerts(&ts, at, "firing").await.is_empty());
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guarantee_upgraded_repository_is_not_alerted_on_the_first_tick() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_versioned_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let ts = quiet_server(&backend).await;
        let http = Http::login(&ts).await;
        let created = create_s3(&http, &s3_draft(&minio, BUCKET, "cloud", &server, &agent)).await;
        let id = created["id"].as_str().unwrap();
        let now = lodestore_server::db::now_ms();
        sqlx::query(
            "UPDATE repositories SET guarantee_checked_at = NULL, created_at = $1 WHERE id = $2",
        )
        .bind(now - 365 * 24 * HOUR)
        .bind(id)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
        let location = location_json(&ts, id).await;

        let proxy = FaultProxy::start(minio.endpoint(), delete_not_implemented()).await;
        point_at(&ts, id, &proxy.endpoint()).await;
        let at = now + 25 * HOUR;
        assert_eq!(daily_tick(&ts, at).await, [id]);
        assert!(
            guarantee_alerts(&ts, at, "firing").await.is_empty(),
            "[{}]",
            backend.name()
        );

        set_location_json(&ts, id, "{}").await;
        let at = now + 50 * HOUR;
        assert_eq!(daily_tick(&ts, at).await, [id]);
        assert!(guarantee_alerts(&ts, at, "firing").await.is_empty());
        assert_eq!(
            counter(&repo_marks(&ts, id).await, "guarantee_failed_runs"),
            1
        );
        set_location_json(&ts, id, &location).await;
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guarantee_unaccepted_weak_level_raises_an_alert() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let proxy = FaultProxy::start(minio.endpoint(), delete_not_implemented()).await;
        let ts = quiet_server(&backend).await;
        let http = Http::login(&ts).await;
        let mut draft = s3_draft(&minio, BUCKET, "cloud", &server, &agent);
        draft["location"]["options"]["endpoint"] = json!(proxy.endpoint());
        let created = create_s3(&http, &draft).await;
        let id = created["id"].as_str().unwrap();
        assert_eq!(created["guarantee_accepted"], "unchecked");
        let now = lodestore_server::db::now_ms();

        point_at(&ts, id, minio.endpoint()).await;
        let at = now + 25 * HOUR;
        assert_eq!(daily_tick(&ts, at).await, [id]);
        let firing = guarantee_alerts(&ts, at, "firing").await;
        assert_eq!(firing.len(), 1, "[{}]", backend.name());
        assert_eq!(reason(&firing[0].2), "weak_unaccepted");

        let accepted = http
            .post(
                &format!("/api/v1/repositories/{id}/guarantee/accept"),
                &json!({ "expected_level": "host_can_overwrite_or_delete" }),
            )
            .await;
        assert_eq!(accepted.status(), 200);
        assert!(guarantee_alerts(&ts, at, "firing").await.is_empty());
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guarantee_downgrade_resolves_when_the_level_returns() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_versioned_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let ts = quiet_server(&backend).await;
        let http = Http::login(&ts).await;
        let created = create_s3(&http, &s3_draft(&minio, BUCKET, "cloud", &server, &agent)).await;
        let id = created["id"].as_str().unwrap();
        let now = lodestore_server::db::now_ms();

        minio
            .mc(&["version", "suspend", &bucket_path()])
            .await
            .expect("mc version suspend");
        let at = now + 25 * HOUR;
        assert_eq!(daily_tick(&ts, at).await, [id]);
        assert_eq!(
            reason(&guarantee_alerts(&ts, at, "firing").await[0].2),
            "lower"
        );

        minio
            .mc(&["version", "enable", &bucket_path()])
            .await
            .expect("mc version enable");
        let at = now + 50 * HOUR;
        assert_eq!(daily_tick(&ts, at).await, [id]);
        assert!(
            guarantee_alerts(&ts, at, "firing").await.is_empty(),
            "[{}]",
            backend.name()
        );
        let marks = repo_marks(&ts, id).await;
        assert_eq!(
            sqlx::Row::get::<String, _>(&marks, "guarantee_accepted"),
            "overwrite_keeps_old_version"
        );
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guarantee_tick_discards_a_result_measured_with_replaced_credentials() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_versioned_bucket(BUCKET).await;
        let root = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let ts = quiet_server(&backend).await;
        let http = Http::login(&ts).await;
        let created = create_s3(&http, &s3_draft(&minio, BUCKET, "cloud", &root, &agent)).await;
        let id = created["id"].as_str().unwrap();
        let key_path = format!("/api/v1/repositories/{id}/agent-key");
        let now = lodestore_server::db::now_ms();
        let proxy = FaultProxy::start(minio.endpoint(), FaultPlan::none()).await;
        point_at(&ts, id, &proxy.endpoint()).await;

        let at = now + 25 * HOUR;
        let (arrived, release) = proxy.hold("GET", &["lifecycle"]);
        let (ticked, ()) = tokio::join!(daily_tick(&ts, at), async {
            arrived.await.unwrap();
            let swapped = http.put(&key_path, &creds(&root)).await;
            assert_eq!(swapped.status(), 200);
            release.send(()).unwrap();
        });
        assert_eq!(ticked, [id]);
        let marks = repo_marks(&ts, id).await;
        assert_eq!(counter(&marks, "access_generation"), 1);
        assert_eq!(
            sqlx::Row::get::<String, _>(&marks, "guarantee_level"),
            "host_can_overwrite_or_delete",
            "[{}]",
            backend.name()
        );
        assert_eq!(
            sqlx::Row::get::<String, _>(&marks, "agent_key_state"),
            "can_delete"
        );
        let written = sqlx::Row::get::<Option<i64>, _>(&marks, "guarantee_checked_at").unwrap();
        assert!(
            written < now + HOUR,
            "the tick's clock was stored: {written}"
        );
        assert_eq!(
            sqlx::Row::get::<Option<i64>, _>(&marks, "guarantee_tick_at"),
            None
        );
        assert_eq!(counter(&marks, "guarantee_unchecked_runs"), 0);
        assert_eq!(counter(&marks, "guarantee_failed_runs"), 0);

        let (arrived, release) = proxy.hold("GET", &["lifecycle"]);
        let (check_path, empty) = (format!("/api/v1/repositories/{id}/check"), json!({}));
        let (rechecked, ()) = tokio::join!(http.post(&check_path, &empty), async {
            arrived.await.unwrap();
            let swapped = http.put(&key_path, &creds(&agent)).await;
            assert_eq!(swapped.status(), 200);
            release.send(()).unwrap();
        });
        assert_eq!(rechecked.status(), 409, "[{}]", backend.name());
        assert_eq!(counter(&repo_marks(&ts, id).await, "access_generation"), 2);
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guarantee_newer_measurement_is_not_overwritten_by_a_slower_check() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_versioned_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let ts = quiet_server(&backend).await;
        let http = Http::login(&ts).await;
        let created = create_s3(&http, &s3_draft(&minio, BUCKET, "cloud", &server, &agent)).await;
        let id = created["id"].as_str().unwrap();
        let now = lodestore_server::db::now_ms();
        let proxy = FaultProxy::start(minio.endpoint(), FaultPlan::none()).await;
        point_at(&ts, id, &proxy.endpoint()).await;

        let at = now + 25 * HOUR;
        let (arrived, release) = proxy.hold("GET", &["lifecycle"]);
        let (ticked, ()) = tokio::join!(daily_tick(&ts, at), async {
            arrived.await.unwrap();
            minio
                .mc(&["version", "suspend", &bucket_path()])
                .await
                .expect("mc version suspend");
            lodestore_server::repos::agent_key::recheck_repository_at(
                ts.state(),
                id,
                at + 1,
                false,
            )
            .await
            .unwrap();
            release.send(()).unwrap();
        });
        assert_eq!(ticked, [id]);
        let marks = repo_marks(&ts, id).await;
        assert_eq!(
            sqlx::Row::get::<String, _>(&marks, "guarantee_level"),
            "host_can_overwrite_or_delete",
            "[{}]",
            backend.name()
        );
        assert_eq!(
            sqlx::Row::get::<Option<i64>, _>(&marks, "checked_at"),
            Some(at + 1)
        );
        let firing = guarantee_alerts(&ts, at + 2 * HOUR, "firing").await;
        assert_eq!(firing.len(), 1);
        assert_eq!(reason(&firing[0].2), "lower");
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guarantee_tick_skips_a_row_it_cannot_read() {
    let minio = MinioServer::start().await;
    minio.create_versioned_bucket(BUCKET).await;
    minio.create_versioned_bucket("second").await;
    let server = minio.root_credentials();
    let ts = quiet_server(&Backend::Sqlite).await;
    let http = Http::login(&ts).await;
    let agent = agent_user(&minio, BUCKET).await;
    let broken = create_s3(&http, &s3_draft(&minio, BUCKET, "broken", &server, &agent)).await;
    let broken = broken["id"].as_str().unwrap();
    let agent = agent_user(&minio, "second").await;
    let fine = create_s3(&http, &s3_draft(&minio, "second", "fine", &server, &agent)).await;
    let fine = fine["id"].as_str().unwrap();
    sqlx::query("UPDATE repositories SET guarantee_tick_at = 'not a time' WHERE id = $1")
        .bind(broken)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();

    let at = lodestore_server::db::now_ms() + 25 * HOUR;
    assert_eq!(daily_tick(&ts, at).await, [fine]);
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guarantee_manual_inconclusive_check_keeps_the_failed_runs() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_versioned_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let ts = quiet_server(&backend).await;
        let http = Http::login(&ts).await;
        let created = create_s3(&http, &s3_draft(&minio, BUCKET, "cloud", &server, &agent)).await;
        let id = created["id"].as_str().unwrap();
        sqlx::query("UPDATE repositories SET guarantee_failed_runs = 2 WHERE id = $1")
            .bind(id)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        let proxy = FaultProxy::start(minio.endpoint(), delete_not_implemented()).await;
        point_at(&ts, id, &proxy.endpoint()).await;

        let manual = recheck(&http, id).await;
        assert_eq!(manual["guarantee_level"], "unchecked");
        let marks = repo_marks(&ts, id).await;
        assert_eq!(
            counter(&marks, "guarantee_failed_runs"),
            2,
            "[{}]",
            backend.name()
        );
        assert_eq!(counter(&marks, "guarantee_unchecked_runs"), 0);

        let at = lodestore_server::db::now_ms() + 25 * HOUR;
        assert_eq!(daily_tick(&ts, at).await, [id]);
        let marks = repo_marks(&ts, id).await;
        assert_eq!(counter(&marks, "guarantee_failed_runs"), 0);
        assert_eq!(counter(&marks, "guarantee_unchecked_runs"), 1);
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guarantee_check_marks_from_the_future_do_not_silence_the_recheck() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_versioned_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let ts = quiet_server(&backend).await;
        let http = Http::login(&ts).await;
        let created = create_s3(&http, &s3_draft(&minio, BUCKET, "cloud", &server, &agent)).await;
        let id = created["id"].as_str().unwrap();
        let now = lodestore_server::db::now_ms();
        let future = now + 30 * 24 * HOUR;
        sqlx::query(
            "UPDATE repositories SET checked_at = $1, guarantee_checked_at = $1, \
             guarantee_tick_at = $1 WHERE id = $2",
        )
        .bind(future)
        .bind(id)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();

        minio
            .mc(&["version", "suspend", &bucket_path()])
            .await
            .expect("mc version suspend");
        assert_eq!(daily_tick(&ts, now).await, [id], "[{}]", backend.name());
        let marks = repo_marks(&ts, id).await;
        assert_eq!(
            sqlx::Row::get::<String, _>(&marks, "guarantee_level"),
            "host_can_overwrite_or_delete",
            "[{}]",
            backend.name()
        );
        for column in ["checked_at", "guarantee_checked_at", "guarantee_tick_at"] {
            assert_eq!(
                sqlx::Row::get::<Option<i64>, _>(&marks, column),
                Some(now),
                "{column}"
            );
        }
        let firing = guarantee_alerts(&ts, now, "firing").await;
        assert_eq!(firing.len(), 1);
        assert_eq!(reason(&firing[0].2), "lower");
        ts.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guarantee_check_of_a_deleted_repository_is_not_found() {
    each_backend(|backend| async move {
        let minio = MinioServer::start().await;
        minio.create_versioned_bucket(BUCKET).await;
        let server = minio.root_credentials();
        let agent = agent_user(&minio, BUCKET).await;
        let ts = quiet_server(&backend).await;
        let http = Http::login(&ts).await;
        let created = create_s3(&http, &s3_draft(&minio, BUCKET, "cloud", &server, &agent)).await;
        let id = created["id"].as_str().unwrap();
        let now = lodestore_server::db::now_ms();
        let proxy = FaultProxy::start(minio.endpoint(), FaultPlan::none()).await;
        point_at(&ts, id, &proxy.endpoint()).await;

        let (arrived, release) = proxy.hold("GET", &["lifecycle"]);
        let (rechecked, ()) = tokio::join!(
            lodestore_server::repos::agent_key::recheck_repository_at(
                ts.state(),
                id,
                now + 25 * HOUR,
                true,
            ),
            async {
                arrived.await.unwrap();
                delete_repository(&ts, id).await;
                release.send(()).unwrap();
            }
        );
        assert!(
            matches!(
                rechecked,
                Err(lodestore_server::error::ApiError::NotFound(_))
            ),
            "[{}]",
            backend.name()
        );
        assert!(daily_tick(&ts, now + 50 * HOUR).await.is_empty());
        let missing = http
            .post(&format!("/api/v1/repositories/{id}/check"), &json!({}))
            .await;
        assert_eq!(missing.status(), 404);
        ts.stop().await;
    })
    .await;
}

async fn delete_repository(ts: &TestServer, id: &str) {
    let pool = &ts.state().db.pool;
    for statement in [
        "DELETE FROM check_settings WHERE repository_id = $1",
        "DELETE FROM catalog_state WHERE repository_id = $1",
        "DELETE FROM gc_settings WHERE repository_id = $1",
        "DELETE FROM repo_usage WHERE repository_id = $1",
    ] {
        sqlx::query(statement).bind(id).execute(pool).await.unwrap();
    }
    sqlx::query("DELETE FROM repositories WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}
