mod common;
#[path = "../../lodestore-format/tests/tree_support/mod.rs"]
mod tree_support;

use std::io::Read;

use common::{Api, TestServer, WAIT, backends, wait_until};
use data_encoding::BASE64URL_NOPAD;
use lodestore_format::index::hex32;
use lodestore_format::repo::Repository;
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, Writer};
use serde_json::{Value, json};

const PASSWORD: &[u8] = b"test-password";

fn b64(bytes: &[u8]) -> String {
    BASE64URL_NOPAD.encode(bytes)
}

async fn assert_too_large(response: reqwest::Response, what: &str) {
    assert_eq!(response.status(), 400, "{what}");
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["code"], "tree_too_large", "{what}: {body}");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|m| m.contains("bytes in one directory")),
        "{what}: {body}"
    );
}

async fn find_session(api: &Api, pattern: &str) -> Value {
    let started: Value = api
        .post("/api/v1/find", &json!({ "pattern": pattern }))
        .await
        .json()
        .await
        .unwrap();
    let url = format!("/api/v1/find/{}", started["id"].as_str().unwrap());
    let done = wait_until(WAIT, || async {
        let found: Value = api.get(&url).await.json().await.unwrap();
        found["state"] != "running"
    })
    .await;
    assert!(done, "the search never finished");
    api.get(&url).await.json().await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tree_limits_wide_directory_is_refused_on_every_http_path() {
    let backend = backends().remove(0);
    let ts = TestServer::start(&backend).await;
    let api = ts.api().await;
    let repo = ts.create_local_repository("main").await;
    let store = LocalFsStore::new(ts.local_repo_root("main"));
    let opened = Repository::open(&store, PASSWORD).await.unwrap();
    let keys = opened.read_keys().write_keys(opened.config_hash());
    let mut writer = Writer::new(&store, keys, opened.config()).await.unwrap();

    let data = ts.dir.path().join("tree");
    std::fs::create_dir_all(data.join("etc")).unwrap();
    std::fs::write(data.join("etc/app.conf"), b"app").unwrap();
    writer
        .backup(
            std::slice::from_ref(&data),
            BackupOptions {
                hostname: Some(b"host-a".to_vec()),
                ..BackupOptions::default()
            },
            &|_| {},
        )
        .await
        .expect("backup");
    let pieces = tree_support::wide_piece_count();
    let wide = hex32(&tree_support::wide_snapshot(&mut writer, pieces).await);

    let synced = api
        .post(
            &format!("/api/v1/repositories/{repo}/catalog/sync"),
            &json!({}),
        )
        .await;
    assert_eq!(synced.status(), 200);
    sqlx::query(
        "UPDATE snapshots SET received_at = time_ms, verified_at = time_ms, verified_by = 'server'",
    )
    .execute(&ts.state().db.pool)
    .await
    .unwrap();

    let entries = |path: &str| format!("/api/v1/snapshots/{wide}/entries?path={path}");
    assert_too_large(
        api.get(&entries(&b64(b"/wide"))).await,
        "entries of the wide directory",
    )
    .await;
    let root = api.get(&entries("")).await;
    assert_eq!(root.status(), 200, "the small root still lists");

    assert_too_large(
        api.post(
            &format!("/api/v1/snapshots/{wide}/summary"),
            &json!({ "items": [b64(b"/wide")] }),
        )
        .await,
        "summary of the wide directory",
    )
    .await;

    let download =
        |item: &[u8]| json!({ "snapshot_id": wide, "items": [b64(item)], "format": "tar_gz" });
    assert_too_large(
        api.post("/api/v1/downloads", &download(b"/wide/f000000"))
            .await,
        "archive of an item inside the wide directory",
    )
    .await;

    let created = api.post("/api/v1/downloads", &download(b"/wide")).await;
    assert_eq!(created.status(), 201);
    let created: Value = created.json().await.unwrap();
    let archive = api.get(created["url"].as_str().unwrap()).await;
    assert_eq!(archive.status(), 200);
    let cut_short = match archive.bytes().await {
        Err(_) => true,
        Ok(bytes) => flate2::read::GzDecoder::new(&bytes[..])
            .read_to_end(&mut Vec::new())
            .is_err(),
    };
    assert!(
        cut_short,
        "the archive of the wide directory must not end cleanly"
    );

    let found = find_session(&api, "*.conf").await;
    assert_eq!(found["state"], "incomplete", "{found}");
    assert_eq!(found["stop_reason"], "walk_limit", "{found}");
    assert!(found["skipped"].as_u64().unwrap() >= 1, "{found}");
    assert_eq!(
        found["files"], 1,
        "the honest snapshot is still found: {found}"
    );

    ts.stop().await;
}
