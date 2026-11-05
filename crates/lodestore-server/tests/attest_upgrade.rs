mod common;

use std::collections::BTreeSet;

use common::{Backend, TestServer, backends};
use lodestore_format::attest::{
    ATTEST_PREFIX, MARKER_LEGACY_SHARDS, open_attestation, put_attestation, read_attestations,
};
use lodestore_format::crypto::{BlobId, Entropy};
use lodestore_format::index::{hex32, index_object_key, parse_hex32};
use lodestore_format::keys::ReadKeys;
use lodestore_format::repo::Repository;
use lodestore_format::store::{BlobStore, LocalFsStore, put_named};
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_server::catalog::open_repository;
use lodestore_server::maintenance::attest::{
    ensure_upgraded, load_verified, record_verified, tick_once,
};
use serde_json::json;
use sqlx::Row;

const PASSWORD: &[u8] = b"test-password";
const INDEX_PREFIX: &str = "index/";

async fn start(backend: &Backend) -> TestServer {
    TestServer::start_with(backend, |c| c.schedule_tick_secs = 0).await
}

async fn backup(ts: &TestServer, name: &str) {
    let store = LocalFsStore::new(ts.local_repo_root(name));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    let mut writer = Writer::new(&store, keys, repo.config()).await.unwrap();
    let data = ts.dir.path().join(format!("data-{name}"));
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("file.txt"), b"content").unwrap();
    writer
        .backup(&[data], BackupOptions::default(), &|_| {})
        .await
        .expect("backup");
}

async fn plant_shard(ts: &TestServer, name: &str) -> BlobId {
    let store = LocalFsStore::new(ts.local_repo_root(name));
    let id: BlobId = Entropy::os().bytes::<32>();
    put_named(&store, &index_object_key(&id), b"planted".to_vec())
        .await
        .unwrap();
    id
}

async fn listed(ts: &TestServer, name: &str) -> BTreeSet<BlobId> {
    LocalFsStore::new(ts.local_repo_root(name))
        .list(INDEX_PREFIX)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| parse_hex32(e.key.strip_prefix(INDEX_PREFIX)?))
        .collect()
}

async fn attest_objects(ts: &TestServer, name: &str) -> usize {
    LocalFsStore::new(ts.local_repo_root(name))
        .list(ATTEST_PREFIX)
        .await
        .unwrap()
        .len()
}

async fn server_keys(ts: &TestServer, id: &str) -> ReadKeys {
    open_repository(ts.state(), id).await.unwrap().keys
}

async fn flag(ts: &TestServer, id: &str) -> Option<i64> {
    sqlx::query("SELECT attest_upgraded_at FROM repositories WHERE id = $1")
        .bind(id)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .try_get("attest_upgraded_at")
        .unwrap()
}

async fn clear_flag(ts: &TestServer, id: &str) {
    sqlx::query("UPDATE repositories SET attest_upgraded_at = NULL WHERE id = $1")
        .bind(id)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_upgrade_tick_signs_existing_shards() {
    for backend in backends() {
        let ts = start(&backend).await;
        let id = ts.create_local_repository("legacy").await;
        backup(&ts, "legacy").await;
        let shards = listed(&ts, "legacy").await;
        assert!(!shards.is_empty());
        assert_eq!(flag(&ts, &id).await, None);

        tick_once(ts.state()).await.unwrap();

        let keys = server_keys(&ts, &id).await;
        let store = LocalFsStore::new(ts.local_repo_root("legacy"));
        let set = read_attestations(&store, &keys).await.unwrap();
        assert_eq!(set.shards.iter().copied().collect::<BTreeSet<_>>(), shards);
        assert!(set.markers.contains(&MARKER_LEGACY_SHARDS));
        assert!(flag(&ts, &id).await.is_some(), "{}", backend.name());
        assert!(load_verified(ts.state(), &id).await.unwrap().is_empty());

        let objects = attest_objects(&ts, "legacy").await;
        tick_once(ts.state()).await.unwrap();
        clear_flag(&ts, &id).await;
        tick_once(ts.state()).await.unwrap();
        assert_eq!(attest_objects(&ts, "legacy").await, objects);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_upgrade_survives_database_loss() {
    for backend in backends() {
        let ts = start(&backend).await;
        let id = ts.create_local_repository("legacy").await;
        backup(&ts, "legacy").await;
        tick_once(ts.state()).await.unwrap();

        let planted = plant_shard(&ts, "legacy").await;
        clear_flag(&ts, &id).await;
        tick_once(ts.state()).await.unwrap();

        let keys = server_keys(&ts, &id).await;
        let store = LocalFsStore::new(ts.local_repo_root("legacy"));
        let set = read_attestations(&store, &keys).await.unwrap();
        assert!(!set.shards.contains(&planted), "{}", backend.name());
        assert!(flag(&ts, &id).await.is_some());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_upgrade_refuses_when_marker_was_removed() {
    for backend in backends() {
        let ts = start(&backend).await;
        let id = ts.create_local_repository("legacy").await;
        backup(&ts, "legacy").await;
        tick_once(ts.state()).await.unwrap();

        let keys = server_keys(&ts, &id).await;
        let store = LocalFsStore::new(ts.local_repo_root("legacy"));
        put_attestation(&store, &keys, &[], &[], Vec::new(), &mut Entropy::os())
            .await
            .unwrap();
        for entry in store.list(ATTEST_PREFIX).await.unwrap() {
            let bytes = store.get(&entry.key).await.unwrap();
            let attestation =
                open_attestation(keys.k_srv(), keys.repo_id(), &entry.key, &bytes).unwrap();
            if attestation.markers.contains(&MARKER_LEGACY_SHARDS) {
                std::fs::remove_file(ts.local_repo_root("legacy").join(&entry.key)).unwrap();
            }
        }

        let planted = plant_shard(&ts, "legacy").await;
        clear_flag(&ts, &id).await;
        let objects = attest_objects(&ts, "legacy").await;

        assert!(ensure_upgraded(ts.state(), &id).await.is_err());

        assert_eq!(attest_objects(&ts, "legacy").await, objects);
        let set = read_attestations(&store, &keys).await.unwrap();
        assert!(!set.shards.contains(&planted), "{}", backend.name());
        assert_eq!(flag(&ts, &id).await, None);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_upgrade_records_verified_shards() {
    for backend in backends() {
        let ts = start(&backend).await;
        let id = ts.create_local_repository("legacy").await;
        let a: BlobId = [1; 32];
        let b: BlobId = [2; 32];
        let c: BlobId = [3; 32];
        record_verified(ts.state(), &id, &[a, b]).await.unwrap();
        record_verified(ts.state(), &id, &[b, c]).await.unwrap();
        let got = load_verified(ts.state(), &id).await.unwrap();
        assert_eq!(got, [a, b, c].into_iter().collect(), "{}", backend.name());
        assert_eq!(hex32(&a).len(), 64);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_upgrade_new_repository_gets_marker_on_creation() {
    for backend in backends() {
        let ts = start(&backend).await;
        let api = ts.api().await;
        let root = ts.local_repo_root("fresh");
        let draft = json!({
            "name": "fresh",
            "location": { "backend": "local", "options": { "path": root.display().to_string() } },
        });
        let started = api.post("/api/v1/repositories/wizard", &draft).await;
        assert_eq!(started.status(), 201);
        let started: serde_json::Value = started.json().await.unwrap();
        let key = started["recovery_key"].as_str().unwrap();
        let committed = api
            .post(
                &format!(
                    "/api/v1/repositories/wizard/{}/commit",
                    started["wizard_id"].as_str().unwrap()
                ),
                &json!({
                    "password": "correct horse battery",
                    "recovery_key_last_group": key.rsplit('-').next().unwrap(),
                }),
            )
            .await;
        assert_eq!(committed.status(), 201);
        let committed: serde_json::Value = committed.json().await.unwrap();
        let id = committed["id"].as_str().unwrap();

        let keys = server_keys(&ts, id).await;
        let store = LocalFsStore::new(root);
        let set = read_attestations(&store, &keys).await.unwrap();
        assert!(
            set.markers.contains(&MARKER_LEGACY_SHARDS),
            "{}",
            backend.name()
        );
        assert!(set.shards.is_empty());
        assert!(flag(&ts, id).await.is_some());
        ts.stop().await;
    }
}
