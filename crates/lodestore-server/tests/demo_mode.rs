mod common;

use std::collections::BTreeSet;
use std::io::Read;
use std::time::Duration;

use common::{ADMIN_PASSWORD, Api, TestServer, WAIT, backends, bearer_client, wait_until};
use data_encoding::{BASE64, BASE64URL_NOPAD, HEXLOWER};
use lodestore_format::index::hex32;
use lodestore_format::repo::Repository;
use lodestore_format::store::LocalFsStore;
use lodestore_format::store::policy::agent_minio_policy_json;
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_server::catalog::ingest_snapshot;
use lodestore_server::db::now_ms;
use lodestore_server::demo::ALLOWED;
use lodestore_server::repos::db::server_access;
use lodestore_testkit::minio::MinioServer;
use reqwest::Method;
use serde_json::{Value, json};
use sqlx::Row;

const REPO_PASSWORD: &str = "correct horse battery";
const CHANNELS: &str = "/api/v1/notifications/channels";
const MIB: usize = 1 << 20;

const NIGHTLY_YAML: &str = "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: nightly\n\
    spec:\n  repository: main\n  selector:\n    matchLabels:\n      env: prod\n  paths:\n    - /etc\n";
const POLICY_CHECKS: &[(&str, &str)] = &[
    ("POST", "/api/v1/policies/validate"),
    ("POST", "/api/v1/policies/{name}/retention-preview"),
];

const SCHEMA_ONLY: &[(&str, &str)] = &[(
    "/api/v1/downloads/{id}/archive",
    "a binary stream; archive_download_works_in_demo reads it and the schema walk covers its fields",
)];

struct Reply {
    status: u16,
    headers: reqwest::header::HeaderMap,
    text: String,
}

impl Reply {
    fn code(&self) -> Option<String> {
        serde_json::from_str::<Value>(&self.text)
            .ok()?
            .get("code")?
            .as_str()
            .map(str::to_string)
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or_else(|_| panic!("not JSON: {}", self.text))
    }
}

struct Visitor {
    client: reqwest::Client,
    base: String,
    cookie: String,
    csrf: String,
}

impl Visitor {
    async fn establish(base: &str, path: &str, body: Option<Value>) -> Visitor {
        let client = reqwest::Client::new();
        let mut request = client.post(format!("{base}{path}"));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 204, "signing in at {path}");
        let cookie = response.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let mut visitor = Visitor::with_cookie(base, &cookie);
        let session = visitor
            .call(Method::GET, "/api/v1/auth/session", None)
            .await;
        assert_eq!(session.status, 200, "{}", session.text);
        visitor.csrf = session.json()["csrf_token"].as_str().unwrap().to_string();
        visitor
    }

    async fn demo(ts: &TestServer) -> Visitor {
        Visitor::establish(&ts.rest_url(), "/api/v1/auth/demo", None).await
    }

    async fn password(ts: &TestServer) -> Visitor {
        let body = json!({ "username": "admin", "password": ADMIN_PASSWORD });
        Visitor::establish(&ts.rest_url(), "/api/v1/auth/login", Some(body)).await
    }

    fn with_cookie(base: &str, cookie: &str) -> Visitor {
        Visitor {
            client: reqwest::Client::new(),
            base: base.to_string(),
            cookie: cookie.to_string(),
            csrf: String::new(),
        }
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        csrf: bool,
    ) -> reqwest::Response {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .header("cookie", &self.cookie);
        if csrf {
            request = request.header("x-csrf-token", &self.csrf);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        request.send().await.unwrap()
    }

    async fn send(&self, method: Method, path: &str, body: Option<Value>, csrf: bool) -> Reply {
        let response = self.request(method, path, body, csrf).await;
        let (status, headers) = (response.status().as_u16(), response.headers().clone());
        Reply {
            status,
            headers,
            text: response.text().await.unwrap_or_default(),
        }
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Reply {
        self.send(method, path, body, true).await
    }

    async fn get(&self, path: &str) -> Reply {
        self.call(Method::GET, path, None).await
    }

    async fn post(&self, path: &str, body: Value) -> Reply {
        self.call(Method::POST, path, Some(body)).await
    }
}

async fn demo_server(backend: &common::Backend) -> TestServer {
    TestServer::start_with(backend, |c| {
        c.demo_mode = true;
        c.schedule_tick_secs = 0;
    })
    .await
}

async fn plain_server(backend: &common::Backend) -> TestServer {
    TestServer::start_with(backend, |c| c.schedule_tick_secs = 0).await
}

async fn into_demo(ts: &mut TestServer) {
    ts.config.demo_mode = true;
    ts.restart().await;
}

fn operations() -> Vec<(String, String)> {
    let spec = serde_json::to_value(lodestore_server::api::openapi()).unwrap();
    let mut out = Vec::new();
    for (path, item) in spec["paths"].as_object().unwrap() {
        for method in ["get", "post", "put", "delete", "patch", "head", "options"] {
            if item.get(method).is_some() {
                out.push((method.to_ascii_uppercase(), path.clone()));
            }
        }
    }
    out
}

fn concrete(template: &str) -> String {
    let mut out = String::new();
    let mut skipping = false;
    for c in template.chars() {
        match c {
            '{' => {
                skipping = true;
                out.push('x');
            }
            '}' => skipping = false,
            _ if !skipping => out.push(c),
            _ => {}
        }
    }
    out
}

fn is_allowed(method: &str, path: &str) -> bool {
    ALLOWED.iter().any(|(m, p)| *m == method && *p == path)
}

#[test]
fn allowlist_has_no_stale_entries() {
    let known: BTreeSet<_> = operations().into_iter().collect();
    for (method, path) in ALLOWED {
        assert!(
            known.contains(&(method.to_string(), path.to_string())),
            "{method} {path} is in ALLOWED but not in the API"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_operation_is_classified() {
    for backend in backends() {
        let ts = demo_server(&backend).await;
        let visitor = Visitor::demo(&ts).await;
        let mut ops = operations();
        ops.sort_by_key(|(_, path)| path == "/api/v1/auth/logout");
        let mut refused = 0;
        for (method, path) in &ops {
            let url = concrete(path);
            let reply = visitor
                .call(method.parse().unwrap(), &url, Some(json!({})))
                .await;
            if is_allowed(method, path) {
                assert_ne!(
                    reply.code().as_deref(),
                    Some("demo_mode"),
                    "[{}] {method} {path} is allowed but refused",
                    backend.name()
                );
            } else {
                refused += 1;
                assert_eq!(
                    (reply.status, reply.code().as_deref()),
                    (403, Some("demo_mode")),
                    "[{}] {method} {path} must be refused: {}",
                    backend.name(),
                    reply.text
                );
                assert_eq!(reply.json()["message"], "Not available in the demo.");
            }
        }
        assert!(refused > 40, "only {refused} operations were refused");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bearer_is_refused() {
    for backend in backends() {
        let mut ts = plain_server(&backend).await;
        let token = ts.issue_token(&["jobs:read"]).await;
        let flag_off = bearer_client(&token)
            .get(format!("{}/api/v1/jobs", ts.rest_url()))
            .send()
            .await
            .unwrap();
        assert_eq!(flag_off.status(), 200, "the token works without the flag");

        into_demo(&mut ts).await;
        let visitor = Visitor::demo(&ts).await;
        let anonymous = reqwest::Client::new();
        for (client, path) in [
            (bearer_client(&token), "/api/v1/jobs"),
            (bearer_client("lsk_x"), "/api/v1/server/status"),
        ] {
            let reply = client
                .get(format!("{}{path}", ts.rest_url()))
                .send()
                .await
                .unwrap();
            assert_eq!(reply.status(), 403, "[{}] {path}", backend.name());
            assert_eq!(reply.json::<Value>().await.unwrap()["code"], "demo_mode");
        }
        let with_cookie_and_bearer = anonymous
            .get(format!("{}/api/v1/jobs", ts.rest_url()))
            .header("cookie", &visitor.cookie)
            .header("authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap();
        assert_eq!(with_cookie_and_bearer.status(), 403);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn password_login_is_closed() {
    for backend in backends() {
        let ts = demo_server(&backend).await;
        let reply = reqwest::Client::new()
            .post(format!("{}/api/v1/auth/login", ts.rest_url()))
            .json(&json!({ "username": "admin", "password": ADMIN_PASSWORD }))
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), 403, "[{}]", backend.name());
        assert_eq!(reply.json::<Value>().await.unwrap()["code"], "demo_mode");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn csrf_still_applies() {
    for backend in backends() {
        let ts = demo_server(&backend).await;
        let visitor = Visitor::demo(&ts).await;
        let reply = visitor
            .send(
                Method::POST,
                "/api/v1/find",
                Some(json!({ "pattern": "*.conf" })),
                false,
            )
            .await;
        assert_eq!(
            (reply.status, reply.code().as_deref()),
            (403, Some("csrf")),
            "[{}] {}",
            backend.name(),
            reply.text
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn head_and_wrong_methods_are_refused() {
    for backend in backends() {
        let mut ts = plain_server(&backend).await;
        let admin = Visitor::password(&ts).await;
        let control = admin
            .request(Method::HEAD, "/api/v1/tokens", None, true)
            .await;
        assert_eq!(control.status(), 200, "HEAD runs the GET handler");

        into_demo(&mut ts).await;
        let visitor = Visitor::demo(&ts).await;
        let head = visitor
            .request(Method::HEAD, "/api/v1/tokens", None, true)
            .await;
        assert_eq!(head.status(), 403, "[{}]", backend.name());
        for (method, path) in [
            (Method::PATCH, "/api/v1/tokens"),
            (Method::PUT, "/api/v1/server/status"),
            (Method::DELETE, "/api/v1/account"),
            (Method::OPTIONS, "/api/v1/tokens"),
            (Method::POST, "/api/v1/jobs"),
        ] {
            let reply = visitor.call(method.clone(), path, None).await;
            assert_eq!(
                (reply.status, reply.code().as_deref()),
                (403, Some("demo_mode")),
                "[{}] {method} {path}",
                backend.name()
            );
        }
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flag_off_leaves_routes_alone() {
    for backend in backends() {
        let ts = plain_server(&backend).await;
        let admin = Visitor::password(&ts).await;
        let tokens = admin.get("/api/v1/tokens").await;
        assert_eq!(tokens.status, 200, "{}", tokens.text);
        let draft = json!({
            "name": "x",
            "location": { "backend": "local", "options": { "path": ts.local_repo_root("x").display().to_string() } },
        });
        let check = admin.post("/api/v1/repositories/check", draft).await;
        assert_ne!(check.code().as_deref(), Some("demo_mode"), "{}", check.text);
        assert_eq!(check.status, 200, "{}", check.text);

        let demo = reqwest::Client::new()
            .post(format!("{}/api/v1/auth/demo", ts.rest_url()))
            .send()
            .await
            .unwrap();
        assert_eq!(demo.status(), 404, "[{}]", backend.name());
        assert_eq!(demo.json::<Value>().await.unwrap()["message"], "not found");

        let status = admin.get("/api/v1/server/status").await;
        assert_eq!(status.json()["demo_mode"], false);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_route_is_absent_in_demo() {
    for backend in backends() {
        let mut ts = plain_server(&backend).await;
        let admin = Visitor::password(&ts).await;
        let scrape = admin.get("/metrics").await;
        assert_eq!(scrape.status, 200, "{}", scrape.text);
        assert!(scrape.text.contains("# TYPE"), "{}", scrape.text);

        into_demo(&mut ts).await;
        let visitor = Visitor::demo(&ts).await;
        let reply = visitor.get("/metrics").await;
        assert!(
            !reply.text.contains("# TYPE") && !reply.text.contains("# HELP"),
            "[{}] /metrics is served in the demo: {}",
            backend.name(),
            reply.text
        );
        assert_ne!(
            reply
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some(lodestore_server::metrics::CONTENT_TYPE_TEXT)
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changing_the_mode_ends_every_session() {
    for backend in backends() {
        let mut ts = plain_server(&backend).await;
        let admin = Visitor::password(&ts).await;

        ts.restart().await;
        let kept = Visitor::with_cookie(&ts.rest_url(), &admin.cookie)
            .get("/api/v1/auth/session")
            .await;
        assert_eq!(
            kept.status,
            200,
            "[{}] a plain restart keeps sessions",
            backend.name()
        );

        into_demo(&mut ts).await;
        let stale = Visitor::with_cookie(&ts.rest_url(), &admin.cookie)
            .get("/api/v1/auth/session")
            .await;
        assert_eq!(
            stale.status,
            401,
            "[{}] admin cookie into demo",
            backend.name()
        );

        let visitor = Visitor::demo(&ts).await;
        ts.restart().await;
        let kept = Visitor::with_cookie(&ts.rest_url(), &visitor.cookie)
            .get("/api/v1/auth/session")
            .await;
        assert_eq!(
            kept.status,
            200,
            "[{}] demo restart keeps sessions",
            backend.name()
        );

        ts.config.demo_mode = false;
        ts.restart().await;
        let stale = Visitor::with_cookie(&ts.rest_url(), &visitor.cookie)
            .get("/api/v1/auth/session")
            .await;
        assert_eq!(
            stale.status,
            401,
            "[{}] demo cookie out of demo",
            backend.name()
        );
        let count: i64 = sqlx::query("SELECT COUNT(*) AS n FROM sessions")
            .fetch_one(&ts.state().db.pool)
            .await
            .unwrap()
            .try_get("n")
            .unwrap();
        assert_eq!(count, 0);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn demo_session_lasts_an_hour() {
    for backend in backends() {
        let ts = demo_server(&backend).await;
        let response = reqwest::Client::new()
            .post(format!("{}/api/v1/auth/demo", ts.rest_url()))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 204);
        let set_cookie = response.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .to_string();
        assert!(set_cookie.contains("Max-Age=3600"), "{set_cookie}");
        let cookie = set_cookie.split(';').next().unwrap().to_string();

        let pool = &ts.state().db.pool;
        let row = sqlx::query("SELECT created_at, expires_at FROM sessions")
            .fetch_one(pool)
            .await
            .unwrap();
        let (created, expires): (i64, i64) = (
            row.try_get("created_at").unwrap(),
            row.try_get("expires_at").unwrap(),
        );
        assert_eq!(expires - created, 3_600_000, "[{}]", backend.name());

        let session = Visitor::with_cookie(&ts.rest_url(), &cookie);
        assert_eq!(session.get("/api/v1/auth/session").await.status, 200);

        let past = now_ms() - 1_000;
        sqlx::query("UPDATE sessions SET expires_at = $1, last_seen_at = $2")
            .bind(past)
            .bind(past - 2 * 3_600_000)
            .execute(pool)
            .await
            .unwrap();
        let expired = session.get("/api/v1/auth/session").await;
        assert_eq!(expired.status, 401, "[{}]", backend.name());
        let after: i64 = sqlx::query("SELECT expires_at FROM sessions")
            .fetch_one(pool)
            .await
            .unwrap()
            .try_get("expires_at")
            .unwrap();
        assert_eq!(after, past, "an expired demo session is not extended");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_demo_signs_in() {
    for backend in backends() {
        let ts = demo_server(&backend).await;
        let visitor = Visitor::demo(&ts).await;
        let session = visitor.get("/api/v1/auth/session").await;
        assert_eq!(session.status, 200);
        assert_eq!(session.json()["username"], "admin");
        assert!(!visitor.csrf.is_empty());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_demo_needs_an_admin() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| {
            c.demo_mode = true;
            c.schedule_tick_secs = 0;
            c.admin_password = None;
        })
        .await;
        let reply = reqwest::Client::new()
            .post(format!("{}/api/v1/auth/demo", ts.rest_url()))
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), 409, "[{}]", backend.name());
        assert_eq!(
            reply.json::<Value>().await.unwrap()["code"],
            "setup_required"
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_demo_is_rate_limited() {
    for backend in backends() {
        let ts = demo_server(&backend).await;
        let client = reqwest::Client::new();
        let url = format!("{}/api/v1/auth/demo", ts.rest_url());
        for n in 0..30 {
            let reply = client.post(&url).send().await.unwrap();
            assert_eq!(reply.status(), 204, "[{}] sign-in {n}", backend.name());
        }
        let reply = client.post(&url).send().await.unwrap();
        assert_eq!(reply.status(), 429, "[{}]", backend.name());
        assert!(reply.headers().contains_key("retry-after"));
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_reports_demo_mode() {
    for backend in backends() {
        let mut ts = plain_server(&backend).await;
        let anonymous = Visitor::with_cookie(&ts.rest_url(), "");
        let off = anonymous
            .send(Method::GET, "/api/v1/server/status", None, false)
            .await;
        assert_eq!(off.json()["demo_mode"], false);

        into_demo(&mut ts).await;
        let on = anonymous
            .send(Method::GET, "/api/v1/server/status", None, false)
            .await;
        assert_eq!(on.json()["demo_mode"], true, "[{}]", backend.name());
        let visitor = Visitor::demo(&ts).await;
        assert_eq!(
            visitor.get("/api/v1/server/info").await.json()["demo_mode"],
            true
        );
        ts.stop().await;
    }
}

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

struct Archive {
    snapshot: String,
    base: String,
}

impl Archive {
    fn item(&self, below: &str) -> String {
        BASE64URL_NOPAD.encode(format!("{}/{below}", self.base).as_bytes())
    }
}

async fn seed_archive(ts: &TestServer, big: usize) -> Archive {
    let repo = ts.create_local_repository("main").await;
    let store = LocalFsStore::new(ts.local_repo_root("main"));
    let opened = Repository::open(&store, b"test-password").await.unwrap();
    let keys = opened.read_keys().write_keys(opened.config_hash());
    let mut writer = Writer::new(&store, keys, opened.config()).await.unwrap();
    let data = ts.dir.path().join("tree");
    std::fs::create_dir_all(data.join("docs")).unwrap();
    std::fs::write(data.join("docs/a.txt"), b"alpha").unwrap();
    if big > 0 {
        std::fs::write(data.join("big.bin"), noise(big, 7)).unwrap();
    }
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
    let snapshot = hex32(&outcome.snapshot_id);
    ingest_snapshot(ts.state(), &repo, &snapshot, None)
        .await
        .expect("ingest");
    Archive {
        snapshot,
        base: data.canonicalize().unwrap().display().to_string(),
    }
}

async fn download_row(ts: &TestServer, id: &str) -> (String, Option<String>) {
    let row = sqlx::query("SELECT state, error_code FROM downloads WHERE id = $1")
        .bind(id)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap();
    (
        row.try_get("state").unwrap(),
        row.try_get("error_code").unwrap(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn archive_download_works_in_demo() {
    for backend in backends() {
        let mut ts = plain_server(&backend).await;
        let archive = seed_archive(&ts, 0).await;
        into_demo(&mut ts).await;
        let visitor = Visitor::demo(&ts).await;

        let created = visitor
            .post(
                "/api/v1/downloads",
                json!({
                    "snapshot_id": archive.snapshot,
                    "items": [archive.item("docs")],
                    "format": "tar_gz",
                }),
            )
            .await;
        assert_eq!(created.status, 201, "[{}] {}", backend.name(), created.text);
        let url = created.json()["url"].as_str().unwrap().to_string();
        let response = visitor.request(Method::GET, &url, None, false).await;
        assert_eq!(response.status(), 200);
        let body = response.bytes().await.unwrap();
        let mut tar = Vec::new();
        flate2::read::GzDecoder::new(&body[..])
            .read_to_end(&mut tar)
            .unwrap();
        assert!(
            tar.windows(5).any(|w| w == b"alpha"),
            "the archive holds the file"
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn demo_downloads_are_limited() {
    for backend in backends() {
        let mut ts = TestServer::start_with(&backend, |c| {
            c.schedule_tick_secs = 0;
            c.demo.max_downloads = 1;
            c.demo.download_timeout = Duration::from_secs(2);
        })
        .await;
        let archive = seed_archive(&ts, 24 * MIB).await;
        into_demo(&mut ts).await;
        let visitor = Visitor::demo(&ts).await;

        let mut links = Vec::new();
        for _ in 0..2 {
            let created = visitor
                .post(
                    "/api/v1/downloads",
                    json!({
                        "snapshot_id": archive.snapshot,
                        "items": [archive.item("big.bin")],
                        "format": "tar_zst",
                    }),
                )
                .await;
            assert_eq!(created.status, 201, "{}", created.text);
            let body = created.json();
            links.push((
                body["id"].as_str().unwrap().to_string(),
                body["url"].as_str().unwrap().to_string(),
            ));
        }

        let slow = visitor.request(Method::GET, &links[0].1, None, false).await;
        assert_eq!(slow.status(), 200, "[{}]", backend.name());
        assert_eq!(download_row(&ts, &links[0].0).await.0, "running");

        let refused = visitor.send(Method::GET, &links[1].1, None, false).await;
        assert_eq!(refused.status, 429, "[{}] {}", backend.name(), refused.text);
        assert_eq!(download_row(&ts, &links[1].0).await.0, "pending");

        let closed = wait_until(Duration::from_secs(20), || async {
            download_row(&ts, &links[0].0).await.0 != "running"
        })
        .await;
        assert!(
            closed,
            "[{}] the stalled download is still running",
            backend.name()
        );
        assert_eq!(
            download_row(&ts, &links[0].0).await,
            ("interrupted".to_string(), Some("timed_out".to_string()))
        );

        let second = visitor.request(Method::GET, &links[1].1, None, false).await;
        assert_eq!(second.status(), 200, "the slot is free again");

        let truncated = match slow.bytes().await {
            Ok(bytes) => bytes.len() < 24 * MIB,
            Err(_) => true,
        };
        assert!(truncated, "the stalled stream was cut off");
        drop(second);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channels_are_hidden_in_demo() {
    for backend in backends() {
        let mut ts = plain_server(&backend).await;
        let admin = Api::login(&ts.rest_url(), "admin", ADMIN_PASSWORD).await;
        let created = admin
            .post(
                CHANNELS,
                &json!({ "type": "webhook", "name": "hook", "enabled": true,
                    "settings": { "url": "https://hooks.example.test/hidden-url" },
                    "secret": "hidden-secret" }),
            )
            .await;
        assert_eq!(created.status(), 201);
        let id = created.json::<Value>().await.unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        into_demo(&mut ts).await;
        let visitor = Visitor::demo(&ts).await;
        let list = visitor.get(CHANNELS).await;
        assert_eq!(list.status, 200);
        assert_eq!(list.text, "[]", "[{}]", backend.name());
        let one = visitor.get(&format!("{CHANNELS}/{id}")).await;
        assert_eq!(
            (one.status, one.code().as_deref()),
            (403, Some("demo_mode"))
        );
        let post = visitor.post(CHANNELS, json!({})).await;
        assert_eq!(post.code().as_deref(), Some("demo_mode"));
        ts.stop().await;
    }
}

fn collect_fields(
    spec: &Value,
    schema: &Value,
    seen: &mut BTreeSet<String>,
    out: &mut Vec<String>,
) {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let name = reference.rsplit('/').next().unwrap();
        if seen.insert(name.to_string()) {
            collect_fields(spec, &spec["components"]["schemas"][name], seen, out);
        }
    }
    for key in ["allOf", "oneOf", "anyOf", "prefixItems"] {
        for part in schema
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            collect_fields(spec, part, seen, out);
        }
    }
    for key in ["items", "additionalProperties"] {
        if let Some(inner) = schema.get(key).filter(|v| v.is_object()) {
            collect_fields(spec, inner, seen, out);
        }
    }
    for (name, inner) in schema
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        out.push(name.clone());
        collect_fields(spec, inner, seen, out);
    }
}

const SECRET_WORDS: &[&str] = &[
    "secret",
    "password",
    "token",
    "private_key",
    "recovery",
    "master",
    "access_key",
    "webhook",
];
const SECRET_NAMES: &[&str] = &["url", "chat_id", "smtp_host"];
const NAME_EXCEPTIONS: &[(&str, &str)] = &[
    (
        "secret_set",
        "a flag that says a secret exists, never its value",
    ),
    ("csrf_token", "belongs to the caller's own session"),
];

#[test]
fn allowed_responses_have_no_secret_fields() {
    let spec = serde_json::to_value(lodestore_server::api::openapi()).unwrap();
    let mut checked = 0;
    for (method, path) in ALLOWED.iter().filter(|(m, _)| *m == "GET") {
        let ok = &spec["paths"][*path]["get"]["responses"]["200"];
        let content = ok.get("content").and_then(Value::as_object);
        let content = content.unwrap_or_else(|| panic!("{method} {path} has no 200 content"));
        assert!(!content.is_empty(), "{method} {path} has no 200 content");
        let mut fields = Vec::new();
        for media in content.values() {
            if let Some(schema) = media.get("schema") {
                collect_fields(&spec, schema, &mut BTreeSet::new(), &mut fields);
            }
        }
        for field in fields {
            if NAME_EXCEPTIONS.iter().any(|(name, _)| *name == field) {
                continue;
            }
            let lower = field.to_ascii_lowercase();
            assert!(
                !SECRET_WORDS.iter().any(|word| lower.contains(word)),
                "{method} {path} returns a field named {field}"
            );
            assert!(
                *path == CHANNELS || !SECRET_NAMES.contains(&lower.as_str()),
                "{method} {path} returns a field named {field}"
            );
        }
        checked += 1;
    }
    assert!(checked > 30, "only {checked} responses were checked");
}

#[test]
fn policy_check_responses_have_no_secret_fields() {
    let spec = serde_json::to_value(lodestore_server::api::openapi()).unwrap();
    for (method, path) in POLICY_CHECKS {
        assert!(is_allowed(method, path), "{method} {path} is not allowed");
        let ok = &spec["paths"][*path]["post"]["responses"]["200"]["content"]["application/json"]["schema"];
        let mut fields = Vec::new();
        collect_fields(&spec, ok, &mut BTreeSet::new(), &mut fields);
        assert!(!fields.is_empty(), "{method} {path} has no response fields");
        for field in fields {
            let lower = field.to_ascii_lowercase();
            assert!(
                !SECRET_WORDS.iter().any(|word| lower.contains(word))
                    && !SECRET_NAMES.contains(&lower.as_str()),
                "{method} {path} returns a field named {field}"
            );
        }
    }
}

#[test]
fn the_schema_walk_sees_the_channel_settings() {
    let spec = serde_json::to_value(lodestore_server::api::openapi()).unwrap();
    let schema = &spec["paths"][CHANNELS]["get"]["responses"]["200"]["content"]["application/json"]
        ["schema"];
    let mut fields = Vec::new();
    collect_fields(&spec, schema, &mut BTreeSet::new(), &mut fields);
    for expected in ["url", "chat_id", "smtp_host", "secret_set"] {
        assert!(
            fields.iter().any(|f| f == expected),
            "{expected} not reached: {fields:?}"
        );
    }
}

struct Ids {
    repo: String,
    snapshot: String,
    agent: String,
    policy: String,
    job: String,
    cycle: String,
    run: String,
    find: String,
    base: String,
}

impl Ids {
    fn dummy() -> Ids {
        let x = || "x".to_string();
        Ids {
            repo: x(),
            snapshot: x(),
            agent: x(),
            policy: x(),
            job: x(),
            cycle: x(),
            run: x(),
            find: x(),
            base: x(),
        }
    }
}

fn get_table(ids: &Ids) -> Vec<(&'static str, String)> {
    let repo = &ids.repo;
    let snapshot = &ids.snapshot;
    let run = &ids.run;
    let checks = format!("/api/v1/repositories/{repo}/checks");
    vec![
        ("/api/v1/auth/session", "/api/v1/auth/session".into()),
        ("/api/v1/server/status", "/api/v1/server/status".into()),
        ("/api/v1/server/info", "/api/v1/server/info".into()),
        ("/api/v1/account", "/api/v1/account".into()),
        ("/api/v1/tokens", "/api/v1/tokens".into()),
        (
            "/api/v1/agent-policy",
            "/api/v1/agent-policy?bucket=demo-bucket".into(),
        ),
        ("/api/v1/fleet", "/api/v1/fleet".into()),
        (
            "/api/v1/fleet/hosts/{id}",
            format!("/api/v1/fleet/hosts/{}", ids.agent),
        ),
        ("/api/v1/hosts", "/api/v1/hosts".into()),
        ("/api/v1/hosts/{id}", format!("/api/v1/hosts/{}", ids.agent)),
        (
            "/api/v1/hosts/{id}/snapshots",
            format!("/api/v1/hosts/{}/snapshots", ids.agent),
        ),
        (
            "/api/v1/hosts/{id}/policies",
            format!("/api/v1/hosts/{}/policies", ids.agent),
        ),
        ("/api/v1/catalog", "/api/v1/catalog".into()),
        (
            "/api/v1/snapshots/{id}",
            format!("/api/v1/snapshots/{snapshot}"),
        ),
        (
            "/api/v1/snapshots/{id}/entries",
            format!("/api/v1/snapshots/{snapshot}/entries"),
        ),
        (
            "/api/v1/snapshots/{id}/search",
            format!(
                "/api/v1/snapshots/{snapshot}/search?path={}&q=txt",
                ids.base
            ),
        ),
        ("/api/v1/restore/hosts", "/api/v1/restore/hosts".into()),
        (
            "/api/v1/restore/snapshots",
            format!("/api/v1/restore/snapshots?host={}", ids.agent),
        ),
        ("/api/v1/find/{id}", format!("/api/v1/find/{}", ids.find)),
        (
            "/api/v1/find/{id}/groups/{index}",
            format!("/api/v1/find/{}/groups/0", ids.find),
        ),
        ("/api/v1/jobs", "/api/v1/jobs".into()),
        ("/api/v1/jobs/{id}", format!("/api/v1/jobs/{}", ids.job)),
        (
            "/api/v1/jobs/{id}/events",
            format!("/api/v1/jobs/{}/events", ids.job),
        ),
        ("/api/v1/alerts", "/api/v1/alerts".into()),
        ("/api/v1/alerts/count", "/api/v1/alerts/count".into()),
        ("/api/v1/policies", "/api/v1/policies".into()),
        (
            "/api/v1/policies/{name}",
            format!("/api/v1/policies/{}", ids.policy),
        ),
        ("/api/v1/repositories", "/api/v1/repositories".into()),
        (
            "/api/v1/repositories/{id}",
            format!("/api/v1/repositories/{repo}"),
        ),
        (
            "/api/v1/repositories/{id}/gc",
            format!("/api/v1/repositories/{repo}/gc"),
        ),
        (
            "/api/v1/repositories/{id}/gc/cycles/{cycle_id}/report",
            format!("/api/v1/repositories/{repo}/gc/cycles/{}/report", ids.cycle),
        ),
        ("/api/v1/repositories/{id}/checks", checks.clone()),
        (
            "/api/v1/repositories/{id}/checks/runs/{run_id}",
            format!("{checks}/runs/{run}"),
        ),
        (
            "/api/v1/repositories/{id}/checks/runs/{run_id}/report",
            format!("{checks}/runs/{run}/report"),
        ),
        ("/api/v1/agent-updates", "/api/v1/agent-updates".into()),
        (CHANNELS, CHANNELS.into()),
        (
            "/api/v1/notifications/thresholds",
            "/api/v1/notifications/thresholds".into(),
        ),
    ]
}

#[test]
fn every_allowed_get_is_classified() {
    let table = get_table(&Ids::dummy());
    let tabled: BTreeSet<_> = table.iter().map(|(template, _)| *template).collect();
    let skipped: BTreeSet<_> = SCHEMA_ONLY.iter().map(|(template, _)| *template).collect();
    for (method, path) in ALLOWED.iter().filter(|(m, _)| *m == "GET") {
        assert!(
            tabled.contains(path) || skipped.contains(path),
            "{method} {path} is allowed but neither in the secret-search table nor in SCHEMA_ONLY"
        );
    }
    for template in tabled.iter().chain(skipped.iter()) {
        assert!(
            is_allowed("GET", template),
            "{template} is classified but not allowed"
        );
    }
    assert!(tabled.is_disjoint(&skipped));
}

fn variants(bytes: &[u8]) -> Vec<String> {
    vec![
        HEXLOWER.encode(bytes),
        BASE64.encode(bytes),
        BASE64URL_NOPAD.encode(bytes),
    ]
}

async fn token_hashes(ts: &TestServer, table: &str) -> Vec<String> {
    let query = match table {
        "api_tokens" => "SELECT token_hash FROM api_tokens",
        _ => "SELECT token_hash FROM join_tokens",
    };
    let rows = sqlx::query(query)
        .fetch_all(&ts.state().db.pool)
        .await
        .unwrap();
    assert!(!rows.is_empty(), "no rows in {table}");
    rows.iter()
        .flat_map(|row| variants(&row.try_get::<Vec<u8>, _>("token_hash").unwrap()))
        .collect()
}

struct Seeded {
    ids: Ids,
    needles: Vec<String>,
    s3: Option<String>,
}

async fn wizard(api: &Api, draft: &Value) -> (String, String) {
    let started = api.post("/api/v1/repositories/wizard", draft).await;
    assert_eq!(started.status(), 201);
    let started: Value = started.json().await.unwrap();
    let key = started["recovery_key"].as_str().unwrap().to_string();
    let committed = api
        .post(
            &format!(
                "/api/v1/repositories/wizard/{}/commit",
                started["wizard_id"].as_str().unwrap()
            ),
            &json!({
                "password": REPO_PASSWORD,
                "recovery_key_last_group": key.rsplit('-').next().unwrap(),
            }),
        )
        .await;
    assert_eq!(committed.status(), 201);
    let committed: Value = committed.json().await.unwrap();
    (committed["id"].as_str().unwrap().to_string(), key)
}

async fn json_of(response: reqwest::Response, status: u16) -> Value {
    let got = response.status().as_u16();
    let text = response.text().await.unwrap();
    assert_eq!(got, status, "{text}");
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

async fn seed_everything(ts: &TestServer, minio: Option<&MinioServer>) -> Seeded {
    let api = ts.api().await;
    let (repo, recovery) = wizard(
        &api,
        &json!({
            "name": "main",
            "location": { "backend": "local", "options": { "path": ts.local_repo_root("main").display().to_string() } },
        }),
    )
    .await;
    let mut needles = vec![
        REPO_PASSWORD.to_string(),
        recovery.clone(),
        recovery.to_ascii_lowercase(),
        "recovery_key".to_string(),
        ADMIN_PASSWORD.to_string(),
        "$argon2".to_string(),
    ];
    needles.extend(variants(&[7u8; 32]));
    let access = server_access(ts.state(), &repo).await.unwrap();
    needles.extend(variants(
        &access.server_slot_secret.expect("a server slot secret"),
    ));
    needles.extend(variants(&access.master));

    sqlx::query(
        "INSERT INTO agents (id, hostname, repository_id, labels_json, os, arch, version, \
         status, enrolled_at, cert_serial, cert_issued_at, cert_not_after, last_seen_at) \
         VALUES ('agent-demo', 'host-a', $1, '[]', 'linux', 'x86_64', '0.1.0', 'active', $2, \
         'serial', $2, $3, $2)",
    )
    .bind(&repo)
    .bind(now_ms() - 86_400_000)
    .bind(now_ms() + 86_400_000 * 300)
    .execute(&ts.state().db.pool)
    .await
    .unwrap();

    let store = LocalFsStore::new(ts.local_repo_root("main"));
    let opened = Repository::open(&store, REPO_PASSWORD.as_bytes())
        .await
        .unwrap();
    let keys = opened.read_keys().write_keys(opened.config_hash());
    let mut writer = Writer::new(&store, keys, opened.config()).await.unwrap();
    let data = ts.dir.path().join("tree");
    std::fs::create_dir_all(data.join("docs")).unwrap();
    std::fs::write(data.join("docs/a.txt"), b"alpha").unwrap();
    std::fs::write(data.join("b.txt"), b"bravo").unwrap();
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
    let snapshot = hex32(&outcome.snapshot_id);
    ingest_snapshot(ts.state(), &repo, &snapshot, None)
        .await
        .expect("ingest");
    let base = BASE64URL_NOPAD.encode(
        data.canonicalize()
            .unwrap()
            .display()
            .to_string()
            .as_bytes(),
    );

    json_of(
        api.post("/api/v1/policies", &json!({ "yaml": NIGHTLY_YAML }))
            .await,
        201,
    )
    .await;

    sqlx::query(
        "INSERT INTO jobs (id, host_id, policy_name, source, state, spec, queued_at, finished_at) \
         VALUES ('job-demo', 'agent-demo', 'nightly', 'manual', 'succeeded', $1, $2, $3)",
    )
    .bind(Vec::<u8>::new())
    .bind(now_ms() - 2_000)
    .bind(now_ms() - 1_000)
    .execute(&ts.state().db.pool)
    .await
    .unwrap();

    let started = json_of(
        api.post(
            &format!("/api/v1/repositories/{repo}/gc/dry-run"),
            &json!({}),
        )
        .await,
        202,
    )
    .await;
    let cycle = started["cycle_id"].as_str().unwrap().to_string();
    let finished = wait_until(WAIT, || async {
        let status = json_of(
            api.get(&format!("/api/v1/repositories/{repo}/gc")).await,
            200,
        )
        .await;
        status["cycle"].is_null() && !status["last_dry_run"].is_null()
    })
    .await;
    assert!(finished, "the dry run did not finish");

    let started = json_of(
        api.post(
            &format!("/api/v1/repositories/{repo}/checks/structure/run"),
            &json!({}),
        )
        .await,
        202,
    )
    .await;
    let run = started["run_id"].as_str().unwrap().to_string();
    let finished = wait_until(WAIT, || async {
        let state = json_of(
            api.get(&format!("/api/v1/repositories/{repo}/checks/runs/{run}"))
                .await,
            200,
        )
        .await;
        !matches!(state["state"].as_str(), Some("queued" | "running"))
    })
    .await;
    assert!(finished, "the check did not finish");

    let token = json_of(
        api.post(
            "/api/v1/tokens",
            &json!({ "name": "ci", "scopes": ["jobs:read"], "expires_in_days": null }),
        )
        .await,
        201,
    )
    .await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let join = api.join_token(&repo, json!([])).await;
    needles.extend([token, join]);
    needles.extend(token_hashes(ts, "api_tokens").await);
    needles.extend(token_hashes(ts, "join_tokens").await);

    let channels = [
        json!({ "type": "webhook", "name": "hook", "enabled": true,
            "settings": { "url": "https://hooks.example.test/unique-webhook-path" },
            "secret": "unique-webhook-bearer" }),
        json!({ "type": "telegram", "name": "tg", "enabled": true,
            "settings": { "chat_id": "-100777123456" }, "secret": "123456:unique-bot-token" }),
        json!({ "type": "slack", "name": "sl", "enabled": true,
            "secret": "https://hooks.slack.test/services/unique-slack-path" }),
        json!({ "type": "email", "name": "mail", "enabled": true,
            "settings": { "smtp_host": "smtp.unique-host.example.test", "port": 1,
                "username": "unique-smtp-user", "from": "a@example.test",
                "recipients": ["b@example.test"] },
            "secret": "unique-smtp-password" }),
    ];
    for channel in &channels {
        json_of(api.post(CHANNELS, channel).await, 201).await;
    }
    needles.extend(
        [
            "unique-webhook-path",
            "unique-webhook-bearer",
            "-100777123456",
            "unique-bot-token",
            "unique-slack-path",
            "unique-host.example.test",
            "unique-smtp-user",
            "unique-smtp-password",
        ]
        .map(str::to_string),
    );

    let mut s3 = None;
    if let Some(minio) = minio {
        minio.create_bucket("repo").await;
        let server = minio.root_credentials();
        let agent = minio
            .create_user_with_policy("repo", &agent_minio_policy_json("repo", ""))
            .await;
        let creds = |pair: &(String, String)| json!({ "access_key_id": pair.0, "secret_access_key": pair.1 });
        let (id, key) = wizard(
            &api,
            &json!({
                "name": "cloud",
                "location": {
                    "backend": "s3",
                    "options": { "bucket": "repo", "endpoint": minio.endpoint(), "path_style": "true" },
                },
                "server_credentials": creds(&server),
                "agent_credentials": creds(&agent),
            }),
        )
        .await;
        needles.extend([server.1, agent.1, key.clone(), key.to_ascii_lowercase()]);
        let access = server_access(ts.state(), &id).await.unwrap();
        needles.extend(variants(&access.master));
        needles.extend(variants(&access.server_slot_secret.unwrap_or_default()));
        s3 = Some(id);
    }

    Seeded {
        ids: Ids {
            repo,
            snapshot,
            agent: "agent-demo".into(),
            policy: "nightly".into(),
            job: "job-demo".into(),
            cycle,
            run,
            find: String::new(),
            base,
        },
        needles,
        s3,
    }
}

async fn assert_gets_leak_nothing(backend: &common::Backend, minio: Option<&MinioServer>) {
    let mut ts = plain_server(backend).await;
    let mut seeded = seed_everything(&ts, minio).await;
    into_demo(&mut ts).await;
    let visitor = Visitor::demo(&ts).await;

    let started = visitor
        .post("/api/v1/find", json!({ "pattern": "*.txt" }))
        .await;
    assert_eq!(started.status, 202, "{}", started.text);
    seeded.ids.find = started.json()["id"].as_str().unwrap().to_string();
    let find = seeded.ids.find.clone();
    let done = wait_until(WAIT, || async {
        visitor.get(&format!("/api/v1/find/{find}")).await.json()["state"] != "running"
    })
    .await;
    assert!(done, "the search did not finish");

    let mut paths = get_table(&seeded.ids);
    if let Some(s3) = &seeded.s3 {
        for suffix in ["", "/gc", "/checks"] {
            paths.push(("extra", format!("/api/v1/repositories/{s3}{suffix}")));
        }
    }
    assert!(!seeded.needles.iter().any(String::is_empty));
    let mut bodies = String::new();
    for (template, path) in &paths {
        let reply = visitor.get(path).await;
        assert_eq!(
            reply.status,
            200,
            "[{}] GET {path} ({template}): {}",
            backend.name(),
            reply.text
        );
        for needle in &seeded.needles {
            assert!(
                !reply.text.contains(needle.as_str()),
                "[{}] GET {path} leaks {needle}",
                backend.name()
            );
        }
        bodies.push_str(&reply.text);
    }
    let checks = [
        (
            "/api/v1/policies/validate".to_string(),
            json!({ "yaml": NIGHTLY_YAML }),
        ),
        (
            "/api/v1/policies/nightly/retention-preview".to_string(),
            json!({ "retention": { "keepLast": 1 } }),
        ),
    ];
    for (path, body) in checks {
        let reply = visitor.post(&path, body).await;
        assert_eq!(
            reply.status,
            200,
            "[{}] POST {path}: {}",
            backend.name(),
            reply.text
        );
        for needle in &seeded.needles {
            assert!(
                !reply.text.contains(needle.as_str()),
                "[{}] POST {path} leaks {needle}",
                backend.name()
            );
        }
        bodies.push_str(&reply.text);
    }
    assert!(bodies.contains("host-a"), "the walk saw the seeded data");
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn allowed_gets_leak_no_secrets() {
    for backend in backends() {
        assert_gets_leak_nothing(&backend, None).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn allowed_gets_leak_no_secrets_on_minio() {
    for backend in backends() {
        let minio = MinioServer::start().await;
        assert_gets_leak_nothing(&backend, Some(&minio)).await;
    }
}

async fn count(ts: &TestServer, table: &str) -> i64 {
    let query = match table {
        "policies" => "SELECT COUNT(*) AS n FROM policies",
        "jobs" => "SELECT COUNT(*) AS n FROM jobs",
        "job_events" => "SELECT COUNT(*) AS n FROM job_events",
        "snapshots" => "SELECT COUNT(*) AS n FROM snapshots",
        _ => "SELECT COUNT(*) AS n FROM agents",
    };
    sqlx::query(query)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .try_get("n")
        .unwrap()
}

async fn policy_state(ts: &TestServer) -> Vec<(String, i64, String)> {
    sqlx::query("SELECT name, revision, spec_json FROM policies ORDER BY name")
        .fetch_all(&ts.state().db.pool)
        .await
        .unwrap()
        .iter()
        .map(|r| {
            (
                r.try_get("name").unwrap(),
                r.try_get("revision").unwrap(),
                r.try_get("spec_json").unwrap(),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_checks_answer_and_change_nothing() {
    for backend in backends() {
        let mut ts = plain_server(&backend).await;
        seed_everything(&ts, None).await;
        into_demo(&mut ts).await;
        let visitor = Visitor::demo(&ts).await;
        let tables = ["policies", "jobs", "job_events", "snapshots", "agents"];
        let mut before = Vec::new();
        for table in tables {
            before.push(count(&ts, table).await);
        }
        let policies_before = policy_state(&ts).await;
        assert!(!policies_before.is_empty());

        let valid = visitor
            .post("/api/v1/policies/validate", json!({ "yaml": NIGHTLY_YAML }))
            .await;
        assert_eq!(valid.status, 200, "[{}] {}", backend.name(), valid.text);
        assert_eq!(valid.json()["valid"], true);
        let invalid = visitor
            .post("/api/v1/policies/validate", json!({ "yaml": "kind: nope" }))
            .await;
        assert_eq!(invalid.status, 200, "[{}] {}", backend.name(), invalid.text);
        assert_eq!(invalid.json()["valid"], false);
        let preview = visitor
            .post(
                "/api/v1/policies/nightly/retention-preview",
                json!({ "retention": { "keepLast": 1 } }),
            )
            .await;
        assert_eq!(preview.status, 200, "[{}] {}", backend.name(), preview.text);
        assert!(preview.json()["total"].is_number());

        let no_csrf = visitor
            .send(
                Method::POST,
                "/api/v1/policies/validate",
                Some(json!({ "yaml": NIGHTLY_YAML })),
                false,
            )
            .await;
        assert_eq!(no_csrf.code().as_deref(), Some("csrf"), "{}", no_csrf.text);

        for (index, table) in tables.iter().enumerate() {
            assert_eq!(
                count(&ts, table).await,
                before[index],
                "[{}] {table} changed",
                backend.name()
            );
        }
        assert_eq!(policy_state(&ts).await, policies_before);

        for (method, path, body) in [
            (
                Method::POST,
                "/api/v1/policies",
                json!({ "yaml": NIGHTLY_YAML.replace("nightly", "other") }),
            ),
            (
                Method::PUT,
                "/api/v1/policies/nightly",
                json!({ "base_revision": 1, "yaml": NIGHTLY_YAML }),
            ),
            (Method::DELETE, "/api/v1/policies/nightly", json!({})),
            (Method::POST, "/api/v1/policies/nightly/run", json!({})),
            (Method::POST, "/api/v1/policies/apply", json!({})),
        ] {
            let reply = visitor.call(method.clone(), path, Some(body)).await;
            assert_eq!(
                (reply.status, reply.code().as_deref()),
                (403, Some("demo_mode")),
                "[{}] {method} {path}: {}",
                backend.name(),
                reply.text
            );
        }
        assert_eq!(policy_state(&ts).await, policies_before);
        ts.stop().await;
    }
}
