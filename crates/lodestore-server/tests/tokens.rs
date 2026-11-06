mod common;

use common::{Api, TestServer, backends, bearer_client};
use data_encoding::BASE64URL_NOPAD;
use lodestore_agent::{EnrollArgs, NetOptions};
use lodestore_format::index::hex32;
use lodestore_format::repo::Repository;
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_server::catalog::ingest_snapshot;
use reqwest::{Client, Method, Response};
use serde_json::{Value, json};
use sqlx::Row;

const ALL_SCOPES: [&str; 14] = [
    "hosts:read",
    "hosts:write",
    "alerts:write",
    "agents:update",
    "policies:read",
    "policies:write",
    "hooks:write",
    "jobs:read",
    "jobs:run",
    "snapshots:read",
    "snapshots:delete",
    "restore:download",
    "restore:host",
    "restore:in-place",
];

async fn send(client: &Client, base: &str, method: &str, path: &str) -> Response {
    client
        .request(
            Method::from_bytes(method.as_bytes()).unwrap(),
            format!("{base}{path}"),
        )
        .json(&json!({}))
        .send()
        .await
        .expect("request")
}

async fn create(api: &Api, name: &str, scopes: &[&str], days: Value) -> Value {
    let response = api
        .post(
            "/api/v1/tokens",
            &json!({ "name": name, "scopes": scopes, "expires_in_days": days }),
        )
        .await;
    assert_eq!(response.status(), 201);
    response.json().await.unwrap()
}

async fn list(api: &Api, query: &str) -> Vec<Value> {
    let response = api.get(&format!("/api/v1/tokens{query}")).await;
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

async fn error_of(response: Response) -> (String, String) {
    let body: Value = response.json().await.unwrap();
    (
        body["code"].as_str().unwrap().to_string(),
        body["message"].as_str().unwrap().to_string(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tokens_lifecycle() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let created = create(&api, "ci", &["policies:read"], Value::Null).await;
        let client = bearer_client(created["token"].as_str().unwrap());
        let base = ts.rest_url();

        let read = send(&client, &base, "GET", "/api/v1/policies").await;
        assert_eq!(read.status(), 200, "[{}]", backend.name());

        let write = send(&client, &base, "POST", "/api/v1/policies").await;
        assert_eq!(write.status(), 403);
        let (code, message) = error_of(write).await;
        assert_eq!(code, "forbidden");
        assert_eq!(message, "The token needs the policies:write scope.");

        let used = list(&api, "").await;
        assert!(used[0]["last_used_at"].is_i64());

        let id = created["item"]["id"].as_str().unwrap();
        let revoke = api
            .post(&format!("/api/v1/tokens/{id}/revoke"), &json!({}))
            .await;
        assert_eq!(revoke.status(), 204);
        let after = send(&client, &base, "GET", "/api/v1/policies").await;
        assert_eq!(after.status(), 401);
        assert_eq!(error_of(after).await.0, "unauthorized");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tokens_shown_once() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let created = create(&api, "ci", &["jobs:run", "hosts:read"], json!(90)).await;
        let token = created["token"].as_str().unwrap();
        assert!(token.starts_with("lsk_"));
        assert_eq!(created["item"]["prefix"], token[..10]);
        assert_eq!(created["item"]["scopes"], json!(["hosts:read", "jobs:run"]));
        assert!(created["item"]["expires_at"].is_i64());

        let text = api.get("/api/v1/tokens").await.text().await.unwrap();
        assert!(!text.contains(token));
        assert!(text.contains(&token[..10]));

        let stored: Vec<u8> = sqlx::query_scalar("SELECT token_hash FROM api_tokens")
            .fetch_one(&ts.state().db.pool)
            .await
            .unwrap();
        assert_eq!(stored.len(), 32);

        let revoked = create(&api, "old", &["hosts:read"], Value::Null).await;
        let id = revoked["item"]["id"].as_str().unwrap();
        api.post(&format!("/api/v1/tokens/{id}/revoke"), &json!({}))
            .await;
        assert_eq!(list(&api, "?state=active").await.len(), 1);
        let gone = list(&api, "?state=revoked").await;
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0]["name"], "old");
        assert_eq!(list(&api, "").await.len(), 2);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tokens_validation() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let cases = [
            (
                json!(""),
                json!(["hosts:read"]),
                json!(null),
                "Enter a name for the token.",
            ),
            (
                json!("x".repeat(65)),
                json!(["hosts:read"]),
                json!(null),
                "Enter a name for the token.",
            ),
            (
                json!("ci"),
                json!([]),
                json!(null),
                "Choose at least one permission.",
            ),
            (
                json!("ci"),
                json!(["hosts:read"]),
                json!(7),
                "Choose 30 days, 90 days, 1 year or no expiry.",
            ),
        ];
        for (name, scopes, days, message) in cases {
            let response = api
                .post(
                    "/api/v1/tokens",
                    &json!({ "name": name, "scopes": scopes, "expires_in_days": days }),
                )
                .await;
            assert_eq!(response.status(), 422, "[{}]", backend.name());
            assert_eq!(error_of(response).await.1, message);
        }
        let unknown = api
            .post(
                "/api/v1/tokens",
                &json!({ "name": "ci", "scopes": ["root"], "expires_in_days": null }),
            )
            .await;
        assert_eq!(unknown.status(), 422);
        assert!(list(&api, "").await.is_empty());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tokens_expired_is_401() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let created = create(&api, "ci", &["policies:read"], json!(30)).await;
        let client = bearer_client(created["token"].as_str().unwrap());
        let base = ts.rest_url();
        assert_eq!(
            send(&client, &base, "GET", "/api/v1/policies")
                .await
                .status(),
            200
        );

        sqlx::query("UPDATE api_tokens SET expires_at = $1")
            .bind(1_i64)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        let expired = send(&client, &base, "GET", "/api/v1/policies").await;
        assert_eq!(expired.status(), 401, "[{}]", backend.name());
        assert_eq!(error_of(expired).await.0, "unauthorized");

        let unknown = bearer_client("lsk_unknown");
        let response = send(&unknown, &base, "GET", "/api/v1/policies").await;
        assert_eq!(response.status(), 401);
        let foreign = bearer_client("lodestore-api-token");
        let response = send(&foreign, &base, "GET", "/api/v1/policies").await;
        assert_eq!(response.status(), 401);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tokens_idempotent_revoke_and_duplicate_names() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let first = create(&api, "ci", &["hosts:read"], Value::Null).await;
        let second = create(&api, "ci", &["hosts:read"], Value::Null).await;
        assert_ne!(first["token"], second["token"]);
        assert_ne!(first["item"]["id"], second["item"]["id"]);

        let path = format!(
            "/api/v1/tokens/{}/revoke",
            first["item"]["id"].as_str().unwrap()
        );
        assert_eq!(api.post(&path, &json!({})).await.status(), 204);
        let stamp = list(&api, "?state=revoked").await[0]["revoked_at"].clone();
        assert!(stamp.is_i64());
        assert_eq!(api.post(&path, &json!({})).await.status(), 204);
        assert_eq!(list(&api, "?state=revoked").await[0]["revoked_at"], stamp);

        let missing = api.post("/api/v1/tokens/nope/revoke", &json!({})).await;
        assert_eq!(missing.status(), 404);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tokens_bearer_wins_over_cookie() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let token = ts.issue_token(&["hosts:read"]).await;
        let response = Client::new()
            .get(format!("{}/api/v1/policies", ts.rest_url()))
            .header("cookie", api.cookie())
            .header("authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "[{}]", backend.name());
        assert_eq!(error_of(response).await.0, "forbidden");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tokens_scheme_is_case_insensitive() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let token = ts.issue_token(&["hosts:read"]).await;
        for scheme in ["bearer", "BEARER"] {
            let response = Client::new()
                .get(format!("{}/api/v1/hosts", ts.rest_url()))
                .header("authorization", format!("{scheme} {token}"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200, "{scheme} [{}]", backend.name());
        }
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tokens_schema() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let pool = &ts.state().db.pool;
        let insert = |id: &'static str| async move {
            sqlx::query(
                "INSERT INTO api_tokens (id, name, prefix, token_hash, scopes_json, created_by, \
                 created_at) VALUES ($1, 'n', 'lsk_x', $2, '[]', 'admin', 0)",
            )
            .bind(id)
            .bind(vec![1u8; 32])
            .execute(pool)
            .await
        };
        insert("a").await.unwrap();
        insert("b").await.expect_err("the token hash is unique");
        ts.stop().await;
    }
}

const MATRIX: &[(&str, &str, &str)] = &[
    ("GET", "/api/v1/policies", "policies:read"),
    ("GET", "/api/v1/policies/nope", "policies:read"),
    ("POST", "/api/v1/policies/validate", "policies:read"),
    ("GET", "/api/v1/hosts/nope/policies", "policies:read"),
    ("POST", "/api/v1/policies", "policies:write"),
    ("POST", "/api/v1/policies/apply", "policies:write"),
    (
        "POST",
        "/api/v1/policies/nope/retention-preview",
        "policies:read",
    ),
    ("PUT", "/api/v1/policies/nope", "policies:write"),
    ("DELETE", "/api/v1/policies/nope", "policies:write"),
    ("POST", "/api/v1/policies/nope/run", "jobs:run"),
    ("POST", "/api/v1/hosts/nope/run", "jobs:run"),
    ("GET", "/api/v1/jobs", "jobs:read"),
    ("GET", "/api/v1/jobs/nope", "jobs:read"),
    ("GET", "/api/v1/jobs/nope/events", "jobs:read"),
    ("POST", "/api/v1/jobs/nope/cancel", "jobs:run"),
    ("POST", "/api/v1/jobs/nope/rerun", "jobs:run"),
    ("GET", "/api/v1/hosts", "hosts:read"),
    ("GET", "/api/v1/agent-updates", "hosts:read"),
    ("POST", "/api/v1/agent-updates", "agents:update"),
    ("POST", "/api/v1/agent-updates/nope/pause", "agents:update"),
    ("POST", "/api/v1/agent-updates/nope/resume", "agents:update"),
    ("POST", "/api/v1/agent-updates/nope/cancel", "agents:update"),
    (
        "POST",
        "/api/v1/agent-updates/nope/dismiss",
        "agents:update",
    ),
    ("GET", "/api/v1/hosts/nope", "hosts:read"),
    ("PUT", "/api/v1/hosts/nope/labels", "hosts:write"),
    ("POST", "/api/v1/hosts/nope/revoke", "hosts:write"),
    ("PUT", "/api/v1/hosts/nope/silence", "alerts:write"),
    ("DELETE", "/api/v1/hosts/nope/silence", "alerts:write"),
    ("GET", "/api/v1/restore/hosts", "snapshots:read"),
    ("GET", "/api/v1/restore/snapshots", "snapshots:read"),
    ("GET", "/api/v1/snapshots/nope", "snapshots:read"),
    ("GET", "/api/v1/snapshots/nope/entries", "snapshots:read"),
    ("GET", "/api/v1/snapshots/nope/search", "snapshots:read"),
    ("POST", "/api/v1/snapshots/nope/summary", "snapshots:read"),
    ("DELETE", "/api/v1/snapshots/nope", "snapshots:delete"),
    (
        "PUT",
        "/api/v1/snapshots/nope/suspicious",
        "snapshots:delete",
    ),
    ("GET", "/api/v1/hosts/nope/snapshots", "snapshots:read"),
    ("POST", "/api/v1/downloads", "restore:download"),
    (
        "GET",
        "/api/v1/downloads/nope/archive",
        "restore:download",
    ),
];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tokens_scope_matrix() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let base = ts.rest_url();
        for &(method, path, scope) in MATRIX {
            let others: Vec<&str> = ALL_SCOPES.into_iter().filter(|s| *s != scope).collect();
            let without = bearer_client(&ts.issue_token(&others).await);
            let denied = send(&without, &base, method, path).await;
            assert_eq!(denied.status(), 403, "{method} {path} [{}]", backend.name());
            let (code, message) = error_of(denied).await;
            assert_eq!(code, "forbidden");
            assert_eq!(message, format!("The token needs the {scope} scope."));

            let with = bearer_client(&ts.issue_token(&[scope]).await);
            let allowed = send(&with, &base, method, path).await;
            let status = allowed.status().as_u16();
            assert!(
                status != 401 && status != 403,
                "{method} {path} with {scope} gave {status} [{}]",
                backend.name()
            );
        }
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tokens_unknown_stored_scope_is_skipped() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let base = ts.rest_url();
        let secret = ts.issue_token(&["hosts:read"]).await;
        sqlx::query("UPDATE api_tokens SET scopes_json = $1")
            .bind(r#"["hosts:read","future:scope"]"#)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        let client = bearer_client(&secret);
        let allowed = send(&client, &base, "GET", "/api/v1/hosts").await;
        assert_eq!(allowed.status(), 200, "[{}]", backend.name());
        let denied = send(&client, &base, "GET", "/api/v1/policies").await;
        assert_eq!(denied.status(), 403, "[{}]", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tokens_session_only_routes() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let base = ts.rest_url();
        let all = bearer_client(&ts.issue_token(&ALL_SCOPES).await);
        for (method, path) in [
            ("GET", "/api/v1/tokens"),
            ("POST", "/api/v1/tokens"),
            ("POST", "/api/v1/tokens/nope/revoke"),
            ("POST", "/api/v1/join-tokens"),
            ("GET", "/api/v1/repositories"),
            ("POST", "/api/v1/repositories/check"),
            ("GET", "/api/v1/agent-policy"),
            ("GET", "/api/v1/catalog"),
            ("POST", "/api/v1/repositories/nope/catalog/sync"),
            ("POST", "/api/v1/repositories/nope/catalog/rebuild"),
            ("GET", "/api/v1/server/info"),
            ("GET", "/api/v1/auth/session"),
            ("POST", "/api/v1/auth/logout"),
        ] {
            let response = send(&all, &base, method, path).await;
            assert_eq!(
                response.status(),
                401,
                "{method} {path} [{}]",
                backend.name()
            );
        }
        ts.stop().await;
    }
}

fn b64(bytes: &[u8]) -> String {
    BASE64URL_NOPAD.encode(bytes)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tokens_actor_recorded() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let join = api.join_token(&repo, json!([])).await;
        lodestore_agent::enroll(EnrollArgs {
            token: join,
            paths: ts.agent_paths("host-a"),
            hostname: Some("host-a".to_string()),
            net: NetOptions::default(),
        })
        .await
        .expect("enroll");
        let host = api.hosts().await[0]["id"].as_str().unwrap().to_string();

        let store = LocalFsStore::new(ts.local_repo_root("main"));
        let opened = Repository::open(&store, b"test-password").await.unwrap();
        let keys = opened.read_keys().write_keys(opened.config_hash());
        let mut writer = Writer::new(&store, keys, opened.config()).await.unwrap();
        let data = ts.dir.path().join("tree");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("b.txt"), b"bravo").unwrap();
        let outcome = writer
            .backup(
                std::slice::from_ref(&data),
                BackupOptions::default(),
                &|_| {},
            )
            .await
            .expect("backup");
        let snapshot = hex32(&outcome.snapshot_id);
        ingest_snapshot(ts.state(), &repo, &snapshot, None)
            .await
            .expect("ingest");
        let item = b64(format!("{}/b.txt", data.canonicalize().unwrap().display()).as_bytes());

        let created = create(&api, "ci", &["restore:host", "restore:download"], Value::Null).await;
        let client = bearer_client(created["token"].as_str().unwrap());
        let base = ts.rest_url();

        let restore = client
            .post(format!("{base}/api/v1/restores"))
            .json(&json!({
                "snapshot_id": snapshot,
                "items": [item],
                "target_host_id": host,
                "destination": "/srv/out",
                "in_place": false,
                "conflict": "skip_existing",
                "restore_owners": false,
                "accept_unverified": true,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(restore.status(), 201, "[{}]", backend.name());
        let created_job: Value = restore.json().await.unwrap();
        let detail: Value = api
            .get(&format!(
                "/api/v1/jobs/{}",
                created_job["job_id"].as_str().unwrap()
            ))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(detail["restore"]["started_by"], "token:ci");

        let download = client
            .post(format!("{base}/api/v1/downloads"))
            .json(&json!({ "snapshot_id": snapshot, "items": [item], "format": "zip" }))
            .send()
            .await
            .unwrap();
        assert_eq!(download.status(), 201, "[{}]", backend.name());
        let started_by: String = sqlx::query("SELECT started_by FROM downloads")
            .fetch_one(&ts.state().db.pool)
            .await
            .unwrap()
            .try_get("started_by")
            .unwrap();
        assert_eq!(started_by, "token:ci");
        ts.stop().await;
    }
}
