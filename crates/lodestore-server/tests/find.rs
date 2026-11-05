mod common;

use std::path::Path;

use common::{Api, TestServer, WAIT, backends, bearer_client, wait_until};
use lodestore_format::index::hex32;
use lodestore_format::pb;
use lodestore_format::repo::Repository;
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, Writer};
use serde_json::{Value, json};

const PASSWORD: &[u8] = b"test-password";

fn write(path: &Path, data: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, data).unwrap();
}

async fn backup(
    writer: &mut Writer<&LocalFsStore>,
    data: &Path,
    host: &str,
    seconds: i64,
) -> String {
    let outcome = writer
        .backup(
            std::slice::from_ref(&data.to_path_buf()),
            BackupOptions {
                hostname: Some(host.as_bytes().to_vec()),
                time: Some(pb::Timestamp { seconds, nanos: 0 }),
                ..BackupOptions::default()
            },
            &|_| {},
        )
        .await
        .expect("backup");
    hex32(&outcome.snapshot_id)
}

async fn seed(ts: &TestServer, api: &Api) -> Vec<String> {
    let repo = ts.create_local_repository("main").await;
    let store = LocalFsStore::new(ts.local_repo_root("main"));
    let opened = Repository::open(&store, PASSWORD).await.unwrap();
    let keys = opened.read_keys().write_keys(opened.config_hash());
    let mut writer = Writer::new(&store, keys, opened.config()).await.unwrap();

    let data = ts.dir.path().join("tree");
    write(&data.join("etc/nginx/nginx.conf"), b"worker 1;");
    write(&data.join("etc/app.conf"), b"app");
    write(&data.join("etc/readme.txt"), b"not a config");
    let first = backup(&mut writer, &data, "host-a", 1_700_000_000).await;
    std::fs::remove_file(data.join("etc/app.conf")).unwrap();
    let second = backup(&mut writer, &data, "host-a", 1_700_000_100).await;

    write(&data.join("etc/nginx/nginx.conf"), b"worker 8; other");
    let third = backup(&mut writer, &data, "host-b", 1_700_000_200).await;

    let response = api
        .post(
            &format!("/api/v1/repositories/{repo}/catalog/sync"),
            &json!({}),
        )
        .await;
    assert_eq!(response.status(), 200);
    sqlx::query(
        "UPDATE snapshots SET received_at = time_ms, verified_at = time_ms, verified_by = 'server'",
    )
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
    vec![first, second, third]
}

async fn session(api: &Api, id: &str, after: Option<u64>) -> Value {
    let query = after.map(|a| format!("?after={a}")).unwrap_or_default();
    let response = api.get(&format!("/api/v1/find/{id}{query}")).await;
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_glob_across_hosts() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let snapshots = seed(&ts, &api).await;

        let response = api
            .post("/api/v1/find", &json!({ "pattern": "*.conf" }))
            .await;
        assert_eq!(response.status(), 202, "[{}]", backend.name());
        let started: Value = response.json().await.unwrap();
        let id = started["id"].as_str().unwrap().to_string();

        assert!(
            wait_until(WAIT, || async {
                session(&api, &id, None).await["state"] != "running"
            })
            .await,
            "the search never finished"
        );
        let found = session(&api, &id, None).await;
        assert_eq!(found["state"], "done", "{found}");
        assert_eq!(found["total"], 3);
        assert_eq!(found["scanned"], 3);
        assert_eq!(found["skipped"], 0);
        assert_eq!(found["files"], 3);
        assert_eq!(found["hosts"], 2);
        assert_eq!(found["searched_hosts"], 2);
        let groups = found["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 3);

        let of = |host: &str, name: &str| -> &Value {
            groups
                .iter()
                .find(|g| {
                    g["host_key"] == format!("agentless:{host}")
                        && g["path"].as_str().unwrap().ends_with(name)
                })
                .unwrap_or_else(|| panic!("no group for {host} {name}"))
        };
        let nginx_a = of("host-a", "nginx.conf");
        let versions = nginx_a["versions"].as_array().unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0]["snapshot_count"], 2);
        assert_eq!(versions[0]["snapshots"][0]["id"], snapshots[1].as_str());
        assert_eq!(nginx_a["agent"], false);
        assert_eq!(nginx_a["gone"], Value::Null);
        let app = of("host-a", "app.conf");
        assert_eq!(app["versions"].as_array().unwrap().len(), 1);
        assert_eq!(app["gone"]["last_ms"], 1_700_000_000_000i64);
        assert_eq!(app["gone"]["next_ms"], 1_700_000_100_000i64);
        let nginx_b = of("host-b", "nginx.conf");
        assert_ne!(
            nginx_b["versions"][0]["content_id"],
            versions[0]["content_id"]
        );

        let again = session(&api, &id, Some(found["revision"].as_u64().unwrap())).await;
        assert!(again["groups"].as_array().unwrap().is_empty());

        let response = api
            .post(
                &format!(
                    "/api/v1/snapshots/{}/summary",
                    versions[0]["snapshots"][0]["id"].as_str().unwrap()
                ),
                &json!({ "items": [nginx_a["path_b64"]] }),
            )
            .await;
        assert_eq!(response.status(), 200, "[{}]", backend.name());
        let summary: Value = response.json().await.unwrap();
        assert_eq!(summary["files"], 1);

        let index = nginx_a["index"].as_u64().unwrap();
        let group = api.get(&format!("/api/v1/find/{id}/groups/{index}")).await;
        assert_eq!(group.status(), 200);
        let group: Value = group.json().await.unwrap();
        assert_eq!(
            group["versions"][0]["snapshots"].as_array().unwrap().len(),
            2
        );
        assert_eq!(
            api.get(&format!("/api/v1/find/{id}/groups/99"))
                .await
                .status(),
            404
        );
        assert_eq!(api.get("/api/v1/find/nope").await.status(), 404);

        let client = reqwest::Client::new();
        let delete = |id: String| {
            let request = client
                .delete(format!("{}/api/v1/find/{id}", ts.rest_url()))
                .header("cookie", api.cookie())
                .header("x-csrf-token", &api.csrf);
            async move { request.send().await.unwrap().status() }
        };
        assert_eq!(delete(id.clone()).await, 204);
        assert_eq!(session(&api, &id, None).await["state"], "done");
        assert_eq!(delete("nope".into()).await, 404);
        ts.stop().await;
    }
}

async fn finished(api: &Api, pattern: &str) -> Value {
    let response = api
        .post("/api/v1/find", &json!({ "pattern": pattern }))
        .await;
    assert_eq!(response.status(), 202);
    let started: Value = response.json().await.unwrap();
    let id = started["id"].as_str().unwrap().to_string();
    assert!(
        wait_until(WAIT, || async {
            session(api, &id, None).await["state"] != "running"
        })
        .await,
        "the search never finished"
    );
    session(api, &id, None).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_stops_at_the_group_limit() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.find.max_matches = 2).await;
        let api = ts.api().await;
        seed(&ts, &api).await;
        let found = finished(&api, "*.conf").await;
        assert_eq!(found["state"], "incomplete", "{found}");
        assert_eq!(found["stop_reason"], "limit");
        assert_eq!(found["limit"], 2);
        assert_eq!(found["groups"].as_array().unwrap().len(), 2);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_stops_at_the_timeout() {
    for backend in backends() {
        let ts =
            TestServer::start_with(&backend, |c| c.find.timeout = std::time::Duration::ZERO).await;
        let api = ts.api().await;
        seed(&ts, &api).await;
        let found = finished(&api, "*.conf").await;
        assert_eq!(found["state"], "incomplete", "{found}");
        assert_eq!(found["stop_reason"], "timeout");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_requires_csrf() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let client = reqwest::Client::new();
        let base = ts.rest_url();
        for (method, path, body) in [
            ("POST", "/api/v1/find", json!({ "pattern": "*.conf" })),
            ("DELETE", "/api/v1/find/nope", json!({})),
        ] {
            let missing = client
                .request(method.parse().unwrap(), format!("{base}{path}"))
                .header("cookie", api.cookie())
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(
                missing.status(),
                403,
                "{method} {path} [{}]",
                backend.name()
            );
            let error: Value = missing.json().await.unwrap();
            assert_eq!(error["code"], "csrf");
        }
        ts.stop().await;
    }
}

async fn post_find(api: &Api, body: &Value) -> reqwest::Response {
    api.post("/api/v1/find", body).await
}

async fn finished_with(api: &Api, body: &Value) -> Value {
    let response = post_find(api, body).await;
    assert_eq!(response.status(), 202, "{body}");
    let started: Value = response.json().await.unwrap();
    let id = started["id"].as_str().unwrap().to_string();
    assert!(
        wait_until(WAIT, || async {
            session(api, &id, None).await["state"] != "running"
        })
        .await,
        "the search never finished"
    );
    session(api, &id, None).await
}

fn paths(found: &Value) -> Vec<(String, String)> {
    let mut paths: Vec<(String, String)> = found["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            (
                g["host_key"].as_str().unwrap().to_string(),
                g["path"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    paths.sort();
    paths
}

async fn enroll_host_a(ts: &TestServer, labels: &str) {
    let pool = &ts.state().db.pool;
    let repo: String = sqlx::query_scalar("SELECT repository_id FROM snapshots LIMIT 1")
        .fetch_one(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, hostname, repository_id, labels_json, os, arch, version, status, \
         enrolled_at, cert_serial, cert_issued_at, cert_not_after) \
         VALUES ('agent-a', 'host-a', $1, $2, 'linux', 'x86_64', 'test', 'active', 0, 'serial', 0, 0)",
    )
    .bind(repo)
    .bind(labels)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("UPDATE snapshots SET host_id = 'agent-a' WHERE hostname = 'host-a'")
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_filters_by_host_and_period() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        seed(&ts, &api).await;

        let only_a = finished_with(
            &api,
            &json!({ "pattern": "*.conf", "hosts": ["agentless:host-a"] }),
        )
        .await;
        assert_eq!(only_a["state"], "done", "{only_a}");
        assert_eq!(only_a["total"], 2);
        assert_eq!(only_a["hosts"], 1);
        assert_eq!(only_a["searched_hosts"], 1);
        assert_eq!(only_a["files"], 2);

        let late = finished_with(
            &api,
            &json!({ "pattern": "*.conf", "since": 1_700_000_050_000i64 }),
        )
        .await;
        assert_eq!(late["total"], 2);
        assert_eq!(
            paths(&late)
                .iter()
                .map(|(host, _)| host.as_str())
                .collect::<Vec<_>>(),
            ["agentless:host-a", "agentless:host-b"]
        );

        let middle = finished_with(
            &api,
            &json!({ "pattern": "*.conf", "since": 1_700_000_050_000i64, "until": 1_700_000_150_000i64 }),
        )
        .await;
        assert_eq!(middle["total"], 1);
        assert_eq!(middle["files"], 1);

        let both = finished_with(
            &api,
            &json!({ "pattern": "*.conf", "hosts": ["agentless:host-a", "agentless:host-b"] }),
        )
        .await;
        assert_eq!(both["total"], 3);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_filters_by_label() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        seed(&ts, &api).await;
        enroll_host_a(&ts, r#"[{"key":"role","value":"web"}]"#).await;

        let web = finished_with(
            &api,
            &json!({ "pattern": "*.conf", "labels": ["role=web"] }),
        )
        .await;
        assert_eq!(web["total"], 2, "{web}");
        assert!(paths(&web).iter().all(|(host, _)| host == "agent-a"));

        let db = finished_with(&api, &json!({ "pattern": "*.conf", "labels": ["role=db"] })).await;
        assert_eq!(db["state"], "done");
        assert_eq!(db["total"], 0);
        assert!(db["groups"].as_array().unwrap().is_empty());

        let by_agent =
            finished_with(&api, &json!({ "pattern": "*.conf", "hosts": ["agent-a"] })).await;
        assert_eq!(by_agent["total"], 2);

        let hosts_of = |found: &Value| -> Vec<String> {
            let mut keys: Vec<String> = paths(found).into_iter().map(|(host, _)| host).collect();
            keys.dedup();
            keys
        };

        let union = finished_with(
            &api,
            &json!({ "pattern": "*.conf", "hosts": ["agentless:host-b"], "labels": ["role=web"] }),
        )
        .await;
        assert_eq!(union["state"], "done", "{union}");
        assert_eq!(union["total"], 3, "{union}");
        assert_eq!(union["searched_hosts"], 2, "{union}");
        assert_eq!(hosts_of(&union), ["agent-a", "agentless:host-b"]);

        let host_only_match = finished_with(
            &api,
            &json!({ "pattern": "*.conf", "hosts": ["agentless:host-b"], "labels": ["role=db"] }),
        )
        .await;
        assert_eq!(host_only_match["total"], 1, "{host_only_match}");
        assert_eq!(hosts_of(&host_only_match), ["agentless:host-b"]);

        let both_labels = finished_with(
            &api,
            &json!({ "pattern": "*.conf", "labels": ["role=web", "env=prod"] }),
        )
        .await;
        assert_eq!(both_labels["total"], 0, "{both_labels}");

        let narrowed = finished_with(
            &api,
            &json!({
                "pattern": "*.conf",
                "hosts": ["agentless:host-b"],
                "labels": ["role=web"],
                "since": 1_700_000_050_000i64
            }),
        )
        .await;
        assert_eq!(narrowed["total"], 2, "{narrowed}");
        assert_eq!(hosts_of(&narrowed), ["agent-a", "agentless:host-b"]);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_rejects_bad_filters() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        seed(&ts, &api).await;
        for (body, field) in [
            (json!({ "pattern": "x", "since": 20, "until": 10 }), "since"),
            (json!({ "pattern": "x", "labels": ["role"] }), "labels"),
            (json!({ "pattern": "x", "labels": ["=web"] }), "labels"),
            (json!({ "pattern": "x", "hosts": ["nobody"] }), "hosts"),
            (
                json!({ "pattern": "x", "hosts": ["agentless:nobody"] }),
                "hosts",
            ),
            (
                json!({ "pattern": "x", "hosts": (0..501).map(|n| format!("h{n}")).collect::<Vec<_>>() }),
                "hosts",
            ),
            (
                json!({ "pattern": "x", "labels": vec!["role=web"; 21] }),
                "labels",
            ),
        ] {
            let response = post_find(&api, &body).await;
            assert_eq!(response.status(), 422, "{body} [{}]", backend.name());
            let error: Value = response.json().await.unwrap();
            assert_eq!(error["errors"][0]["path"], field, "{body}");
        }
        let deduped = finished_with(
            &api,
            &json!({ "pattern": "*.conf", "hosts": vec!["agentless:host-a"; 500] }),
        )
        .await;
        assert_eq!(deduped["total"], 2, "{deduped}");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_gone_follows_the_source_of_the_file() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let store = LocalFsStore::new(ts.local_repo_root("main"));
        let opened = Repository::open(&store, PASSWORD).await.unwrap();
        let keys = opened.read_keys().write_keys(opened.config_hash());
        let mut writer = Writer::new(&store, keys, opened.config()).await.unwrap();

        let etc = ts.dir.path().join("etc");
        let var = ts.dir.path().join("var");
        write(&etc.join("nginx.conf"), b"worker 1;");
        write(&var.join("data.bin"), b"data");
        backup(&mut writer, &etc, "host-c", 1_700_000_000).await;
        backup(&mut writer, &var, "host-c", 1_700_000_100).await;
        std::fs::remove_file(etc.join("nginx.conf")).unwrap();
        let partial = backup(&mut writer, &etc, "host-c", 1_700_000_200).await;
        let response = api
            .post(
                &format!("/api/v1/repositories/{repo}/catalog/sync"),
                &json!({}),
            )
            .await;
        assert_eq!(response.status(), 200);
        sqlx::query("UPDATE snapshots SET incomplete = 1 WHERE id = $1")
            .bind(&partial)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();

        let found = finished(&api, "nginx.conf").await;
        assert_eq!(found["files"], 1, "{found}");
        assert_eq!(
            found["groups"][0]["gone"],
            Value::Null,
            "[{}]",
            backend.name()
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_handles_empty_cases() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let none = finished(&api, "*.conf").await;
        assert_eq!(none["state"], "done", "{none}");
        assert_eq!(none["total"], 0);
        assert!(none["groups"].as_array().unwrap().is_empty());

        seed(&ts, &api).await;
        let nothing = finished(&api, "*.nothing").await;
        assert_eq!(nothing["state"], "done");
        assert_eq!(nothing["total"], 3);
        assert_eq!(nothing["searched_hosts"], 2);
        assert_eq!(nothing["hosts"], 0);
        assert!(nothing["groups"].as_array().unwrap().is_empty());
        ts.stop().await;
    }
}

fn find_object(dir: &Path, id: &str) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(found) = find_object(&path, id) {
                return Some(found);
            }
        } else if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().contains(id))
            && path.parent().is_some_and(|p| p.ends_with("snapshots"))
        {
            return Some(path);
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_skips_a_vanished_snapshot() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let snapshots = seed(&ts, &api).await;
        let object = find_object(&ts.local_repo_root("main"), &snapshots[2])
            .expect("the snapshot object of host-b");
        std::fs::remove_file(object).unwrap();

        let found = finished(&api, "*.conf").await;
        assert_eq!(found["state"], "done", "{found}");
        assert_eq!(found["total"], 3);
        assert_eq!(found["skipped"], 1);
        assert_eq!(found["files"], 2);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_cancel_codes() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        seed(&ts, &api).await;
        let client = reqwest::Client::new();
        let delete = |id: String| {
            let request = client
                .delete(format!("{}/api/v1/find/{id}", ts.rest_url()))
                .header("cookie", api.cookie())
                .header("x-csrf-token", &api.csrf);
            async move { request.send().await.unwrap().status() }
        };
        assert_eq!(delete("nope".into()).await, 404);

        let started: Value = post_find(&api, &json!({ "pattern": "*.conf" }))
            .await
            .json()
            .await
            .unwrap();
        let id = started["id"].as_str().unwrap().to_string();
        assert_eq!(delete(id.clone()).await, 204);
        assert!(
            wait_until(WAIT, || async {
                session(&api, &id, None).await["state"] != "running"
            })
            .await,
            "the search kept running after DELETE"
        );
        let state = session(&api, &id, None).await["state"].clone();
        assert!(state == "cancelled" || state == "done", "{state}");
        assert_eq!(delete(id).await, 204);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_needs_the_snapshots_read_scope() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let base = ts.rest_url();
        let body = json!({ "pattern": "*.conf" });

        let anonymous = reqwest::Client::new();
        let denied = anonymous
            .post(format!("{base}/api/v1/find"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), 401, "[{}]", backend.name());
        let denied = anonymous
            .get(format!("{base}/api/v1/find/nope"))
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), 401);

        let other = bearer_client(&ts.issue_token(&["hosts:read"]).await);
        let denied = other
            .post(format!("{base}/api/v1/find"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), 403);
        let denied = other
            .get(format!("{base}/api/v1/find/nope"))
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), 403);

        let reader = bearer_client(&ts.issue_token(&["snapshots:read"]).await);
        let accepted = reader
            .post(format!("{base}/api/v1/find"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(accepted.status(), 202);
        ts.stop().await;
    }
}

fn cache_dir(ts: &TestServer) -> std::path::PathBuf {
    ts.config.data_dir.join("find-cache").join("v1")
}

fn cache_files(ts: &TestServer, suffix: &str) -> Vec<std::path::PathBuf> {
    let mut found: Vec<_> = std::fs::read_dir(cache_dir(ts))
        .map(|read| {
            read.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.to_string_lossy().contains(suffix))
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found
}

fn digest(found: &Value) -> Vec<(String, String, Vec<String>)> {
    let mut groups: Vec<_> = found["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            let mut ids: Vec<String> = g["versions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| format!("{}x{}", v["content_id"], v["snapshot_count"]))
                .collect();
            ids.sort();
            (
                g["host_key"].as_str().unwrap().to_string(),
                g["path"].as_str().unwrap().to_string(),
                ids,
            )
        })
        .collect();
    groups.sort();
    groups
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_cache_serves_repeat_search() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        seed(&ts, &api).await;
        let first = finished(&api, "*.conf").await;
        assert_eq!(first["files"], 3, "{first}");
        assert_eq!(cache_files(&ts, ".bin").len(), 3);
        assert!(cache_files(&ts, ".tmp-").is_empty());

        let away = ts.dir.path().join("repo-away");
        std::fs::rename(ts.local_repo_root("main"), &away).unwrap();
        let second = finished(&api, "*.conf").await;
        assert_eq!(second["state"], "done", "{second}");
        assert_eq!(second["skipped"], 0, "{second}");
        assert_eq!(digest(&second), digest(&first));

        let readme = finished(&api, "readme.txt").await;
        assert_eq!(readme["skipped"], 0);
        assert_eq!(readme["files"], 2);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_cache_corrupt_is_miss() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        seed(&ts, &api).await;
        let first = finished(&api, "*.conf").await;

        let mut broken = Vec::new();
        for path in cache_files(&ts, ".bin") {
            let mut bytes = std::fs::read(&path).unwrap();
            let last = bytes.len() - 1;
            bytes[last] ^= 1;
            std::fs::write(&path, &bytes).unwrap();
            broken.push((path, bytes));
        }
        let second = finished(&api, "*.conf").await;
        assert_eq!(second["skipped"], 0, "{second}");
        assert_eq!(digest(&second), digest(&first));
        for (path, bytes) in &broken {
            assert_ne!(&std::fs::read(path).unwrap(), bytes, "rewritten");
        }

        let away = ts.dir.path().join("repo-away");
        std::fs::rename(ts.local_repo_root("main"), &away).unwrap();
        let third = finished(&api, "*.conf").await;
        assert_eq!(third["skipped"], 0, "{third}");
        assert_eq!(digest(&third), digest(&first));
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_cache_cancel_writes_nothing() {
    for backend in backends() {
        let ts =
            TestServer::start_with(&backend, |c| c.find.timeout = std::time::Duration::ZERO).await;
        let api = ts.api().await;
        seed(&ts, &api).await;
        let stopped = finished(&api, "*.conf").await;
        assert_eq!(stopped["state"], "incomplete", "{stopped}");
        assert!(cache_files(&ts, ".bin").is_empty());
        assert!(cache_files(&ts, ".tmp-").is_empty());
        ts.stop().await;

        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let snapshots = seed(&ts, &api).await;
        let started: Value = post_find(&api, &json!({ "pattern": "*.conf" }))
            .await
            .json()
            .await
            .unwrap();
        let id = started["id"].as_str().unwrap().to_string();
        let cancelled = reqwest::Client::new()
            .delete(format!("{}/api/v1/find/{id}", ts.rest_url()))
            .header("cookie", api.cookie())
            .header("x-csrf-token", &api.csrf)
            .send()
            .await
            .unwrap();
        assert_eq!(cancelled.status(), 204);
        assert!(
            wait_until(WAIT, || async {
                session(&api, &id, None).await["state"] != "running"
            })
            .await
        );
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(cache_files(&ts, ".tmp-").is_empty());
        for path in cache_files(&ts, ".bin") {
            let name = path.file_stem().unwrap().to_string_lossy().to_string();
            assert!(snapshots.contains(&name), "{name}");
        }
        ts.stop().await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_cache_removed_with_snapshot() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let snapshots = seed(&ts, &api).await;
        finished(&api, "*.conf").await;
        let file = |id: &str| cache_dir(&ts).join(format!("{id}.bin"));
        assert!(file(&snapshots[1]).exists());

        let deleter = bearer_client(&ts.issue_token(&["snapshots:delete"]).await);
        let response = deleter
            .delete(format!(
                "{}/api/v1/snapshots/{}",
                ts.rest_url(),
                snapshots[1]
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "[{}]", backend.name());
        assert!(!file(&snapshots[1]).exists());
        assert!(file(&snapshots[0]).exists() && file(&snapshots[2]).exists());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_cache_skips_a_snapshot_forgotten_during_the_walk() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let snapshots = seed(&ts, &api).await;
        let pool = &ts.state().db.pool;
        let repo: String = sqlx::query_scalar("SELECT repository_id FROM snapshots LIMIT 1")
            .fetch_one(pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM snapshots WHERE id = $1")
            .bind(&snapshots[0])
            .execute(pool)
            .await
            .unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let entries = lodestore_server::find::walk::snapshot_entries(
            ts.state(),
            &repo,
            &snapshots[0],
            &cancel,
        )
        .await
        .unwrap();
        assert!(!entries.is_empty());
        assert!(cache_files(&ts, ".bin").is_empty(), "[{}]", backend.name());
        lodestore_server::find::walk::snapshot_entries(ts.state(), &repo, &snapshots[1], &cancel)
            .await
            .unwrap();
        assert_eq!(cache_files(&ts, ".bin").len(), 1);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_cache_keeps_no_list_over_the_budget() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.find.cache_max_bytes = 1).await;
        let api = ts.api().await;
        seed(&ts, &api).await;
        let found = finished(&api, "*.conf").await;
        assert_eq!(found["files"], 3, "{found}");
        assert!(cache_files(&ts, ".bin").is_empty(), "[{}]", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_cache_evicts_oldest() {
    let only_a = json!({ "pattern": "*.conf", "hosts": ["agentless:host-a"] });
    for backend in backends() {
        let probe = TestServer::start(&backend).await;
        let api = probe.api().await;
        seed(&probe, &api).await;
        finished_with(&api, &only_a).await;
        let sizes: Vec<u64> = cache_files(&probe, ".bin")
            .iter()
            .map(|p| std::fs::metadata(p).unwrap().len())
            .collect();
        assert_eq!(sizes.len(), 2);
        let biggest = *sizes.iter().max().unwrap();
        probe.stop().await;

        let cap = biggest + biggest / 4;
        let ts = TestServer::start_with(&backend, |c| c.find.cache_max_bytes = cap).await;
        let api = ts.api().await;
        seed(&ts, &api).await;
        let found = finished_with(&api, &only_a).await;
        assert_eq!(found["state"], "done", "{found}");
        assert_eq!(cache_files(&ts, ".bin").len(), 1, "[{}]", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_orders_by_server_time_and_marks_unsigned_versions() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        seed(&ts, &api).await;

        let store = LocalFsStore::new(ts.local_repo_root("main"));
        let opened = Repository::open(&store, PASSWORD).await.unwrap();
        let keys = opened.read_keys().write_keys(opened.config_hash());
        let mut writer = Writer::new(&store, keys, opened.config()).await.unwrap();
        let data = ts.dir.path().join("tree");
        write(&data.join("etc/nginx/nginx.conf"), b"forged by the host");
        let forged = backup(&mut writer, &data, "host-a", 253_402_300_799).await;
        let repo: String = sqlx::query_scalar("SELECT repository_id FROM snapshots LIMIT 1")
            .fetch_one(&ts.state().db.pool)
            .await
            .unwrap();
        let response = api
            .post(
                &format!("/api/v1/repositories/{repo}/catalog/sync"),
                &json!({}),
            )
            .await;
        assert_eq!(response.status(), 200);
        sqlx::query("UPDATE snapshots SET received_at = $1 WHERE id = $2")
            .bind(1_700_000_300_000i64)
            .bind(&forged)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();

        let found = finished_with(
            &api,
            &json!({ "pattern": "nginx.conf", "hosts": ["agentless:host-a"] }),
        )
        .await;
        let group = &found["groups"][0];
        assert_eq!(group["last_seen_ms"], 1_700_000_300_000i64);
        let versions = group["versions"].as_array().unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0]["verified_by"], "server");
        assert_eq!(versions[0]["last_ms"], 1_700_000_100_000i64);
        assert_eq!(versions[1]["verified_by"], Value::Null);
        assert_eq!(versions[1]["snapshots"][0]["id"], forged.as_str());
        assert_eq!(versions[1]["snapshots"][0]["time_ms"], 1_700_000_300_000i64);
        ts.stop().await;
    }
}
