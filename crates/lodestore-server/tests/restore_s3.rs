mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use common::{AgentEnv, TestServer, backends};
use lodestore_format::crypto::Entropy;
use lodestore_format::keys::KdfCost;
use lodestore_format::repo::{InitOptions, PendingInit};
use lodestore_format::store::{BlobStore, RepoLocation, StaticCredentials, Store};
use lodestore_proto::agent::v1::ConflictPolicy;
use lodestore_server::jobs::restore::{RestoreRequest, enqueue_restore};
use lodestore_server::repos::db::{AccessKey, NewRepository, RepoLocation as DbLocation};
use lodestore_testkit::minio::MinioServer;
use serde_json::Value;
use sqlx::Row;

const PASSWORD: &[u8] = b"test-password";
const BUCKET: &str = "repo";
const BIG: usize = 32 * (1 << 20);

fn noise(len: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

struct Cloud {
    minio: MinioServer,
    agent_key: (String, String),
    server_store: Store,
    options: BTreeMap<String, String>,
    format_repo_id: [u8; 16],
    config_hash: [u8; 32],
    master: lodestore_format::keys::MasterKey,
}

impl Cloud {
    async fn start() -> Cloud {
        let minio = MinioServer::start().await;
        minio.create_bucket(BUCKET).await;
        let root = minio.root_credentials();
        let agent_key = minio
            .create_user_with_policy(
                BUCKET,
                &lodestore_format::store::policy::agent_minio_policy_json(BUCKET, ""),
            )
            .await;
        let options = BTreeMap::from([
            ("bucket".to_string(), BUCKET.to_string()),
            ("endpoint".to_string(), minio.endpoint().to_string()),
            ("path_style".to_string(), "true".to_string()),
        ]);
        let server_store = open(&options, &root);
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
            server_store,
            options,
            format_repo_id,
            config_hash,
            master,
        }
    }

    async fn register(&self, ts: &TestServer) -> String {
        let root = self.minio.root_credentials();
        lodestore_server::repos::db::create_repository(
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
                    access_key_id: root.0,
                    secret_access_key: root.1,
                }),
                agent_creds: Some(AccessKey {
                    access_key_id: self.agent_key.0.clone(),
                    secret_access_key: self.agent_key.1.clone(),
                }),
            },
        )
        .await
        .unwrap()
    }
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

fn write_tree(root: &Path) {
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("big.bin"), noise(BIG)).unwrap();
    fs::write(root.join("sub/small.txt"), b"over the wire").unwrap();
}

async fn progress(ts: &TestServer, job: &str) -> Value {
    let row = sqlx::query("SELECT progress_json FROM jobs WHERE id = $1")
        .bind(job)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap();
    let text: String = row.try_get("progress_json").unwrap();
    serde_json::from_str(&text).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_s3_presigned_range_e2e() {
    let cloud = Cloud::start().await;
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let repo = cloud.register(&ts).await;
        let env = AgentEnv::start_on(&ts, &repo, "cloud", "host-s3").await;
        let src = env.work.path().join("src");
        write_tree(&src);
        let src = src.canonicalize().unwrap();
        let snapshot = env.backup(&[&src]).await;
        let out = env.work.path().join("out");

        let id = enqueue_restore(
            ts.state(),
            RestoreRequest {
                snapshot_id: snapshot,
                items: vec![src.to_string_lossy().into_owned().into_bytes()],
                target_host_id: env.host.clone(),
                destination: out.to_string_lossy().into_owned(),
                in_place: false,
                conflict: ConflictPolicy::OverwriteChanged,
                restore_owners: false,
                accept_unverified: true,
            },
            "admin",
        )
        .await
        .unwrap();
        let job = env.wait_job(&id).await;
        assert_eq!(job["state"], "succeeded", "[{}] {job}", backend.name());

        let restored = out.join(src.strip_prefix("/").unwrap());
        assert!(fs::read(restored.join("big.bin")).unwrap() == noise(BIG));
        assert_eq!(
            fs::read(restored.join("sub/small.txt")).unwrap(),
            b"over the wire"
        );
        let progress = progress(&ts, &id).await;
        assert!(
            progress["bytes_downloaded"].as_u64().unwrap() > 0,
            "{progress}"
        );
        env.stop().await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_s3_agent_key_cannot_read_packs() {
    let cloud = Cloud::start().await;
    let backend = backends().remove(0);
    let ts = TestServer::start(&backend).await;
    let repo = cloud.register(&ts).await;
    let env = AgentEnv::start_on(&ts, &repo, "cloud", "host-s3").await;
    let src = env.work.path().join("src");
    write_tree(&src);
    env.backup(&[&src.canonicalize().unwrap()]).await;

    let packs = cloud.server_store.list("packs/").await.unwrap();
    assert!(!packs.is_empty(), "the backup wrote no packs");
    let agent = open(&cloud.options, &cloud.agent_key);
    for pack in &packs {
        assert!(
            agent.get_range(&pack.key, 0..16).await.is_err(),
            "the agent key read {}",
            pack.key
        );
        assert!(cloud.server_store.get_range(&pack.key, 0..16).await.is_ok());
    }
    env.stop().await;
    ts.stop().await;
}
