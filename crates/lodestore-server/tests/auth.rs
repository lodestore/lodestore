mod common;

use std::sync::{Arc, Mutex};

use common::{Backend, TestServer, backends};
use data_encoding::BASE64URL_NOPAD;
use lodestore_agent::{NetOptions, connect, tls};
use lodestore_proto::PROTOCOL_VERSION;
use lodestore_proto::agent::v1::enrollment_client::EnrollmentClient;
use lodestore_proto::agent::v1::{EnrollRequest, GetTrustAnchorRequest, Versions};
use reqwest::{Client, Response};
use serde_json::{Value, json};
use sqlx::Row;
use tonic::transport::Channel;

const SETUP_PASSWORD: &str = "a sufficiently long passphrase";

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogBuffer {
    fn take(&self) -> String {
        String::from_utf8_lossy(&std::mem::take(&mut *self.0.lock().unwrap())).into_owned()
    }
}

fn capture_logs() -> LogBuffer {
    let logs = LogBuffer::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("the only test that logs");
    logs
}

fn setup_lines(log: &str) -> Vec<&str> {
    log.lines()
        .filter(|l| l.contains("/setup?token="))
        .collect()
}

fn setup_token(line: &str) -> String {
    line.split("token=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("a token in the setup line")
        .to_string()
}

fn cookie_of(response: &Response) -> String {
    response
        .headers()
        .get("set-cookie")
        .expect("a session cookie")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

async fn post(client: &Client, base: &str, path: &str, body: Value) -> Response {
    client
        .post(format!("{base}{path}"))
        .json(&body)
        .send()
        .await
        .expect("POST request")
}

async fn get(client: &Client, base: &str, path: &str, cookie: Option<&str>) -> Response {
    let mut request = client.get(format!("{base}{path}"));
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    request.send().await.expect("GET request")
}

async fn error_code(response: Response) -> String {
    let body: Value = response.json().await.expect("an error body");
    body["code"].as_str().expect("an error code").to_string()
}

async fn start_without_admin(backend: &Backend) -> TestServer {
    TestServer::start_with(backend, |c| c.admin_password = None).await
}

async fn creates_admin(backend: &Backend, logs: &LogBuffer) {
    let name = backend.name();
    logs.take();
    let mut ts = start_without_admin(backend).await;
    let base = ts.rest_url();
    let client = Client::new();

    let log = logs.take();
    let lines = setup_lines(&log);
    assert_eq!(lines.len(), 1, "[{name}] exactly one setup link: {log}");
    assert!(lines[0].contains(&format!("{base}/setup?token=")));
    let token = setup_token(lines[0]);

    let hosts = get(&client, &base, "/api/v1/hosts", None).await;
    assert_eq!(hosts.status(), 409, "[{name}]");
    assert_eq!(error_code(hosts).await, "setup_required");

    let status: Value = get(&client, &base, "/api/v1/server/status", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(status["setup_required"], true);

    let verify = |token: String| {
        let (client, base) = (client.clone(), base.clone());
        async move {
            post(
                &client,
                &base,
                "/api/v1/setup/verify",
                json!({ "token": token }),
            )
            .await
        }
    };
    assert_eq!(verify(token.clone()).await.status(), 204);
    assert_eq!(verify("not-the-token".to_string()).await.status(), 404);

    let short = post(
        &client,
        &base,
        "/api/v1/setup",
        json!({ "token": token, "username": "root", "password": "elevenchars" }),
    )
    .await;
    assert_eq!(short.status(), 422, "[{name}]");
    assert_eq!(error_code(short).await, "validation");

    let created = post(
        &client,
        &base,
        "/api/v1/setup",
        json!({ "token": token, "username": "root", "password": SETUP_PASSWORD }),
    )
    .await;
    assert_eq!(created.status(), 204, "[{name}]");
    let cookie = cookie_of(&created);
    let session: Value = get(&client, &base, "/api/v1/auth/session", Some(&cookie))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(session["username"], "root");

    let again = post(
        &client,
        &base,
        "/api/v1/setup",
        json!({ "token": token, "username": "second", "password": SETUP_PASSWORD }),
    )
    .await;
    assert_eq!(again.status(), 409, "[{name}]");
    assert_eq!(error_code(again).await, "conflict");

    let status: Value = get(&client, &base, "/api/v1/server/status", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(status["setup_required"], false);

    logs.take();
    ts.restart().await;
    let log = logs.take();
    assert!(
        setup_lines(&log).is_empty(),
        "[{name}] no setup link after the administrator exists"
    );
    let relogin = post(
        &client,
        &base,
        "/api/v1/auth/login",
        json!({ "username": "root", "password": SETUP_PASSWORD }),
    )
    .await;
    assert_eq!(relogin.status(), 204, "[{name}]");
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn setup_link_creates_admin() {
    let logs = capture_logs();
    for backend in backends() {
        creates_admin(&backend, &logs).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn setup_not_logged_when_env_admin() {
    let logs = capture_logs();
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let base = ts.rest_url();
        let log = logs.take();
        assert!(
            log.contains("lodestore server started"),
            "the log capture saw nothing"
        );
        assert!(setup_lines(&log).is_empty(), "[{}]", backend.name());

        let client = Client::new();
        let response = post(
            &client,
            &base,
            "/api/v1/setup",
            json!({ "token": "anything", "username": "x", "password": SETUP_PASSWORD }),
        )
        .await;
        assert_eq!(response.status(), 409);
        let status: Value = get(&client, &base, "/api/v1/server/status", None)
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(status["setup_required"], false);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn setup_login_before_setup_is_refused() {
    for backend in backends() {
        let ts = start_without_admin(&backend).await;
        let login = post(
            &Client::new(),
            &ts.rest_url(),
            "/api/v1/auth/login",
            json!({ "username": "root", "password": SETUP_PASSWORD }),
        )
        .await;
        assert_eq!(login.status(), 409, "[{}]", backend.name());
        assert_eq!(error_code(login).await, "setup_required");
        ts.stop().await;
    }
}

async fn sign_in(client: &Client, base: &str) -> (String, String) {
    let response = post(
        client,
        base,
        "/api/v1/auth/login",
        json!({ "username": "admin", "password": common::ADMIN_PASSWORD }),
    )
    .await;
    assert_eq!(response.status(), 204, "login failed");
    let cookie = cookie_of(&response);
    let csrf = csrf_of(client, base, &cookie).await;
    (cookie, csrf)
}

async fn csrf_of(client: &Client, base: &str, cookie: &str) -> String {
    let session: Value = get(client, base, "/api/v1/auth/session", Some(cookie))
        .await
        .json()
        .await
        .unwrap();
    session["csrf_token"].as_str().unwrap().to_string()
}

async fn post_as(
    client: &Client,
    base: &str,
    path: &str,
    cookie: &str,
    csrf: Option<&str>,
    body: Value,
) -> Response {
    let mut request = client
        .post(format!("{base}{path}"))
        .header("cookie", cookie)
        .json(&body);
    if let Some(csrf) = csrf {
        request = request.header("x-csrf-token", csrf);
    }
    request.send().await.expect("POST request")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_login_rate_limited() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let (client, base) = (Client::new(), ts.rest_url());
        let wrong = json!({ "username": "admin", "password": "wrong password" });
        for attempt in 1..=5 {
            let response = post(&client, &base, "/api/v1/auth/login", wrong.clone()).await;
            assert_eq!(
                response.status(),
                401,
                "[{}] attempt {attempt}",
                backend.name()
            );
        }
        let limited = post(&client, &base, "/api/v1/auth/login", wrong).await;
        assert_eq!(limited.status(), 429, "[{}]", backend.name());
        let retry: u64 = limited
            .headers()
            .get("retry-after")
            .expect("a Retry-After header")
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!((1..=60).contains(&retry), "Retry-After was {retry}");
        assert_eq!(error_code(limited).await, "rate_limited");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_login_failures_lock_out_only_their_own_client_and_user() {
    for backend in backends() {
        let name = backend.name();
        let ts = TestServer::start(&backend).await;
        let (client, base) = (Client::new(), ts.rest_url());
        let ghost = json!({ "username": "ghost", "password": "wrong password" });
        for _ in 0..5 {
            let response = post(&client, &base, "/api/v1/auth/login", ghost.clone()).await;
            assert_eq!(response.status(), 401, "[{name}]");
        }
        let limited = post(&client, &base, "/api/v1/auth/login", ghost).await;
        assert_eq!(limited.status(), 429, "[{name}]");
        let admin = json!({ "username": "admin", "password": common::ADMIN_PASSWORD });
        let ok = post(&client, &base, "/api/v1/auth/login", admin).await;
        assert_eq!(
            ok.status(),
            204,
            "[{name}] a bad username must not block the administrator"
        );
        ts.stop().await;

        let ts = TestServer::start_with(&backend, |c| {
            c.trusted_proxies = vec!["127.0.0.0/8".to_string()];
        })
        .await;
        let base = ts.rest_url();
        let forwarded = |ip: &'static str, password: &'static str| {
            client
                .post(format!("{base}/api/v1/auth/login"))
                .header("x-forwarded-for", ip)
                .json(&json!({ "username": "admin", "password": password }))
                .send()
        };
        for _ in 0..5 {
            let response = forwarded("198.51.100.1", "wrong password").await.unwrap();
            assert_eq!(response.status(), 401, "[{name}]");
        }
        let limited = forwarded("198.51.100.1", "wrong password").await.unwrap();
        assert_eq!(limited.status(), 429, "[{name}]");
        let other = forwarded("203.0.113.5", common::ADMIN_PASSWORD)
            .await
            .unwrap();
        assert_eq!(
            other.status(),
            204,
            "[{name}] a client behind the proxy must have its own limit"
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_sessions_are_purged_and_capped() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let (client, base) = (Client::new(), ts.rest_url());
        let (cookie, _) = sign_in(&client, &base).await;
        let pool = &ts.state().db.pool;
        let day_ms: i64 = 86_400_000;
        let now = lodestore_server::db::now_ms();

        sqlx::query(
            "INSERT INTO sessions (id_hash, user_id, csrf_token, created_at, expires_at, last_seen_at) \
             SELECT $1, user_id, 'x', 1, 2, 1 FROM sessions",
        )
        .bind(vec![9u8; 32])
        .execute(pool)
        .await
        .unwrap();
        let removed = lodestore_server::auth::purge_expired_sessions(ts.state())
            .await
            .unwrap();
        assert_eq!(removed, 1, "[{}]", backend.name());
        let left: i64 = sqlx::query("SELECT COUNT(*) AS n FROM sessions")
            .fetch_one(pool)
            .await
            .unwrap()
            .try_get("n")
            .unwrap();
        assert_eq!(left, 1);

        let created = now - 29 * day_ms - day_ms / 2;
        sqlx::query("UPDATE sessions SET created_at = $1, last_seen_at = $2, expires_at = $3")
            .bind(created)
            .bind(now - 2 * 3_600_000)
            .bind(now + day_ms)
            .execute(pool)
            .await
            .unwrap();
        let active = get(&client, &base, "/api/v1/auth/session", Some(&cookie)).await;
        assert_eq!(active.status(), 200);
        let expires: i64 = sqlx::query("SELECT expires_at FROM sessions")
            .fetch_one(pool)
            .await
            .unwrap()
            .try_get("expires_at")
            .unwrap();
        assert_eq!(
            expires,
            created + 30 * day_ms,
            "[{}] a session may not outlive its absolute limit",
            backend.name()
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn api_sets_security_headers() {
    let ts = TestServer::start(&Backend::Sqlite).await;
    let response = get(&Client::new(), &ts.rest_url(), "/api/v1/auth/session", None).await;
    let headers = response.headers();
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["x-frame-options"], "DENY");
    assert_eq!(headers["referrer-policy"], "no-referrer");
    assert_eq!(headers["cache-control"], "no-store");
    assert!(
        headers["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'")
    );
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_logout_ends_session() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let (client, base) = (Client::new(), ts.rest_url());
        let (cookie, csrf) = sign_in(&client, &base).await;

        let response = post_as(
            &client,
            &base,
            "/api/v1/auth/logout",
            &cookie,
            Some(&csrf),
            json!({}),
        )
        .await;
        assert_eq!(response.status(), 204, "[{}]", backend.name());
        let cleared = response
            .headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(cleared.contains("Max-Age=0"), "{cleared}");

        let after = get(&client, &base, "/api/v1/auth/session", Some(&cookie)).await;
        assert_eq!(after.status(), 401, "[{}]", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_session_expired() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let (client, base) = (Client::new(), ts.rest_url());
        let (cookie, _) = sign_in(&client, &base).await;
        let before = get(&client, &base, "/api/v1/auth/session", Some(&cookie)).await;
        assert_eq!(before.status(), 200);

        sqlx::query("UPDATE sessions SET expires_at = 1")
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        let after = get(&client, &base, "/api/v1/auth/session", Some(&cookie)).await;
        assert_eq!(after.status(), 401, "[{}]", backend.name());
        assert_eq!(error_code(after).await, "unauthorized");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_session_survives_restart() {
    for backend in backends() {
        let mut ts = TestServer::start(&backend).await;
        let (client, base) = (Client::new(), ts.rest_url());
        let (cookie, _) = sign_in(&client, &base).await;
        ts.restart().await;
        let after = get(&client, &base, "/api/v1/auth/session", Some(&cookie)).await;
        assert_eq!(after.status(), 200, "[{}]", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_session_slides_with_activity() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let (client, base) = (Client::new(), ts.rest_url());
        let (cookie, _) = sign_in(&client, &base).await;

        let day_ms: i64 = 86_400_000;
        let old = lodestore_server::db::now_ms() - 2 * 3_600_000;
        sqlx::query("UPDATE sessions SET last_seen_at = $1, expires_at = $2")
            .bind(old)
            .bind(old + day_ms)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        let active = get(&client, &base, "/api/v1/auth/session", Some(&cookie)).await;
        assert_eq!(active.status(), 200);

        let expires: i64 = sqlx::query("SELECT expires_at FROM sessions")
            .fetch_one(&ts.state().db.pool)
            .await
            .unwrap()
            .try_get("expires_at")
            .unwrap();
        assert!(
            expires > old + 6 * day_ms,
            "[{}] expires_at was not extended",
            backend.name()
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_cookie_secure_only_https() {
    for backend in backends() {
        for (public_url, secure) in [
            ("https://lodestore.example", true),
            ("http://lodestore.example", false),
        ] {
            let ts =
                TestServer::start_with(&backend, |c| c.public_url = public_url.to_string()).await;
            let base = format!("http://{}", ts.config.listen);
            let response = post(
                &Client::new(),
                &base,
                "/api/v1/auth/login",
                json!({ "username": "admin", "password": common::ADMIN_PASSWORD }),
            )
            .await;
            assert_eq!(response.status(), 204);
            let header = response
                .headers()
                .get("set-cookie")
                .unwrap()
                .to_str()
                .unwrap();
            assert_eq!(
                header.contains("Secure"),
                secure,
                "[{}] {header}",
                backend.name()
            );
            assert!(header.contains("HttpOnly"), "{header}");
            assert!(header.contains("SameSite=Strict"), "{header}");
            ts.stop().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_bearer_rejected_without_csrf() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let response = Client::new()
            .post(format!("{}/api/v1/join-tokens", ts.rest_url()))
            .header("authorization", "Bearer lodestore-api-token")
            .json(&json!({ "repository_id": "x", "labels": [] }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401, "[{}]", backend.name());
        assert_eq!(error_code(response).await, "unauthorized");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rest_csrf_required() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let (client, base) = (Client::new(), ts.rest_url());
        let repo = ts.create_local_repository("main").await;
        let (cookie, csrf) = sign_in(&client, &base).await;
        let body = json!({ "repository_id": repo, "labels": [] });
        let name = backend.name();

        let path = "/api/v1/join-tokens";
        let missing = post_as(&client, &base, path, &cookie, None, body.clone()).await;
        assert_eq!(missing.status(), 403, "[{name}]");
        assert_eq!(error_code(missing).await, "csrf");

        let foreign = client
            .post(format!("{base}{path}"))
            .header("cookie", &cookie)
            .header("x-csrf-token", &csrf)
            .header("origin", "http://evil.example")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(foreign.status(), 403, "[{name}]");
        assert_eq!(error_code(foreign).await, "csrf");

        let anonymous = post(&client, &base, path, body.clone()).await;
        assert_eq!(anonymous.status(), 401, "[{name}]");
        assert_eq!(error_code(anonymous).await, "unauthorized");

        let read = get(&client, &base, "/api/v1/hosts", Some(&cookie)).await;
        assert_eq!(read.status(), 200, "[{name}]");

        let ok = post_as(&client, &base, path, &cookie, Some(&csrf), body).await;
        assert_eq!(ok.status(), 201, "[{name}]");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rest_csrf_required_for_account_and_channels() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let (client, base) = (Client::new(), ts.rest_url());
        let (cookie, _) = sign_in(&client, &base).await;
        let name = backend.name();

        for (method, path) in [
            ("PUT", "/api/v1/account"),
            ("POST", "/api/v1/account/password"),
            ("POST", "/api/v1/notifications/channels"),
            ("POST", "/api/v1/notifications/channels/test"),
            ("PUT", "/api/v1/notifications/channels/nope"),
            ("DELETE", "/api/v1/notifications/channels/nope"),
        ] {
            let missing = client
                .request(method.parse().unwrap(), format!("{base}{path}"))
                .header("cookie", &cookie)
                .json(&json!({}))
                .send()
                .await
                .unwrap();
            assert_eq!(missing.status(), 403, "{method} {path} [{name}]");
            assert_eq!(error_code(missing).await, "csrf", "{method} {path}");
        }

        let anonymous = get(&client, &base, "/api/v1/account", None).await;
        assert_eq!(anonymous.status(), 401, "[{name}]");
        assert_eq!(error_code(anonymous).await, "unauthorized");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rest_csrf_origin_follows_public_url_behind_a_proxy() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| {
            c.public_url = "https://lodestore.example.com".to_string();
        })
        .await;
        let (client, base) = (Client::new(), format!("http://{}", ts.config.listen));
        let repo = ts.create_local_repository("main").await;
        let (cookie, csrf) = sign_in(&client, &base).await;
        let send = |origin: &'static str| {
            client
                .post(format!("{base}/api/v1/join-tokens"))
                .header("cookie", &cookie)
                .header("x-csrf-token", &csrf)
                .header("origin", origin)
                .json(&json!({ "repository_id": repo, "labels": [] }))
                .send()
        };
        let name = backend.name();
        for (origin, status) in [
            ("https://lodestore.example.com", 201),
            ("https://LodeStore.example.com:443", 201),
            ("https://evil.example", 403),
        ] {
            assert_eq!(
                send(origin).await.unwrap().status(),
                status,
                "[{name}] {origin}"
            );
        }
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_no_secrets_in_logs() {
    let logs = capture_logs();
    let mut secrets: Vec<String> = vec![SETUP_PASSWORD.to_string(), "07".repeat(32)];
    for backend in backends() {
        logs.take();
        let ts = start_without_admin(&backend).await;
        let (client, base) = (Client::new(), ts.rest_url());
        let log = logs.take();
        let token = setup_token(setup_lines(&log)[0]);
        let created = post(
            &client,
            &base,
            "/api/v1/setup",
            json!({ "token": token, "username": "root", "password": SETUP_PASSWORD }),
        )
        .await;
        assert_eq!(created.status(), 204);
        let setup_cookie = cookie_of(&created);

        let relogin = post(
            &client,
            &base,
            "/api/v1/auth/login",
            json!({ "username": "root", "password": SETUP_PASSWORD }),
        )
        .await;
        assert_eq!(relogin.status(), 204);
        let cookie = cookie_of(&relogin);
        let csrf = csrf_of(&client, &base, &cookie).await;

        let repo = ts.create_local_repository("main").await;
        let issued = post_as(
            &client,
            &base,
            "/api/v1/join-tokens",
            &cookie,
            Some(&csrf),
            json!({ "repository_id": repo, "labels": [] }),
        )
        .await;
        assert_eq!(issued.status(), 201);
        let issued: Value = issued.json().await.unwrap();

        secrets.push(setup_cookie.split_once('=').unwrap().1.to_string());
        secrets.push(cookie.split_once('=').unwrap().1.to_string());
        secrets.push(csrf);
        secrets.push(issued["token"].as_str().unwrap().to_string());
        ts.stop().await;
        let captured = logs.take();
        assert!(captured.contains("admin account created"));
        for secret in &secrets {
            assert!(
                !captured.contains(secret.as_str()),
                "[{}] a secret reached the logs",
                backend.name()
            );
        }
    }
}

fn csr_der() -> Vec<u8> {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    rcgen::CertificateParams::default()
        .serialize_request(&key)
        .unwrap()
        .der()
        .to_vec()
}

async fn enrollment_client(ts: &TestServer) -> EnrollmentClient<Channel> {
    let address = ts.grpc_address();
    let bootstrap = connect::channel(
        &address,
        "127.0.0.1",
        tls::bootstrap_config().unwrap(),
        &NetOptions::default(),
    )
    .await
    .expect("bootstrap channel");
    let ca_pem = EnrollmentClient::new(bootstrap)
        .get_trust_anchor(GetTrustAnchorRequest {})
        .await
        .expect("GetTrustAnchor")
        .into_inner()
        .ca_pem;
    let channel = connect::channel(
        &address,
        "127.0.0.1",
        tls::pinned_config(&ca_pem, None).unwrap(),
        &NetOptions::default(),
    )
    .await
    .expect("pinned channel");
    EnrollmentClient::new(channel)
}

fn enroll_request(secret: String, versions: Versions) -> EnrollRequest {
    EnrollRequest {
        join_secret: secret,
        csr_der: csr_der(),
        hostname: "limited".to_string(),
        os: "linux".to_string(),
        arch: "x86_64".to_string(),
        agent_version: "test".to_string(),
        versions: Some(versions),
        ..Default::default()
    }
}

fn secret_of(token: &str) -> String {
    let body = token.strip_prefix("lode1.").unwrap();
    let payload: Value =
        serde_json::from_slice(&BASE64URL_NOPAD.decode(body.as_bytes()).unwrap()).unwrap();
    payload["s"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enroll_rate_limited() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let mut client = enrollment_client(&ts).await;
        let bogus = BASE64URL_NOPAD.encode(&[1u8; 32]);
        for attempt in 1..=20 {
            let status = client
                .enroll(enroll_request(
                    bogus.clone(),
                    Versions {
                        min: PROTOCOL_VERSION,
                        max: PROTOCOL_VERSION,
                    },
                ))
                .await
                .expect_err("a bogus token is refused");
            assert_eq!(
                status.code(),
                tonic::Code::PermissionDenied,
                "[{}] attempt {attempt}",
                backend.name()
            );
        }
        let status = client
            .enroll(enroll_request(
                bogus,
                Versions {
                    min: PROTOCOL_VERSION,
                    max: PROTOCOL_VERSION,
                },
            ))
            .await
            .expect_err("the 21st attempt is limited");
        assert_eq!(status.code(), tonic::Code::ResourceExhausted);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enroll_version_mismatch() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let secret = secret_of(&api.join_token(&repo, json!([])).await);
        let mut client = enrollment_client(&ts).await;

        let status = client
            .enroll(enroll_request(secret.clone(), Versions { min: 3, max: 4 }))
            .await
            .expect_err("no common version");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert_eq!(
            status.metadata().get("lodestore-error").unwrap(),
            "version_unsupported"
        );

        let status = client
            .enroll(enroll_request(secret.clone(), Versions { min: 1, max: 1 }))
            .await
            .expect_err("a version 1 client is refused");
        assert_eq!(
            status.metadata().get("lodestore-error").unwrap(),
            "version_unsupported"
        );

        let response = client
            .enroll(enroll_request(
                secret,
                Versions {
                    min: PROTOCOL_VERSION,
                    max: PROTOCOL_VERSION,
                },
            ))
            .await
            .expect("the token was not consumed by the refused attempt");
        assert!(!response.into_inner().agent_id.is_empty());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_body_gets_a_json_error() {
    let backend = &backends()[0];
    let ts = TestServer::start(backend).await;
    let client = reqwest::Client::new();
    let url = format!("{}/api/v1/auth/login", ts.rest_url());

    let response = client
        .post(&url)
        .header("content-type", "application/json")
        .body("{bad")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["code"], "bad_request");
    assert!(!body["message"].as_str().unwrap().is_empty());

    let response = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(response.status(), 415);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["code"], "unsupported_media_type");
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn login_rejects_oversized_password_without_counting() {
    for backend in backends() {
        let name = backend.name();
        let ts = TestServer::start(&backend).await;
        let (client, base) = (Client::new(), ts.rest_url());
        let huge = json!({ "username": "admin", "password": "a".repeat(4097) });
        for _ in 0..6 {
            let response = post(&client, &base, "/api/v1/auth/login", huge.clone()).await;
            assert_eq!(response.status(), 401, "[{name}]");
        }
        let admin = json!({ "username": "admin", "password": common::ADMIN_PASSWORD });
        let ok = post(&client, &base, "/api/v1/auth/login", admin).await;
        assert_eq!(ok.status(), 204, "[{name}] oversized tries are not counted");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn login_accepts_existing_long_utf8_password() {
    for backend in backends() {
        let name = backend.name();
        let ts = TestServer::start(&backend).await;
        let (client, base) = (Client::new(), ts.rest_url());
        let long = "ж".repeat(1000);
        let hash = lodestore_server::auth::hash_password(&long).unwrap();
        sqlx::query("UPDATE admin_users SET password_hash = $1")
            .bind(hash)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        let body = json!({ "username": "admin", "password": long });
        let ok = post(&client, &base, "/api/v1/auth/login", body).await;
        assert_eq!(ok.status(), 204, "[{name}]");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn busy_login_returns_503_and_does_not_spend_the_attempt() {
    for backend in backends() {
        let name = backend.name();
        let ts = TestServer::start(&backend).await;
        let (client, base) = (Client::new(), ts.rest_url());
        let gate = ts.state().auth.argon.clone();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let held = Arc::new(Mutex::new(held));
        let mut holders = Vec::new();
        for _ in 0..gate.available() {
            let (gate, held) = (gate.clone(), held.clone());
            holders.push(tokio::spawn(async move {
                gate.run(move || {
                    let _ = held.lock().unwrap().recv();
                })
                .await
            }));
        }
        while gate.available() > 0 {
            tokio::task::yield_now().await;
        }

        let admin = json!({ "username": "admin", "password": common::ADMIN_PASSWORD });
        for _ in 0..6 {
            let busy = post(&client, &base, "/api/v1/auth/login", admin.clone()).await;
            assert_eq!(busy.status(), 503, "[{name}]");
            assert_eq!(busy.headers()["retry-after"], "2");
            assert_eq!(error_code(busy).await, "busy");
        }
        drop(release);
        for holder in holders {
            holder.await.unwrap().unwrap();
        }
        let ok = post(&client, &base, "/api/v1/auth/login", admin).await;
        assert_eq!(ok.status(), 204, "[{name}] a busy answer is not an attempt");
        ts.stop().await;
    }
}

async fn login_with_cookie(client: &Client, base: &str, cookie: &str, password: &str) -> Response {
    client
        .post(format!("{base}/api/v1/auth/login"))
        .header("cookie", cookie)
        .json(&json!({ "username": "admin", "password": password }))
        .send()
        .await
        .expect("login request")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn five_failed_logins_carrying_a_session_end_it() {
    for backend in backends() {
        let name = backend.name();
        let ts = TestServer::start(&backend).await;
        let (client, base) = (Client::new(), ts.rest_url());
        let (cookie, _) = sign_in(&client, &base).await;
        for attempt in 1..=5 {
            let wrong = login_with_cookie(&client, &base, &cookie, "wrong password").await;
            assert_eq!(wrong.status(), 401, "attempt {attempt} [{name}]");
        }
        let after = get(&client, &base, "/api/v1/auth/session", Some(&cookie)).await;
        assert_eq!(after.status(), 401, "[{name}]");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_correct_login_carrying_a_session_resets_its_failure_count() {
    for backend in backends() {
        let name = backend.name();
        let ts = TestServer::start(&backend).await;
        let (client, base) = (Client::new(), ts.rest_url());
        let (cookie, _) = sign_in(&client, &base).await;
        for _ in 0..3 {
            let wrong = login_with_cookie(&client, &base, &cookie, "wrong password").await;
            assert_eq!(wrong.status(), 401, "[{name}]");
        }
        let ok = login_with_cookie(&client, &base, &cookie, common::ADMIN_PASSWORD).await;
        assert_eq!(ok.status(), 204, "[{name}]");
        for _ in 0..2 {
            let wrong = login_with_cookie(&client, &base, &cookie, "wrong password").await;
            assert_eq!(wrong.status(), 401, "[{name}]");
        }
        let after = get(&client, &base, "/api/v1/auth/session", Some(&cookie)).await;
        assert_eq!(after.status(), 200, "[{name}]");
        ts.stop().await;
    }
}
