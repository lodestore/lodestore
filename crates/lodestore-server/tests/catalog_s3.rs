mod common;

use std::collections::BTreeMap;

use common::{TestServer, backends};
use lodestore_format::crypto::Entropy;
use lodestore_format::index::hex32;
use lodestore_format::keys::KdfCost;
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::snapshot::snapshot_object_key;
use lodestore_format::store::{BlobStore, RepoLocation, StaticCredentials, Store};
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_server::catalog::sync_repository;
use lodestore_server::db::now_ms;
use lodestore_server::repos::db::{AccessKey, NewRepository, RepoLocation as DbLocation};
use lodestore_testkit::minio::MinioServer;
use sqlx::Row;

const PASSWORD: &[u8] = b"test-password";
const BUCKET: &str = "repo";
const SKEW_MS: i64 = 5_000;

struct Cloud {
    minio: MinioServer,
    store: Store,
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
        let options = BTreeMap::from([
            ("bucket".to_string(), BUCKET.to_string()),
            ("endpoint".to_string(), minio.endpoint().to_string()),
            ("path_style".to_string(), "true".to_string()),
        ]);
        let store = RepoLocation {
            backend: "s3".to_string(),
            options: options.clone(),
        }
        .open(Some(StaticCredentials {
            access_key_id: root.0,
            secret_access_key: root.1,
        }))
        .unwrap();
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
            .commit(&store, PASSWORD, &mut entropy)
            .await
            .unwrap();
        Cloud {
            minio,
            store,
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
                agent_creds: None,
            },
        )
        .await
        .unwrap()
    }

    async fn write_snapshot(&self) -> String {
        let repo = Repository::open(&self.store, PASSWORD).await.unwrap();
        let keys = repo.read_keys().write_keys(repo.config_hash());
        let mut writer = Writer::new(&self.store, keys, repo.config()).await.unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(data.path().join("file.txt"), b"content").unwrap();
        let outcome = writer
            .backup(
                &[data.path().to_path_buf()],
                BackupOptions {
                    hostname: Some(b"host-s3".to_vec()),
                    ..BackupOptions::default()
                },
                &|_| {},
            )
            .await
            .expect("backup");
        hex32(&outcome.snapshot_id)
    }
}

async fn received(ts: &TestServer, id: &str) -> (i64, i64) {
    let row = sqlx::query("SELECT received_at, received_exact FROM snapshots WHERE id = $1")
        .bind(id)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap();
    (
        row.try_get("received_at").unwrap(),
        row.try_get("received_exact").unwrap(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalog_s3_received_last_modified() {
    let cloud = Cloud::start().await;
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let repo = cloud.register(&ts).await;
        sync_repository(ts.state(), &repo).await.unwrap();
        let before = now_ms();
        let id = cloud.write_snapshot().await;
        let after = now_ms();

        sync_repository(ts.state(), &repo).await.unwrap();
        let (at, exact) = received(&ts, &id).await;
        assert_eq!(exact, 1, "[{}]", backend.name());
        assert!(
            (before - SKEW_MS..=after + SKEW_MS).contains(&at),
            "received_at {at} outside {before}..{after}"
        );

        sqlx::query("DELETE FROM snapshots")
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        sync_repository(ts.state(), &repo).await.unwrap();
        let (rebuilt, exact) = received(&ts, &id).await;
        assert_eq!(exact, 1);
        assert!((rebuilt - at).abs() < 1_000, "{rebuilt} vs {at}");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalog_s3_modified_ms_matches_listing() {
    let cloud = Cloud::start().await;
    let id = cloud.write_snapshot().await;
    let key = snapshot_object_key(&lodestore_format::index::parse_hex32(&id).unwrap());

    let listed = cloud.store.list("snapshots/").await.unwrap();
    let entry = listed.iter().find(|e| e.key == key).expect("listed");
    let stat = cloud.store.modified_ms(&key).await.unwrap();

    let listed_ms = entry.modified_ms.expect("the listing carries a time");
    let stat_ms = stat.expect("stat carries a time");
    assert!(
        (listed_ms - stat_ms).abs() < 1_000,
        "{listed_ms} vs {stat_ms}"
    );
}
