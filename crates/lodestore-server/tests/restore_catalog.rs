mod common;

use common::{Api, TestServer, backends};
use lodestore_agent::{EnrollArgs, NetOptions};
use serde_json::{Value, json};

async fn enroll(ts: &TestServer, api: &Api, repo: &str, hostname: &str, dir: &str) -> String {
    let token = api.join_token(repo, json!([])).await;
    lodestore_agent::enroll(EnrollArgs {
        token,
        paths: ts.agent_paths(dir),
        hostname: Some(hostname.to_string()),
        net: NetOptions::default(),
    })
    .await
    .expect("enroll");
    api.hosts()
        .await
        .iter()
        .filter(|h| h["hostname"] == hostname)
        .map(|h| h["id"].as_str().unwrap().to_string())
        .next_back()
        .expect("the enrolled host")
}

fn snapshot_id(n: u64) -> String {
    format!("{n:064x}")
}

async fn insert(ts: &TestServer, repo: &str, host: Option<&str>, hostname: &str, n: u64, ms: i64) {
    let stats = json!({
        "files_new": 0, "files_changed": 0, "files_unchanged": 0, "dirs": 0,
        "bytes_total": 0, "bytes_read": 0, "bytes_added": 0,
    });
    sqlx::query(
        "INSERT INTO snapshots (id, repository_id, host_id, hostname, time_ms, paths_json, \
         tags_json, incomplete, errors_total, warnings_total, stats_json, discovered_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, 0, 0, 0, $8, 0)",
    )
    .bind(snapshot_id(n))
    .bind(repo)
    .bind(host)
    .bind(hostname)
    .bind(ms)
    .bind(r#"["/data"]"#)
    .bind("[]")
    .bind(stats.to_string())
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
}

async fn page(api: &Api, host: &str, extra: &str) -> Value {
    let response = api
        .get(&format!("/api/v1/restore/snapshots?host={host}{extra}"))
        .await;
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

fn ids(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_catalog_snapshots_keyset_pages() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a", "a").await;
        for n in 1..=120 {
            insert(&ts, &repo, Some(&host), "host-a", n, 1_000 + n as i64 * 10).await;
        }

        let first = page(&api, &host, "").await;
        assert_eq!(first["total"], 120, "[{}]", backend.name());
        assert_eq!(first["items"].as_array().unwrap().len(), 50);
        assert_eq!(first["items"][0]["host_key"], host);
        assert_eq!(first["items"][0]["repository_id"], repo);
        let mut seen = ids(&first);
        assert_eq!(seen[0], snapshot_id(120));

        insert(&ts, &repo, Some(&host), "host-a", 500, 99_999).await;

        let second = page(
            &api,
            &host,
            &format!("&before={}", first["next"].as_str().unwrap()),
        )
        .await;
        assert_eq!(second["total"], 121);
        seen.extend(ids(&second));
        let third = page(
            &api,
            &host,
            &format!("&before={}", second["next"].as_str().unwrap()),
        )
        .await;
        seen.extend(ids(&third));
        assert!(third["next"].is_null());

        let expected: Vec<String> = (1..=120).rev().map(snapshot_id).collect();
        assert_eq!(seen, expected);

        let small = page(&api, &host, "&limit=1").await;
        assert_eq!(ids(&small), [snapshot_id(500)]);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_catalog_same_time_ordered_by_id() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        for n in [3, 1, 4, 2] {
            insert(&ts, &repo, None, "host-a", n, 5_000).await;
        }

        let host = "agentless:host-a";
        let first = page(&api, host, "&limit=2").await;
        let second = page(
            &api,
            host,
            &format!("&limit=2&before={}", first["next"].as_str().unwrap()),
        )
        .await;
        let mut all = ids(&first);
        all.extend(ids(&second));
        assert_eq!(all, [4, 3, 2, 1].map(snapshot_id), "[{}]", backend.name());
        assert!(second["next"].is_null());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_catalog_hosts_include_agentless_and_revoked() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let idle = enroll(&ts, &api, &repo, "host-idle", "idle").await;
        let gone = enroll(&ts, &api, &repo, "host-gone", "gone").await;
        insert(&ts, &repo, Some(&gone), "host-gone", 1, 100).await;
        insert(&ts, &repo, Some(&gone), "host-gone", 2, 300).await;
        sqlx::query("UPDATE agents SET status = 'revoked' WHERE id = $1")
            .bind(&gone)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        insert(&ts, &repo, None, "x", 3, 5).await;
        insert(&ts, &repo, None, "x", 4, 9).await;
        insert(&ts, &repo, None, "y", 5, 7).await;

        let response = api.get("/api/v1/restore/hosts").await;
        assert_eq!(response.status(), 200);
        let hosts: Vec<Value> = response.json().await.unwrap();
        assert_eq!(hosts.len(), 4, "[{}]", backend.name());
        let by_key = |key: &str| -> &Value {
            hosts
                .iter()
                .find(|h| h["key"] == key)
                .unwrap_or_else(|| panic!("no host {key}"))
        };

        let idle = by_key(&idle);
        assert_eq!(idle["agent"], true);
        assert_eq!(idle["status"], "offline");
        assert_eq!(idle["snapshot_count"], 0);
        assert!(idle["latest_snapshot_ms"].is_null());
        assert_eq!(idle["repository_name"], "main");

        let gone = by_key(&gone);
        assert_eq!(gone["status"], "revoked");
        assert_eq!(gone["snapshot_count"], 2);
        assert_eq!(gone["latest_snapshot_ms"], 300);

        let x = by_key("agentless:x");
        assert_eq!(x["agent"], false);
        assert!(x["status"].is_null());
        assert!(x["host_id"].is_null());
        assert_eq!(x["hostname"], "x");
        assert_eq!(x["snapshot_count"], 2);
        assert_eq!(x["latest_snapshot_ms"], 9);
        assert_eq!(x["repository_id"], repo);
        assert_eq!(by_key("agentless:y")["snapshot_count"], 1);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_catalog_snapshot_detail() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a", "a").await;
        insert(&ts, &repo, Some(&host), "host-a", 1, 100).await;
        insert(&ts, &repo, None, "host-b", 2, 200).await;

        let bound: Value = api
            .get(&format!("/api/v1/snapshots/{}", snapshot_id(1)))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(bound["id"], snapshot_id(1), "[{}]", backend.name());
        assert_eq!(bound["repository_id"], repo);
        assert_eq!(bound["host_key"], host);
        assert_eq!(bound["time"], 100);

        let loose: Value = api
            .get(&format!("/api/v1/snapshots/{}", snapshot_id(2)))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(loose["host_key"], "agentless:host-b");

        assert_eq!(
            api.get(&format!("/api/v1/snapshots/{}", snapshot_id(9)))
                .await
                .status(),
            404
        );
        assert_eq!(api.get("/api/v1/snapshots/nope").await.status(), 404);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_catalog_validation_errors() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a", "a").await;
        let status = |query: String| {
            let api = &api;
            async move {
                api.get(&format!("/api/v1/restore/snapshots{query}"))
                    .await
                    .status()
            }
        };

        assert_eq!(
            status("?host=unknown".into()).await,
            404,
            "[{}]",
            backend.name()
        );
        assert_eq!(status("?host=agentless:nobody".into()).await, 404);
        assert_eq!(status(String::new()).await, 422);
        assert_eq!(status(format!("?host={host}&limit=0")).await, 422);
        assert_eq!(status(format!("?host={host}&limit=201")).await, 422);
        assert_eq!(status(format!("?host={host}&limit=200")).await, 200);
        assert_eq!(status(format!("?host={host}&before=garbage")).await, 422);
        assert_eq!(status(format!("?host={host}&before=5.zz")).await, 422);
        assert_eq!(
            status(format!("?host={host}&before=x.{}", snapshot_id(1))).await,
            422
        );
        ts.stop().await;
    }
}
