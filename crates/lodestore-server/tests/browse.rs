mod common;

use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;

use common::{Api, TestServer, backends};
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

fn write(path: &Path, data: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, data).unwrap();
}

struct Seeded {
    repo: String,
    snapshot: String,
    base: String,
}

impl Seeded {
    fn path(&self, below: &str) -> String {
        b64(format!("{}/{below}", self.base).as_bytes())
    }
}

async fn seed(ts: &TestServer, api: &Api) -> Seeded {
    let repo = ts.create_local_repository("main").await;
    let store = LocalFsStore::new(ts.local_repo_root("main"));
    let opened = Repository::open(&store, PASSWORD).await.unwrap();
    let keys = opened.read_keys().write_keys(opened.config_hash());
    let mut writer = Writer::new(&store, keys, opened.config()).await.unwrap();

    let data = ts.dir.path().join("tree");
    for i in 0..450 {
        write(
            &data.join(format!("big/f{i:03}")),
            format!("content {i}").as_bytes(),
        );
    }
    write(&data.join("big/alpha/inner.txt"), b"alpha");
    write(&data.join("big/Beta/inner.txt"), b"beta");
    write(&data.join("big/zeta/deep/leaf.txt"), b"zeta leaf");
    std::os::unix::fs::symlink("f000", data.join("big/ln")).unwrap();
    for i in 0..600 {
        write(&data.join(format!("many/match-{i:03}.txt")), b"m");
    }
    write(&data.join("other/report-outside.txt"), b"outside");
    write(&data.join("docs/Report-2024.txt"), b"annual");
    write(&data.join("docs/report.pdf"), b"pdf bytes");
    write(&data.join("docs/reports/q1.txt"), b"q1");
    write(&data.join("docs/notes.md"), b"notes");
    write(
        &data.join("docs/\u{43e}\u{442}\u{447}\u{451}\u{442}.txt"),
        b"cyr",
    );

    let outcome = writer
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

    let response = api
        .post(
            &format!("/api/v1/repositories/{repo}/catalog/sync"),
            &json!({}),
        )
        .await;
    assert_eq!(response.status(), 200);
    Seeded {
        repo,
        snapshot: hex32(&outcome.snapshot_id),
        base: data.canonicalize().unwrap().display().to_string(),
    }
}

async fn entries(api: &Api, seeded: &Seeded, path: &str, offset: u64) -> Value {
    let response = api
        .get(&format!(
            "/api/v1/snapshots/{}/entries?path={path}&offset={offset}",
            seeded.snapshot
        ))
        .await;
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

fn names(page: &Value) -> Vec<String> {
    page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browse_entries_lists_dirs_first_and_pages() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let seeded = seed(&ts, &api).await;

        let top = entries(&api, &seeded, "", 0).await;
        assert_eq!(top["total"], 1, "[{}]", backend.name());

        let first = entries(&api, &seeded, &seeded.path("big"), 0).await;
        assert_eq!(first["total"], 454);
        assert_eq!(first["entries"].as_array().unwrap().len(), 200);
        let kinds: Vec<&str> = first["entries"]
            .as_array()
            .unwrap()
            .iter()
            .take(4)
            .map(|e| e["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["dir", "dir", "dir", "file"]);
        assert_eq!(
            &names(&first)[..5],
            ["alpha", "Beta", "zeta", "f000", "f001"]
        );

        let second = entries(&api, &seeded, &seeded.path("big"), 200).await;
        assert_eq!(second["offset"], 200);
        assert_eq!(second["entries"].as_array().unwrap().len(), 200);
        let third = entries(&api, &seeded, &seeded.path("big"), 400).await;
        assert_eq!(third["entries"].as_array().unwrap().len(), 54);

        let mut all = names(&first);
        all.extend(names(&second));
        all.extend(names(&third));
        assert_eq!(all.len(), 454);
        assert_eq!(all[453], "ln");
        assert_eq!(all[452], "f449");

        let link = third["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == "ln")
            .unwrap();
        assert_eq!(link["kind"], "symlink");
        assert_eq!(link["link_target"], "f000");
        let file = &first["entries"][3];
        assert_eq!(file["size"], 9);
        assert!(file["mtime_ms"].as_i64().unwrap() > 0);
        assert_eq!(file["name_b64"], b64(b"f000"));

        let beyond = entries(&api, &seeded, &seeded.path("big"), 4000).await;
        assert!(beyond["entries"].as_array().unwrap().is_empty());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browse_entries_non_utf8_name_roundtrip() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let odd = std::ffi::OsStr::from_bytes(b"d-\xff");
        let probe = ts.dir.path().join("probe");
        std::fs::create_dir_all(&probe).unwrap();
        if std::fs::create_dir(probe.join(odd)).is_err() {
            eprintln!("skipped: this filesystem refuses non-UTF-8 names");
            ts.stop().await;
            return;
        }
        let repo = ts.create_local_repository("main").await;
        let store = LocalFsStore::new(ts.local_repo_root("main"));
        let opened = Repository::open(&store, PASSWORD).await.unwrap();
        let keys = opened.read_keys().write_keys(opened.config_hash());
        let mut writer = Writer::new(&store, keys, opened.config()).await.unwrap();
        write(&probe.join(odd).join("inner.txt"), b"odd");
        let outcome = writer
            .backup(
                std::slice::from_ref(&probe),
                BackupOptions {
                    hostname: Some(b"host-a".to_vec()),
                    ..BackupOptions::default()
                },
                &|_| {},
            )
            .await
            .expect("backup");
        api.post(
            &format!("/api/v1/repositories/{repo}/catalog/sync"),
            &json!({}),
        )
        .await;
        let seeded = Seeded {
            repo,
            snapshot: hex32(&outcome.snapshot_id),
            base: probe.canonicalize().unwrap().display().to_string(),
        };

        let listing = entries(&api, &seeded, &seeded.path(""), 0).await;
        let item = &listing["entries"][0];
        assert!(item["name"].as_str().unwrap().contains('\u{fffd}'));
        let name = BASE64URL_NOPAD
            .decode(item["name_b64"].as_str().unwrap().as_bytes())
            .unwrap();
        assert_eq!(name, b"d-\xff");

        let mut path = format!("{}/", seeded.base).into_bytes();
        path.extend_from_slice(&name);
        let inner = entries(&api, &seeded, &b64(&path), 0).await;
        assert_eq!(names(&inner), ["inner.txt"]);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browse_entries_errors() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let seeded = seed(&ts, &api).await;
        let get = |snapshot: String, path: String| {
            let api = &api;
            async move {
                api.get(&format!("/api/v1/snapshots/{snapshot}/entries?path={path}"))
                    .await
                    .status()
                    .as_u16()
            }
        };

        assert_eq!(get("0".repeat(64), String::new()).await, 404);
        assert_eq!(get("not-hex".into(), String::new()).await, 404);
        assert_eq!(
            get(seeded.snapshot.clone(), seeded.path("nowhere")).await,
            404,
            "[{}]",
            backend.name()
        );
        assert_eq!(
            get(seeded.snapshot.clone(), seeded.path("docs/notes.md")).await,
            404
        );
        assert_eq!(get(seeded.snapshot.clone(), "!!!".into()).await, 422);
        assert_eq!(get(seeded.snapshot.clone(), b64(b"/a/../b")).await, 422);

        std::fs::remove_dir_all(ts.local_repo_root("main").join("snapshots")).unwrap();
        assert_eq!(get(seeded.snapshot.clone(), String::new()).await, 404);
        let response = api
            .get(&format!("/api/v1/snapshots/{}/entries", seeded.snapshot))
            .await;
        let body: Value = response.json().await.unwrap();
        assert!(
            body["message"]
                .as_str()
                .unwrap()
                .contains("no longer in storage")
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browse_entries_cached_second_time() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let seeded = seed(&ts, &api).await;
        let path = seeded.path("big");

        let before = entries(&api, &seeded, &path, 0).await;
        let readers = &ts.state().readers;
        assert!(Arc::ptr_eq(
            &readers.cache(&seeded.repo),
            &readers.cache(&seeded.repo)
        ));

        let root = ts.local_repo_root("main");
        std::fs::remove_dir_all(root.join("packs")).unwrap();
        std::fs::remove_dir_all(root.join("index")).unwrap();
        let after = entries(&api, &seeded, &path, 0).await;
        assert_eq!(names(&before), names(&after), "[{}]", backend.name());
        ts.stop().await;
    }
}

async fn search(api: &Api, seeded: &Seeded, path: &str, q: &str) -> Vec<Value> {
    let response = api
        .get(&format!(
            "/api/v1/snapshots/{}/search?path={path}&q={q}",
            seeded.snapshot
        ))
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "application/x-ndjson");
    response
        .text()
        .await
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn found(lines: &[Value]) -> Vec<String> {
    let mut names: Vec<String> = lines
        .iter()
        .filter_map(|l| l.get("match"))
        .map(|m| m["name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    names
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browse_search_streams_case_insensitive() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let seeded = seed(&ts, &api).await;

        let docs = search(&api, &seeded, &seeded.path("docs"), "REPORT").await;
        assert_eq!(
            found(&docs),
            ["Report-2024.txt", "report.pdf", "reports"],
            "[{}]",
            backend.name()
        );
        assert_eq!(
            docs.last().unwrap(),
            &json!({"done": {"count": 3, "truncated": false}})
        );

        let whole = search(&api, &seeded, "", "report").await;
        assert_eq!(found(&whole).len(), 4);

        let leaf = search(&api, &seeded, "", "q1").await;
        let m = &leaf[0]["match"];
        assert_eq!(m["kind"], "file");
        assert_eq!(m["parent"], format!("{}/docs/reports", seeded.base));
        assert_eq!(
            m["parent_b64"],
            b64(format!("{}/docs/reports", seeded.base).as_bytes())
        );

        let cyrillic = search(&api, &seeded, "", "%D0%9E%D1%82%D1%87%D1%91%D1%82").await;
        assert_eq!(
            found(&cyrillic),
            ["\u{43e}\u{442}\u{447}\u{451}\u{442}.txt"]
        );

        let none = search(&api, &seeded, &seeded.path("docs"), "outside").await;
        assert_eq!(none, [json!({"done": {"count": 0, "truncated": false}})]);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browse_search_caps_at_500() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let seeded = seed(&ts, &api).await;

        let mut response = api
            .get(&format!(
                "/api/v1/snapshots/{}/search?path={}&q=match-",
                seeded.snapshot,
                seeded.path("many")
            ))
            .await;
        assert_eq!(response.status(), 200);
        let first = response.chunk().await.unwrap().unwrap();
        let first = String::from_utf8(first.to_vec()).unwrap();
        assert!(first.starts_with("{\"match\""), "[{}]", backend.name());
        assert!(!first.contains("\"done\""));
        let mut text = first;
        while let Some(chunk) = response.chunk().await.unwrap() {
            text.push_str(std::str::from_utf8(&chunk).unwrap());
        }
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 501);
        assert_eq!(
            lines[500],
            json!({"done": {"count": 500, "truncated": true}})
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browse_search_client_drop_is_harmless() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let seeded = seed(&ts, &api).await;
        let path = seeded.path("many");

        let mut response = api
            .get(&format!(
                "/api/v1/snapshots/{}/search?path={path}&q=match-",
                seeded.snapshot
            ))
            .await;
        assert!(response.chunk().await.unwrap().is_some());
        drop(response);

        let again = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            search(&api, &seeded, &path, "match-"),
        )
        .await
        .expect("the next search answers in the usual time");
        assert_eq!(again.len(), 501, "[{}]", backend.name());
        assert_eq!(api.get("/healthz").await.status(), 200);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browse_search_validation() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let seeded = seed(&ts, &api).await;
        let status = |query: String| {
            let api = &api;
            let id = seeded.snapshot.clone();
            async move {
                api.get(&format!("/api/v1/snapshots/{id}/search?{query}"))
                    .await
                    .status()
                    .as_u16()
            }
        };

        assert_eq!(status("q=".into()).await, 422, "[{}]", backend.name());
        assert_eq!(status(String::new()).await, 422);
        assert_eq!(status(format!("q={}", "a".repeat(256))).await, 422);
        assert_eq!(status(format!("q={}", "a".repeat(255))).await, 200);
        assert_eq!(status("path=!!!&q=a".into()).await, 422);
        assert_eq!(
            status(format!("path={}&q=a", seeded.path("nowhere"))).await,
            404
        );
        let missing = api
            .get(&format!("/api/v1/snapshots/{}/search?q=a", "0".repeat(64)))
            .await;
        assert_eq!(missing.status(), 404);
        ts.stop().await;
    }
}

async fn summary(api: &Api, seeded: &Seeded, items: Value) -> reqwest::Response {
    api.post(
        &format!("/api/v1/snapshots/{}/summary", seeded.snapshot),
        &json!({ "items": items }),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browse_summary_counts_selection() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let seeded = seed(&ts, &api).await;

        let response = summary(
            &api,
            &seeded,
            json!([seeded.path("docs/notes.md"), seeded.path("docs/reports")]),
        )
        .await;
        assert_eq!(response.status(), 200, "[{}]", backend.name());
        let body: Value = response.json().await.unwrap();
        assert_eq!(body, json!({"files": 2, "dirs": 1, "bytes": 7}));

        let response = summary(
            &api,
            &seeded,
            json!([seeded.path("docs/report.pdf"), seeded.path("big/zeta")]),
        )
        .await;
        let body: Value = response.json().await.unwrap();
        assert_eq!(body, json!({"files": 2, "dirs": 2, "bytes": 18}));

        let whole = summary(&api, &seeded, json!([b64(b"/")])).await;
        let body: Value = whole.json().await.unwrap();
        assert_eq!(body["files"], 450 + 3 + 600 + 1 + 4 + 1);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browse_summary_validation() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let seeded = seed(&ts, &api).await;
        let status = |items: Value| {
            let (api, seeded) = (&api, &seeded);
            async move { summary(api, seeded, items).await.status().as_u16() }
        };

        assert_eq!(status(json!([])).await, 422, "[{}]", backend.name());
        assert_eq!(status(json!(["!!!"])).await, 422);
        assert_eq!(status(json!([b64(b"/x/../y")])).await, 422);
        assert_eq!(status(json!([b64(b"relative/path")])).await, 422);
        assert_eq!(status(json!(vec!["Lw"; 10_001])).await, 422);
        assert_eq!(status(json!([seeded.path("docs/missing.txt")])).await, 404);
        assert_eq!(
            status(json!([seeded.path("docs/notes.md/inner")])).await,
            404
        );

        let unknown = api
            .post(
                &format!("/api/v1/snapshots/{}/summary", "0".repeat(64)),
                &json!({ "items": [seeded.path("docs")] }),
            )
            .await;
        assert_eq!(unknown.status(), 404);
        ts.stop().await;
    }
}
