mod common;

use std::time::{Duration, Instant};

use common::{Api, TestServer, backends};
use lodestore_format::crypto::Entropy;
use lodestore_format::index::{hex32, parse_hex32};
use lodestore_format::repo::Repository;
use lodestore_format::snapshot::{open_snapshot, seal_snapshot, snapshot_object_key};
use lodestore_format::store::{LocalFsStore, put_named};
use lodestore_format::writer::{BackupOptions, Writer};
use serde_json::{Value, json};

const PASSWORD: &[u8] = b"test-password";

async fn write_snapshot(ts: &TestServer, name: &str, hostname: &str) -> String {
    let store = LocalFsStore::new(ts.local_repo_root(name));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    let mut writer = Writer::new(&store, keys, repo.config()).await.unwrap();
    let data = ts.dir.path().join(format!("data-{hostname}"));
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("file.txt"), format!("content of {hostname}")).unwrap();
    let outcome = writer
        .backup(
            &[data],
            BackupOptions {
                hostname: Some(hostname.as_bytes().to_vec()),
                ..BackupOptions::default()
            },
            &|_| {},
        )
        .await
        .expect("backup");
    hex32(&outcome.snapshot_id)
}

async fn forge_variants(ts: &TestServer, name: &str, original: &str, count: usize) {
    let store = LocalFsStore::new(ts.local_repo_root(name));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let keys = repo.read_keys();
    let id = parse_hex32(original).unwrap();
    let bytes = std::fs::read(ts.local_repo_root(name).join(snapshot_object_key(&id))).unwrap();
    let snapshot = open_snapshot(keys.hpke_private_key(), keys.repo_id(), &id, &bytes).unwrap();
    for n in 0..count {
        let mut variant = snapshot.clone();
        variant.tags = vec![format!("n={n}")];
        let (forged, bytes) = seal_snapshot(
            &variant,
            keys.hpke_public_key(),
            keys.repo_id(),
            &mut Entropy::os(),
        )
        .unwrap();
        put_named(&store, &snapshot_object_key(&forged), bytes)
            .await
            .unwrap();
    }
}

async fn catalog_of(api: &Api, repository_id: &str) -> Value {
    let all: Vec<Value> = api.get("/api/v1/catalog").await.json().await.unwrap();
    all.into_iter()
        .find(|s| s["repository_id"] == repository_id)
        .expect("the repository has a catalog status")
}

async fn wait_finished(api: &Api, repository_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let status = catalog_of(api, repository_id).await;
        if matches!(status["phase"].as_str().unwrap(), "done" | "failed") {
            return status;
        }
        assert!(Instant::now() < deadline, "the rebuild did not finish");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn rebuild(api: &Api, repository_id: &str) -> reqwest::Response {
    api.post(
        &format!("/api/v1/repositories/{repository_id}/catalog/rebuild"),
        &json!({}),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rebuild_into_empty_db_reports_progress() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let original = write_snapshot(&ts, "main", "host-a").await;
        forge_variants(&ts, "main", &original, 19).await;

        let started = rebuild(&api, &repo).await;
        assert_eq!(started.status(), 202, "[{}]", backend.name());
        let started: Value = started.json().await.unwrap();
        assert!(matches!(
            started["phase"].as_str().unwrap(),
            "listing" | "reading"
        ));
        assert_eq!(started["repository_name"], "main");

        let mut most_read = 0;
        let deadline = Instant::now() + Duration::from_secs(60);
        let finished = loop {
            let status = catalog_of(&api, &repo).await;
            most_read = most_read.max(status["read"].as_u64().unwrap());
            if status["phase"] == "done" {
                break status;
            }
            assert_ne!(status["phase"], "failed", "{status}");
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        assert_eq!(finished["total"], 20);
        assert_eq!(finished["read"], 20);
        assert_eq!(finished["added"], 20);
        assert_eq!(finished["snapshots"], 20);
        assert_eq!(finished["skipped"].as_array().unwrap().len(), 0);
        assert!(finished["last_sync_at"].is_i64());
        assert!(finished["finished_at"].is_i64());
        assert_eq!(most_read, 20);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rebuild_skips_unreadable_with_reason() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        write_snapshot(&ts, "main", "host-a").await;
        let dir = ts.local_repo_root("main").join("snapshots");
        let broken = format!("snapshots/{}", "0".repeat(64));
        std::fs::write(dir.join("0".repeat(64)), b"not a snapshot").unwrap();
        std::fs::write(dir.join("zz"), b"not a snapshot").unwrap();

        assert_eq!(rebuild(&api, &repo).await.status(), 202);
        let status = wait_finished(&api, &repo).await;
        assert_eq!(status["phase"], "done", "[{}]", backend.name());
        assert_eq!(status["added"], 1);
        assert_eq!(status["total"], 2);
        let skipped = status["skipped"].as_array().unwrap();
        assert_eq!(skipped.len(), 2);
        for entry in skipped {
            assert!(!entry["reason"].as_str().unwrap().is_empty(), "{entry}");
        }
        let keys: Vec<&str> = skipped.iter().map(|s| s["key"].as_str().unwrap()).collect();
        assert!(keys.contains(&"snapshots/zz"));
        assert!(keys.contains(&broken.as_str()));
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rebuild_reads_legacy_skipped_json() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        sqlx::query(
            "INSERT INTO catalog_state (repository_id, last_sync_at, last_error, skipped_json) \
             VALUES ($1, $2, NULL, $3)",
        )
        .bind(&repo)
        .bind(1_000_i64)
        .bind(r#"["snapshots/x"]"#)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();

        let status = catalog_of(&api, &repo).await;
        assert_eq!(status["phase"], "idle", "[{}]", backend.name());
        assert_eq!(status["last_sync_at"], 1_000);
        assert_eq!(
            status["skipped"],
            json!([{ "key": "snapshots/x", "reason": "" }])
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rebuild_is_single_flight() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let original = write_snapshot(&ts, "main", "host-a").await;
        forge_variants(&ts, "main", &original, 79).await;

        let state = ts.state();
        let first = lodestore_server::catalog::start_rebuild(state, &repo);
        let second = lodestore_server::catalog::start_rebuild(state, &repo);
        assert_eq!(first.started_at, second.started_at, "[{}]", backend.name());

        let sync = async {
            let response = api
                .post(
                    &format!("/api/v1/repositories/{repo}/catalog/sync"),
                    &json!({}),
                )
                .await;
            assert_eq!(response.status(), 200);
            response.json::<Value>().await.unwrap()
        };
        let (a, b, report) = tokio::join!(rebuild(&api, &repo), rebuild(&api, &repo), sync);
        let (a, b): (Value, Value) = (a.json().await.unwrap(), b.json().await.unwrap());
        assert_eq!(a["started_at"].as_i64(), first.started_at);
        assert_eq!(b["started_at"].as_i64(), first.started_at);
        assert_eq!(report["added"], 80);
        assert_eq!(report["total"], 80);

        let status = wait_finished(&api, &repo).await;
        assert_eq!(status["started_at"].as_i64(), first.started_at);
        assert_eq!(status["added"], 80);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rebuild_resumes_after_restart() {
    for backend in backends() {
        let mut ts = TestServer::start(&backend).await;
        let repo = ts.create_local_repository("main").await;
        let original = write_snapshot(&ts, "main", "host-a").await;
        forge_variants(&ts, "main", &original, 4).await;

        ts.restart().await;
        let api = ts.api().await;
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let status = catalog_of(&api, &repo).await;
            let settled = status["phase"] == "done" || status["phase"] == "idle";
            if settled && status["snapshots"] == 5 && status["last_sync_at"].is_i64() {
                break;
            }
            assert_ne!(status["phase"], "failed", "{status}");
            assert!(
                Instant::now() < deadline,
                "the catalog did not catch up after the restart"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rebuild_never_deletes_rows() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let original = write_snapshot(&ts, "main", "host-a").await;
        forge_variants(&ts, "main", &original, 2).await;

        assert_eq!(rebuild(&api, &repo).await.status(), 202);
        let status = wait_finished(&api, &repo).await;
        assert_eq!(status["snapshots"], 3, "[{}]", backend.name());

        std::fs::remove_file(
            ts.local_repo_root("main")
                .join(format!("snapshots/{original}")),
        )
        .unwrap();
        assert_eq!(rebuild(&api, &repo).await.status(), 202);
        let status = wait_finished(&api, &repo).await;
        assert_eq!(status["phase"], "done");
        assert_eq!(status["added"], 0);
        assert_eq!(status["snapshots"], 3);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn routine_sync_stays_out_of_the_rebuild_status() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let sync = || async {
            let response = api
                .post(
                    &format!("/api/v1/repositories/{repo}/catalog/sync"),
                    &json!({}),
                )
                .await;
            assert_eq!(response.status(), 200);
            response.json::<Value>().await.unwrap()
        };

        write_snapshot(&ts, "main", "host-a").await;
        assert_eq!(sync().await["added"], 1, "[{}]", backend.name());
        let status = catalog_of(&api, &repo).await;
        assert_eq!(status["phase"], "idle", "{status}");
        assert!(status["started_at"].is_null());
        assert_eq!(status["snapshots"], 1);

        assert_eq!(rebuild(&api, &repo).await.status(), 202);
        let rebuilt = wait_finished(&api, &repo).await;
        write_snapshot(&ts, "main", "host-b").await;
        assert_eq!(sync().await["added"], 1);
        let status = catalog_of(&api, &repo).await;
        assert_eq!(status["phase"], "done", "{status}");
        assert_eq!(status["started_at"], rebuilt["started_at"]);
        assert_eq!(status["snapshots"], 2);
        ts.stop().await;
    }
}
