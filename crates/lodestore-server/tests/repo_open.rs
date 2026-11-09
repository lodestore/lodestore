mod common;

use std::path::Path;
use std::time::Duration;

use common::{Api, TestServer, backends, wait_until};
use lodestore_format::config;
use lodestore_server::catalog::{RepositoryUnavailable, open_repository};
use lodestore_server::repos::db::writer_material;
use serde_json::{Value, json};
use sqlx::Row;

async fn set_hash(ts: &TestServer, id: &str, hash: &[u8]) {
    sqlx::query("UPDATE repositories SET config_hash = $1 WHERE id = $2")
        .bind(hash.to_vec())
        .bind(id)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
}

async fn recorded_hash(ts: &TestServer, id: &str) -> Vec<u8> {
    sqlx::query("SELECT config_hash FROM repositories WHERE id = $1")
        .bind(id)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .try_get("config_hash")
        .unwrap()
}

async fn reason(api: &Api, id: &str) -> Value {
    let response = api.get(&format!("/api/v1/repositories/{id}")).await;
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    body["unavailable_reason"].clone()
}

async fn sync(api: &Api, id: &str) -> reqwest::Response {
    api.post(
        &format!("/api/v1/repositories/{id}/catalog/sync"),
        &json!({}),
    )
    .await
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repo_open_refuses_another_config_hash() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let id = ts.create_local_repository("main").await;
        assert!(open_repository(ts.state(), &id).await.is_ok());
        assert_eq!(reason(&api, &id).await, Value::Null);

        set_hash(&ts, &id, &[9; 32]).await;
        assert!(open_repository(ts.state(), &id).await.is_err());
        let shown = reason(&api, &id).await;
        assert!(
            shown
                .as_str()
                .unwrap()
                .contains("differs from the configuration recorded"),
            "[{}] {shown}",
            backend.name()
        );
        assert!(!sync(&api, &id).await.status().is_success());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repo_open_refuses_unknown_features() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let id = ts.create_local_repository("main").await;
        let path = ts.local_repo_root("main").join("config");
        let mut parsed = config::decode(&std::fs::read(&path).unwrap()).unwrap();
        parsed.incompat_features = vec!["future".to_string()];
        let bytes = config::encode(&parsed);
        std::fs::write(&path, &bytes).unwrap();
        set_hash(&ts, &id, &config::config_hash(&bytes)).await;

        assert!(open_repository(ts.state(), &id).await.is_err());
        let shown = reason(&api, &id).await;
        assert!(
            shown.as_str().unwrap().contains("cannot read or write"),
            "[{}] {shown}",
            backend.name()
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repo_open_other_repositories_keep_working() {
    for backend in backends() {
        let mut ts = TestServer::start(&backend).await;
        let bad = ts.create_local_repository("bad").await;
        let good = ts.create_local_repository("good").await;
        set_hash(&ts, &bad, &[9; 32]).await;
        ts.restart().await;
        let api = ts.api().await;

        let marked = wait_until(Duration::from_secs(20), || async {
            reason(&api, &bad).await.is_string()
        })
        .await;
        assert!(
            marked,
            "[{}] the start-up probe marks the repository",
            backend.name()
        );
        assert_eq!(reason(&api, &good).await, Value::Null);
        assert!(sync(&api, &good).await.status().is_success());
        assert!(!sync(&api, &bad).await.status().is_success());
        let list: Value = api.get("/api/v1/repositories").await.json().await.unwrap();
        assert_eq!(list.as_array().unwrap().len(), 2);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repo_open_clears_after_fix() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let id = ts.create_local_repository("main").await;
        let good = recorded_hash(&ts, &id).await;

        set_hash(&ts, &id, &[9; 32]).await;
        assert!(open_repository(ts.state(), &id).await.is_err());
        assert!(reason(&api, &id).await.is_string());

        set_hash(&ts, &id, &good).await;
        assert!(open_repository(ts.state(), &id).await.is_ok());
        assert_eq!(reason(&api, &id).await, Value::Null, "[{}]", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repo_open_store_error_is_not_recorded() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let id = ts.create_local_repository("main").await;
        std::fs::remove_file(ts.local_repo_root("main").join("config")).unwrap();

        assert!(open_repository(ts.state(), &id).await.is_err());
        assert_eq!(reason(&api, &id).await, Value::Null, "[{}]", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repo_open_oversized_config_is_recorded() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let id = ts.create_local_repository("main").await;
        let path = ts.local_repo_root("main").join("config");
        let original = std::fs::metadata(&path).unwrap().len();
        let resize = |len: u64| {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(len)
                .unwrap();
        };

        resize(config::MAX_CONFIG_SIZE + 1);
        let error = open_repository(ts.state(), &id).await.err().unwrap();
        assert!(error.downcast_ref::<RepositoryUnavailable>().is_some());
        let shown = reason(&api, &id).await;
        assert!(
            shown.as_str().unwrap().contains("config"),
            "[{}] {shown}",
            backend.name()
        );

        resize(original);
        assert!(open_repository(ts.state(), &id).await.is_ok());
        assert_eq!(reason(&api, &id).await, Value::Null, "[{}]", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repo_open_v1_fixture_connects() {
    let fixtures =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../lodestore-format/tests/fixtures/v1");
    let password = std::fs::read_to_string(fixtures.join("PASSWORD")).unwrap();
    let expected: Value =
        serde_json::from_slice(&std::fs::read(fixtures.join("expected.json")).unwrap()).unwrap();
    let expected_ids: Vec<String> = expected["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_string())
        .collect();
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let root = ts.local_repo_root("v1");
        copy_dir(&fixtures.join("repo"), &root);

        let connected = api
            .post(
                "/api/v1/repositories/connect",
                &json!({
                    "draft": {
                        "name": "v1",
                        "location": {
                            "backend": "local",
                            "options": { "path": root.display().to_string() },
                        },
                    },
                    "password": password.trim(),
                    "recovery_key": null,
                }),
            )
            .await;
        assert_eq!(connected.status(), 201, "[{}]", backend.name());
        let id = connected.json::<Value>().await.unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        assert!(open_repository(ts.state(), &id).await.is_ok());
        let synced = sync(&api, &id).await;
        assert_eq!(synced.status(), 200);
        assert_eq!(reason(&api, &id).await, Value::Null);

        let mut ids: Vec<String> = sqlx::query("SELECT id FROM snapshots WHERE repository_id = $1")
            .bind(&id)
            .fetch_all(&ts.state().db.pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.try_get("id").unwrap())
            .collect();
        ids.sort();
        let mut want = expected_ids.clone();
        want.sort();
        assert_eq!(ids, want, "[{}]", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repo_open_browse_answers_unavailable() {
    let fixtures =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../lodestore-format/tests/fixtures/v1");
    let password = std::fs::read_to_string(fixtures.join("PASSWORD")).unwrap();
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let root = ts.local_repo_root("v1");
        copy_dir(&fixtures.join("repo"), &root);
        let connected = api
            .post(
                "/api/v1/repositories/connect",
                &json!({
                    "draft": {
                        "name": "v1",
                        "location": {
                            "backend": "local",
                            "options": { "path": root.display().to_string() },
                        },
                    },
                    "password": password.trim(),
                    "recovery_key": null,
                }),
            )
            .await;
        assert_eq!(connected.status(), 201, "[{}]", backend.name());
        let id = connected.json::<Value>().await.unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(sync(&api, &id).await.status(), 200);
        let snapshot: String = sqlx::query("SELECT id FROM snapshots WHERE repository_id = $1")
            .bind(&id)
            .fetch_one(&ts.state().db.pool)
            .await
            .unwrap()
            .try_get("id")
            .unwrap();

        set_hash(&ts, &id, &[9; 32]).await;
        let entries = api
            .get(&format!("/api/v1/snapshots/{snapshot}/entries"))
            .await;
        let download = api
            .post(
                "/api/v1/downloads",
                &json!({ "snapshot_id": snapshot, "items": ["L2E"], "format": "zip" }),
            )
            .await;
        for response in [entries, download] {
            assert_eq!(response.status(), 503, "[{}]", backend.name());
            let body: Value = response.json().await.unwrap();
            assert_eq!(body["code"], "repository_unavailable");
            assert!(
                body["message"]
                    .as_str()
                    .unwrap()
                    .contains("differs from the configuration recorded"),
                "[{}] {body}",
                backend.name()
            );
        }
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repo_open_refuses_writer_material() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let id = ts.create_local_repository("main").await;
        let good = recorded_hash(&ts, &id).await;

        set_hash(&ts, &id, &[9; 32]).await;
        assert!(open_repository(ts.state(), &id).await.is_err());
        let refused = writer_material(ts.state(), &id).await.err().unwrap();
        assert!(
            refused.downcast_ref::<RepositoryUnavailable>().is_some(),
            "[{}] {refused:#}",
            backend.name()
        );

        set_hash(&ts, &id, &good).await;
        assert!(open_repository(ts.state(), &id).await.is_ok());
        assert!(writer_material(ts.state(), &id).await.is_ok());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repo_open_recheck_clears_after_fix() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let id = ts.create_local_repository("main").await;
        let good = recorded_hash(&ts, &id).await;

        set_hash(&ts, &id, &[9; 32]).await;
        assert!(open_repository(ts.state(), &id).await.is_err());
        assert!(reason(&api, &id).await.is_string());

        set_hash(&ts, &id, &good).await;
        assert!(reason(&api, &id).await.is_string());
        let checked = api
            .post(&format!("/api/v1/repositories/{id}/check"), &json!({}))
            .await;
        assert_eq!(checked.status(), 200, "[{}]", backend.name());
        assert_eq!(reason(&api, &id).await, Value::Null, "[{}]", backend.name());
        ts.stop().await;
    }
}
