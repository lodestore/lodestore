mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::{AgentEnv, TestServer, WAIT, backends};
use lodestore_format::crypto::Entropy;
use lodestore_format::keys::KdfCost;
use lodestore_format::repo::{InitOptions, PendingInit};
use lodestore_format::store::{BlobStore, RepoLocation, StaticCredentials, Store};
use lodestore_server::catalog::sync_repository;
use lodestore_server::repos::db::{AccessKey, NewRepository, RepoLocation as DbLocation};
use lodestore_testkit::minio::MinioServer;
use serde_json::{Value, json};
use sqlx::Row;

const PASSWORD: &[u8] = b"test-password";
const BUCKET: &str = "repo";

struct Cloud {
    minio: MinioServer,
    agent_key: (String, String),
    server_key: (String, String),
    server_store: Store,
    options: BTreeMap<String, String>,
    format_repo_id: [u8; 16],
    config_hash: [u8; 32],
    master: lodestore_format::keys::MasterKey,
}

impl Cloud {
    async fn start(locked: bool, server_can_delete: bool) -> Cloud {
        let minio = MinioServer::start().await;
        if locked {
            minio.create_locked_bucket(BUCKET).await;
        } else {
            minio.create_bucket(BUCKET).await;
        }
        let agent_key = minio
            .create_user_with_policy(
                BUCKET,
                &lodestore_format::store::policy::agent_minio_policy_json(BUCKET, ""),
            )
            .await;
        let server_key = if server_can_delete {
            minio.root_credentials()
        } else {
            minio
                .create_user_with_policy(BUCKET, &no_delete_policy().to_string())
                .await
        };
        let options = BTreeMap::from([
            ("bucket".to_string(), BUCKET.to_string()),
            ("endpoint".to_string(), minio.endpoint().to_string()),
            ("path_style".to_string(), "true".to_string()),
        ]);
        let server_store = open(&options, &server_key);
        let mut entropy = Entropy::os();
        let pending = PendingInit::generate(
            &mut entropy,
            InitOptions {
                kdf: KdfCost {
                    memory_kib: 64,
                    iterations: 1,
                    parallelism: 1,
                },
                ..InitOptions::default()
            },
        )
        .unwrap();
        let keys = pending.read_keys();
        let master = keys.master().clone();
        let format_repo_id = *keys.repo_id();
        let config_hash = lodestore_format::config::config_hash(pending.config_bytes());
        pending
            .commit(&server_store, PASSWORD, &mut entropy)
            .await
            .unwrap();
        Cloud {
            minio,
            agent_key,
            server_key,
            server_store,
            options,
            format_repo_id,
            config_hash,
            master,
        }
    }

    async fn register(&self, ts: &TestServer) -> String {
        let repo = lodestore_server::repos::db::create_repository(
            ts.state(),
            NewRepository {
                name: "cloud".to_string(),
                location: DbLocation {
                    backend: "s3".to_string(),
                    options: self.options.clone(),
                },
                experimental: false,
                format_repo_id: self.format_repo_id,
                config_hash: self.config_hash,
                master: self.master.clone(),
                server_slot_secret: None,
                server_creds: Some(AccessKey {
                    access_key_id: self.server_key.0.clone(),
                    secret_access_key: self.server_key.1.clone(),
                }),
                agent_creds: Some(AccessKey {
                    access_key_id: self.agent_key.0.clone(),
                    secret_access_key: self.agent_key.1.clone(),
                }),
            },
        )
        .await
        .unwrap();
        sync_repository(ts.state(), &repo).await.unwrap();
        repo
    }

    async fn manifests(&self) -> Vec<String> {
        self.server_store
            .list("snapshots/")
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.key)
            .collect()
    }
}

fn no_delete_policy() -> Value {
    let arn = |path: &str| format!("arn:aws:s3:::{BUCKET}{path}");
    json!({
        "Version": "2012-10-17",
        "Statement": [
            {"Effect": "Allow", "Action": ["s3:GetObject", "s3:PutObject"],
             "Resource": [arn("/*")]},
            {"Effect": "Allow", "Action": ["s3:ListBucket", "s3:GetBucketLocation"],
             "Resource": [arn("")]},
        ],
    })
}

fn open(options: &BTreeMap<String, String>, key: &(String, String)) -> Store {
    RepoLocation {
        backend: "s3".to_string(),
        options: options.clone(),
    }
    .open(Some(StaticCredentials {
        access_key_id: key.0.clone(),
        secret_access_key: key.1.clone(),
    }))
    .unwrap()
}

impl AgentEnv {
    async fn s3_policy(&self, name: &str, path: &std::path::Path) {
        let yaml = format!(
            "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: {name}\n\
             spec:\n  repository: cloud\n  selector:\n    matchLabels:\n      agent: {}\n  \
             paths:\n    - {}\n  retention:\n    keepLast: 1\n",
            self.hostname,
            path.display()
        );
        let response = self
            .api
            .post("/api/v1/policies", &json!({ "yaml": yaml }))
            .await;
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        assert_eq!(status, 201, "{body}");
    }

    async fn s3_run(&self, ts: &TestServer, policy: &str) -> Value {
        let response = self
            .api
            .post(&format!("/api/v1/policies/{policy}/run"), &json!({}))
            .await;
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        let id = body["jobs"][0]["id"].as_str().unwrap().to_string();
        let job = self.wait_job(&id).await;
        let deadline = std::time::Instant::now() + WAIT;
        loop {
            let row = sqlx::query("SELECT retention_json FROM jobs WHERE id = $1")
                .bind(&id)
                .fetch_one(&ts.state().db.pool)
                .await
                .unwrap();
            if let Some(text) = row.try_get::<Option<String>, _>("retention_json").unwrap() {
                let retention: Value = serde_json::from_str(&text).unwrap();
                if retention["state"] != "running" {
                    return json!({ "job": job, "retention": retention });
                }
            }
            assert!(std::time::Instant::now() < deadline, "retention stuck");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn s3_rows(&self, ts: &TestServer, table: &'static str) -> Vec<String> {
        sqlx::query(table)
            .fetch_all(&ts.state().db.pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.try_get("id").unwrap())
            .collect()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_s3_denied_delete_tombstones() {
    let cloud = Cloud::start(false, false).await;
    let backend = backends().remove(0);
    let ts = TestServer::start(&backend).await;
    let repo = cloud.register(&ts).await;
    let env = AgentEnv::start_on(&ts, &repo, "cloud", "host-s3").await;
    let src = env.work.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("file"), b"retention over s3").unwrap();
    env.s3_policy("p", &src.canonicalize().unwrap()).await;

    let first = env.s3_run(&ts, "p").await;
    env.s3_run(&ts, "p").await;
    let third = env.s3_run(&ts, "p").await;

    let oldest = first["job"]["snapshot_id"].as_str().unwrap().to_string();
    assert_eq!(third["job"]["state"], "succeeded", "{third}");
    assert_eq!(third["retention"]["forgot"], 0, "{third}");
    assert_eq!(third["retention"]["blocked"][0]["id"], oldest, "{third}");
    let job_id = third["job"]["id"].as_str().unwrap();
    let blocked = env
        .events(job_id, "")
        .await
        .into_iter()
        .find(|e| e["kind"] == "forget_blocked")
        .expect("a forget_blocked entry");
    assert_eq!(blocked["level"], "warning");
    assert!(
        blocked["message"]
            .as_str()
            .unwrap()
            .starts_with(&format!("Could not delete snapshot {oldest}: ")),
        "{blocked}"
    );
    assert_eq!(
        env.s3_rows(&ts, "SELECT id FROM snapshot_tombstones").await,
        vec![oldest.clone()]
    );
    assert_eq!(env.s3_rows(&ts, "SELECT id FROM snapshots").await.len(), 2);
    assert!(
        cloud
            .manifests()
            .await
            .contains(&format!("snapshots/{oldest}"))
    );

    sync_repository(ts.state(), &repo).await.unwrap();

    assert!(
        !env.s3_rows(&ts, "SELECT id FROM snapshots")
            .await
            .contains(&oldest)
    );
    assert_eq!(
        env.s3_rows(&ts, "SELECT id FROM snapshot_tombstones")
            .await
            .len(),
        1
    );
    env.stop().await;
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_s3_object_lock_delete_marker() {
    let cloud = Cloud::start(true, true).await;
    let backend = backends().remove(0);
    let ts = TestServer::start(&backend).await;
    let repo = cloud.register(&ts).await;
    let env = AgentEnv::start_on(&ts, &repo, "cloud", "host-s3").await;
    let src = env.work.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("file"), b"retention over a locked bucket").unwrap();
    env.s3_policy("p", &src.canonicalize().unwrap()).await;

    let first = env.s3_run(&ts, "p").await;
    env.s3_run(&ts, "p").await;
    let third = env.s3_run(&ts, "p").await;

    let oldest = first["job"]["snapshot_id"].as_str().unwrap().to_string();
    assert_eq!(third["retention"]["forgot"], 1, "{third}");
    assert_eq!(third["retention"]["blocked"], json!([]), "{third}");
    assert!(
        env.s3_rows(&ts, "SELECT id FROM snapshot_tombstones")
            .await
            .is_empty()
    );
    assert_eq!(env.s3_rows(&ts, "SELECT id FROM snapshots").await.len(), 2);
    let manifests = cloud.manifests().await;
    assert_eq!(manifests.len(), 2, "{manifests:?}");
    assert!(!manifests.contains(&format!("snapshots/{oldest}")));
    let versions = cloud
        .minio
        .object_versions(BUCKET, &format!("snapshots/{oldest}"))
        .await;
    assert!(!versions.is_empty(), "the locked version is gone");
    env.stop().await;
    ts.stop().await;
}
