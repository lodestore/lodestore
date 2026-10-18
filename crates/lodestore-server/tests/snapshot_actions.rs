mod common;

use std::collections::BTreeMap;

use common::{AgentEnv, Api, TestServer, backends, bearer_client};
use lodestore_format::crypto::Entropy;
use lodestore_format::keys::KdfCost;
use lodestore_format::repo::{InitOptions, PendingInit};
use lodestore_format::store::{BlobStore, RepoLocation, StaticCredentials};
use lodestore_server::catalog::retention::plan_policy;
use lodestore_server::repos::db::{AccessKey, NewRepository, RepoLocation as DbLocation};
use lodestore_testkit::minio::MinioServer;
use reqwest::Method;
use serde_json::{Value, json};
use sqlx::Row;

const STATS: &str = r#"{"files_new":0,"files_changed":0,"files_unchanged":0,"dirs":0,"bytes_total":0,"bytes_read":0,"bytes_added":0}"#;

fn snapshot_id(n: u64) -> String {
    format!("{n:064x}")
}

async fn insert_snapshot(
    ts: &TestServer,
    n: u64,
    host: Option<&str>,
    hostname: &str,
    policy: Option<&str>,
    received: i64,
) -> String {
    let id = snapshot_id(n);
    sqlx::query(
        "INSERT INTO snapshots (id, repository_id, host_id, hostname, time_ms, paths_json, \
         tags_json, policy_name, job_id, incomplete, errors_total, warnings_total, stats_json, \
         discovered_at, received_at, received_exact) \
         SELECT $1, id, $2, $3, $4, '[]', '[]', $5, 'seeded', 0, 0, 0, $6, $4, $4, 1 \
         FROM repositories WHERE name = 'main'",
    )
    .bind(&id)
    .bind(host)
    .bind(hostname)
    .bind(received)
    .bind(policy)
    .bind(STATS)
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
    id
}

async fn session(
    ts: &TestServer,
    api: &Api,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .request(method, format!("{}{path}", ts.rest_url()))
        .header("cookie", api.cookie())
        .header("x-csrf-token", &api.csrf);
    if let Some(body) = body {
        request = request.json(body);
    }
    request.send().await.unwrap()
}

async fn restore_items(api: &Api, host: &str) -> Vec<Value> {
    let response = api
        .get(&format!("/api/v1/restore/snapshots?host={host}"))
        .await;
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    body["items"].as_array().unwrap().clone()
}

async fn simple_server(backend: &common::Backend) -> TestServer {
    let ts = TestServer::start(backend).await;
    ts.create_local_repository("main").await;
    ts
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_actions_mark_and_clear() {
    for backend in backends() {
        let ts = simple_server(&backend).await;
        let api = ts.api().await;
        let id = insert_snapshot(&ts, 1, None, "h", None, 1_000).await;
        let path = format!("/api/v1/snapshots/{id}/suspicious");

        let response = session(
            &ts,
            &api,
            Method::PUT,
            &path,
            Some(&json!({ "suspicious": true, "reason": "  Looks wrong  " })),
        )
        .await;
        assert_eq!(response.status(), 200, "{}", backend.name());
        let marked: Value = response.json().await.unwrap();
        assert_eq!(marked["suspicious"]["note"], "Looks wrong");

        let listed = restore_items(&api, "agentless:h").await;
        let suspicious = &listed[0]["suspicious"];
        assert_eq!(suspicious["note"], "Looks wrong");
        assert_eq!(suspicious["marked_by"], "admin");
        assert_eq!(suspicious["automatic"], false);
        assert_eq!(suspicious["reasons"], json!([]));
        assert!(suspicious["marked_at"].as_i64().unwrap() > 0);

        let response = session(
            &ts,
            &api,
            Method::PUT,
            &path,
            Some(&json!({ "suspicious": false, "reason": "Verified by hand" })),
        )
        .await;
        assert_eq!(response.status(), 200);
        let cleared: Value = response.json().await.unwrap();
        assert_eq!(cleared["suspicious"], Value::Null);
        assert_eq!(
            restore_items(&api, "agentless:h").await[0]["suspicious"],
            Value::Null
        );
        let row = sqlx::query(
            "SELECT suspicious, suspicious_cleared_by, suspicious_cleared_reason, \
             suspicious_cleared_at FROM snapshots WHERE id = $1",
        )
        .bind(&id)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap();
        assert_eq!(row.try_get::<i64, _>("suspicious").unwrap(), 0);
        assert_eq!(
            row.try_get::<Option<String>, _>("suspicious_cleared_reason")
                .unwrap()
                .as_deref(),
            Some("Verified by hand")
        );
        assert_eq!(
            row.try_get::<Option<String>, _>("suspicious_cleared_by")
                .unwrap()
                .as_deref(),
            Some("admin")
        );
        assert!(
            row.try_get::<Option<i64>, _>("suspicious_cleared_at")
                .unwrap()
                .is_some()
        );

        let unknown = session(
            &ts,
            &api,
            Method::PUT,
            &format!("/api/v1/snapshots/{}/suspicious", snapshot_id(99)),
            Some(&json!({ "suspicious": true, "reason": "x" })),
        )
        .await;
        assert_eq!(unknown.status(), 404);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_actions_need_delete_scope() {
    for backend in backends() {
        let ts = simple_server(&backend).await;
        let id = insert_snapshot(&ts, 1, None, "h", None, 1_000).await;
        let url = format!("{}/api/v1/snapshots/{id}/suspicious", ts.rest_url());
        let body = json!({ "suspicious": true, "reason": "Odd" });

        let reader = bearer_client(&ts.issue_token(&["snapshots:read"]).await);
        let denied = reader.put(&url).json(&body).send().await.unwrap();
        assert_eq!(denied.status(), 403, "{}", backend.name());
        let denied: Value = denied.json().await.unwrap();
        assert_eq!(
            denied["message"],
            "The token needs the snapshots:delete scope."
        );

        let deleter = bearer_client(&ts.issue_token(&["snapshots:delete"]).await);
        let allowed = deleter.put(&url).json(&body).send().await.unwrap();
        assert_eq!(allowed.status(), 200);
        let allowed: Value = allowed.json().await.unwrap();
        assert_eq!(allowed["suspicious"]["marked_by"], "token:test");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_actions_reason_required() {
    for backend in backends() {
        let ts = simple_server(&backend).await;
        let api = ts.api().await;
        let id = insert_snapshot(&ts, 1, None, "h", None, 1_000).await;
        let path = format!("/api/v1/snapshots/{id}/suspicious");
        let long = "x".repeat(201);
        for reason in ["", "   ", long.as_str()] {
            for suspicious in [true, false] {
                let response = session(
                    &ts,
                    &api,
                    Method::PUT,
                    &path,
                    Some(&json!({ "suspicious": suspicious, "reason": reason })),
                )
                .await;
                assert_eq!(response.status(), 422, "{}", backend.name());
                let body: Value = response.json().await.unwrap();
                assert_eq!(body["message"], "Enter a reason.");
            }
        }
        let ok = session(
            &ts,
            &api,
            Method::PUT,
            &path,
            Some(&json!({ "suspicious": true, "reason": "x".repeat(200) })),
        )
        .await;
        assert_eq!(ok.status(), 200);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_policy_deleted_flag() {
    for backend in backends() {
        let ts = simple_server(&backend).await;
        let api = ts.api().await;
        let yaml = "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: live\n\
                    spec:\n  repository: main\n  paths:\n    - /srv\n";
        let created = api.post("/api/v1/policies", &json!({ "yaml": yaml })).await;
        assert_eq!(created.status(), 201);
        insert_snapshot(&ts, 1, None, "h", Some("live"), 3_000).await;
        insert_snapshot(&ts, 2, None, "h", Some("gone"), 2_000).await;
        insert_snapshot(&ts, 3, None, "h", None, 1_000).await;

        let items = restore_items(&api, "agentless:h").await;
        let flags: Vec<(String, bool)> = items
            .iter()
            .map(|i| {
                (
                    i["policy_name"].as_str().unwrap_or("-").to_string(),
                    i["policy_deleted"].as_bool().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            flags,
            vec![
                ("live".to_string(), false),
                ("gone".to_string(), true),
                ("-".to_string(), false),
            ],
            "{}",
            backend.name()
        );
        assert_eq!(items[0]["received_at"], 3_000);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_list_host_with_token() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let ts = env.ts();
        insert_snapshot(ts, 1, Some(&env.host), &env.hostname, None, 1_000).await;
        let url = format!("{}/api/v1/hosts/{}/snapshots", ts.rest_url(), env.host);

        let reader = bearer_client(&ts.issue_token(&["snapshots:read"]).await);
        let listed = reader.get(&url).send().await.unwrap();
        assert_eq!(listed.status(), 200, "{}", backend.name());
        let page: Value = listed.json().await.unwrap();
        assert_eq!(page["total"], 1);
        assert_eq!(page["items"][0]["received_at"], 1_000);
        assert_eq!(page["items"][0]["policy_deleted"], false);

        let other = bearer_client(&ts.issue_token(&["policies:read"]).await);
        assert_eq!(other.get(&url).send().await.unwrap().status(), 403);
        let path = format!("/api/v1/hosts/{}/snapshots", env.host);
        assert_eq!(env.api.get(&path).await.status(), 200);
        env.stop().await;
    }
}

const BUCKET: &str = "repo";

async fn add_policy(env: &AgentEnv, name: &str) {
    let yaml = format!(
        "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: {name}\n\
         spec:\n  repository: main\n  selector:\n    matchLabels:\n      env: e2e\n  \
         paths:\n    - /srv\n"
    );
    let response = env
        .api
        .post("/api/v1/policies", &json!({ "yaml": yaml }))
        .await;
    assert_eq!(response.status(), 201);
}

async fn preview(env: &AgentEnv, policy: &str, body: &Value) -> reqwest::Response {
    env.api
        .post(
            &format!("/api/v1/policies/{policy}/retention-preview"),
            body,
        )
        .await
}

fn manifest_exists(env: &AgentEnv, id: &str) -> bool {
    env.ts()
        .local_repo_root("main")
        .join("snapshots")
        .join(id)
        .exists()
}

async fn row_count(ts: &TestServer, table: &'static str, id: &str) -> i64 {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) AS n FROM {table} WHERE id = $1"
    )))
    .bind(id)
    .fetch_one(&ts.state().db.pool)
    .await
    .unwrap()
    .try_get("n")
    .unwrap()
}

async fn fill(env: &AgentEnv, host: &str, hostname: &str, first: u64, count: u64, base: i64) {
    for n in first..first + count {
        let received = base + (n - first) as i64;
        insert_snapshot(env.ts(), n, Some(host), hostname, Some("p"), received).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_actions_delete_removes_manifest() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let data = env.work.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("file"), b"to be deleted").unwrap();
        let id = env.backup(&[&data]).await;
        assert!(manifest_exists(&env, &id));
        let path = format!("/api/v1/snapshots/{id}");

        let response = session(env.ts(), &env.api, Method::DELETE, &path, None).await;

        assert_eq!(response.status(), 200, "{}", backend.name());
        let body: Value = response.json().await.unwrap();
        assert_eq!(
            body,
            json!({ "id": id, "storage_kept": false, "reason": null })
        );
        assert!(!manifest_exists(&env, &id));
        assert_eq!(row_count(env.ts(), "snapshots", &id).await, 0);
        lodestore_server::catalog::sync_repository(env.ts().state(), &env.repo)
            .await
            .unwrap();
        assert_eq!(row_count(env.ts(), "snapshots", &id).await, 0);
        let again = session(env.ts(), &env.api, Method::DELETE, &path, None).await;
        assert_eq!(again.status(), 404);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_actions_delete_needs_scope() {
    for backend in backends() {
        let ts = simple_server(&backend).await;
        let id = insert_snapshot(&ts, 1, None, "h", None, 1_000).await;
        let url = format!("{}/api/v1/snapshots/{id}", ts.rest_url());

        let reader = bearer_client(&ts.issue_token(&["snapshots:read"]).await);
        let denied = reader.delete(&url).send().await.unwrap();
        assert_eq!(denied.status(), 403, "{}", backend.name());
        assert_eq!(row_count(&ts, "snapshots", &id).await, 1);

        let deleter = bearer_client(&ts.issue_token(&["snapshots:delete"]).await);
        let allowed = deleter.delete(&url).send().await.unwrap();
        assert_eq!(allowed.status(), 200);
        assert_eq!(row_count(&ts, "snapshots", &id).await, 0);
        ts.stop().await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_actions_delete_denied_keeps_out() {
    let minio = MinioServer::start().await;
    minio.create_bucket(BUCKET).await;
    let key = minio
        .create_user_with_policy(BUCKET, &no_delete_policy().to_string())
        .await;
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
        access_key_id: key.0.clone(),
        secret_access_key: key.1.clone(),
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
        .commit(&store, b"test-password", &mut entropy)
        .await
        .unwrap();
    let ts = TestServer::start(&backends().remove(0)).await;
    lodestore_server::repos::db::create_repository(
        ts.state(),
        NewRepository {
            name: "main".to_string(),
            location: DbLocation {
                backend: "s3".to_string(),
                options,
            },
            experimental: false,
            format_repo_id,
            config_hash,
            master,
            server_slot_secret: None,
            server_creds: Some(AccessKey {
                access_key_id: key.0,
                secret_access_key: key.1,
            }),
            agent_creds: None,
        },
    )
    .await
    .unwrap();
    let api = ts.api().await;
    let id = insert_snapshot(&ts, 1, None, "h", None, 1_000).await;
    insert_snapshot(&ts, 2, None, "h", None, 2_000).await;
    store
        .put_if_absent(&format!("snapshots/{id}"), b"manifest".to_vec().into())
        .await
        .unwrap();

    let response = session(
        &ts,
        &api,
        Method::DELETE,
        &format!("/api/v1/snapshots/{id}"),
        None,
    )
    .await;

    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["storage_kept"], true, "{body}");
    assert!(!body["reason"].as_str().unwrap().is_empty());
    assert_eq!(row_count(&ts, "snapshots", &id).await, 0);
    assert_eq!(row_count(&ts, "snapshot_tombstones", &id).await, 1);
    let left = restore_items(&api, "agentless:h").await;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0]["id"], snapshot_id(2));
    assert!(store.get(&format!("snapshots/{id}")).await.is_ok());
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_preview_lists_forget() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let other = AgentEnv::start_on(env.ts(), &env.repo, "main", "host-two").await;
        add_policy(&env, "p").await;
        fill(&env, &env.host, &env.hostname, 0, 5, 1_000).await;
        fill(&env, &other.host, "host-two", 10, 5, 1_000).await;

        let response = preview(&env, "p", &json!({ "retention": { "keepLast": 1 } })).await;

        assert_eq!(response.status(), 200, "{}", backend.name());
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["total"], 10);
        assert_eq!(body["forget"], 6);
        assert_eq!(body["floor"], 2);
        assert_eq!(body["suspicious"], 0);
        assert_eq!(body["truncated"], 0);
        let hosts = body["hosts"].as_array().unwrap();
        assert_eq!(hosts.len(), 2);
        for host in hosts {
            assert_eq!(host["total"], 5);
            assert_eq!(host["forget"], 3);
            assert_eq!(host["snapshots"].as_array().unwrap().len(), 3);
        }
        let mine = hosts
            .iter()
            .find(|h| h["host_id"] == env.host.as_str())
            .unwrap();
        let ids: Vec<&str> = mine["snapshots"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, [snapshot_id(2), snapshot_id(1), snapshot_id(0)]);
        assert_eq!(mine["snapshots"][0]["received_at"], 1_002);

        let row = sqlx::query("SELECT spec_json FROM policies WHERE name = 'p'")
            .fetch_one(&env.ts().state().db.pool)
            .await
            .unwrap();
        let mut policy: lodestore_policy::BackupPolicy =
            serde_json::from_str(&row.try_get::<String, _>("spec_json").unwrap()).unwrap();
        let rules = lodestore_policy::Retention {
            keep_last: Some(1),
            ..Default::default()
        };
        policy.spec.retention = Some(rules.clone());
        let plan = plan_policy(env.ts().state(), &policy, &rules)
            .await
            .unwrap();
        let planned: usize = plan
            .hosts
            .iter()
            .map(|h| h.items.iter().filter(|i| !i.keep).count())
            .sum();
        assert_eq!(planned as u64, body["forget"]);
        other.stop().await;
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_preview_counts_suspicious() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        add_policy(&env, "p").await;
        fill(&env, &env.host, &env.hostname, 0, 5, 1_000).await;
        sqlx::query("UPDATE snapshots SET suspicious = 1, suspicious_by = 'anomaly' WHERE id = $1")
            .bind(snapshot_id(0))
            .execute(&env.ts().state().db.pool)
            .await
            .unwrap();

        let response = preview(&env, "p", &json!({ "retention": { "keepLast": 1 } })).await;

        let body: Value = response.json().await.unwrap();
        assert_eq!(body["suspicious"], 1, "{}", backend.name());
        assert_eq!(body["total"], 5);
        assert_eq!(body["forget"], 2);
        let listed: Vec<&str> = body["hosts"][0]["snapshots"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(listed, [snapshot_id(2), snapshot_id(1)]);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_preview_caps_list() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        add_policy(&env, "p").await;
        fill(&env, &env.host, &env.hostname, 0, 302, 1_000).await;

        let response = preview(&env, "p", &json!({ "retention": { "keepLast": 1 } })).await;

        let body: Value = response.json().await.unwrap();
        assert_eq!(body["total"], 302, "{}", backend.name());
        assert_eq!(body["forget"], 300);
        assert_eq!(body["truncated"], 100);
        assert_eq!(body["hosts"][0]["forget"], 300);
        assert_eq!(body["hosts"][0]["snapshots"].as_array().unwrap().len(), 200);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_preview_validates_rules() {
    for backend in backends() {
        let ts = simple_server(&backend).await;
        let api = ts.api().await;
        let yaml = "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: p\n\
                    spec:\n  repository: main\n  paths:\n    - /srv\n";
        let created = api.post("/api/v1/policies", &json!({ "yaml": yaml })).await;
        assert_eq!(created.status(), 201);
        let path = "/api/v1/policies/p/retention-preview";

        let zero = api
            .post(path, &json!({ "retention": { "keepLast": 0 } }))
            .await;
        assert_eq!(zero.status(), 422, "{}", backend.name());
        let zero: Value = zero.json().await.unwrap();
        assert_eq!(zero["message"], "Enter a whole number of 1 or more.");

        let zone = json!({ "retention": { "keepDaily": 7 }, "timezone": "Europe/Kyiv" });
        assert_eq!(api.post(path, &zone).await.status(), 200);
        let bad = json!({ "retention": { "keepDaily": 7 }, "timezone": "Nowhere/Land" });
        assert_eq!(api.post(path, &bad).await.status(), 422);

        let missing = api
            .post(
                "/api/v1/policies/nope/retention-preview",
                &json!({ "retention": { "keepLast": 1 } }),
            )
            .await;
        assert_eq!(missing.status(), 404);

        let reader = bearer_client(&ts.issue_token(&["snapshots:read"]).await);
        let denied = reader
            .post(format!("{}{path}", ts.rest_url()))
            .json(&json!({ "retention": { "keepLast": 1 } }))
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), 403);
        ts.stop().await;
    }
}
