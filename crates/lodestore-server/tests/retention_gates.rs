mod common;

use common::{Api, TestServer, backends, bearer_client};
use lodestore_agent::{EnrollArgs, NetOptions};
use lodestore_server::db::now_ms;
use serde_json::{Value, json};

const DAILY: &str = "0 3 * * *";
const SCHEDULE_REFUSED: &str =
    "The token needs the snapshots:delete scope to run a policy with retention more often.";

fn yaml(cron: Option<&str>, retention: bool, path: &str) -> String {
    let schedule = cron.map_or(String::new(), |c| {
        format!("  schedule:\n    cron: \"{c}\"\n    jitter: 0m\n")
    });
    let retention = if retention {
        "  retention:\n    keepLast: 3\n"
    } else {
        ""
    };
    format!(
        "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: db\n\
         spec:\n  repository: main\n  selector:\n    matchLabels:\n      role: db\n  \
         paths:\n    - {path}\n{schedule}{retention}"
    )
}

struct Session<'a> {
    api: Api,
    ts: &'a TestServer,
}

impl Session<'_> {
    async fn send(&self, method: reqwest::Method, path: &str, body: &Value) -> (u16, Value) {
        let response = reqwest::Client::new()
            .request(method, format!("{}{path}", self.ts.rest_url()))
            .header("cookie", self.api.cookie())
            .header("x-csrf-token", &self.api.csrf)
            .json(body)
            .send()
            .await
            .unwrap();
        (
            response.status().as_u16(),
            response.json().await.unwrap_or(Value::Null),
        )
    }

    async fn post(&self, path: &str) -> (u16, Value) {
        self.send(reqwest::Method::POST, path, &json!({})).await
    }

    async fn create(&self, yaml: String) -> Value {
        let (status, body) = self
            .send(
                reqwest::Method::POST,
                "/api/v1/policies",
                &json!({ "yaml": yaml }),
            )
            .await;
        assert_eq!(status, 201, "{body}");
        body
    }

    async fn enroll(&self, repo: &str) -> String {
        let token = self
            .api
            .join_token(repo, json!([{ "key": "role", "value": "db" }]))
            .await;
        lodestore_agent::enroll(EnrollArgs {
            token,
            paths: self.ts.agent_paths("host-a"),
            hostname: Some("host-a".to_string()),
            net: NetOptions::default(),
        })
        .await
        .expect("enroll")
        .agent_id
    }
}

struct Token {
    client: reqwest::Client,
    base: String,
}

impl Token {
    async fn new(ts: &TestServer, scopes: &[&str]) -> Token {
        Token {
            client: bearer_client(&ts.issue_token(scopes).await),
            base: ts.rest_url(),
        }
    }

    async fn raw(&self, method: reqwest::Method, path: &str, body: &Value) -> reqwest::Response {
        self.client
            .request(method, format!("{}{path}", self.base))
            .json(body)
            .send()
            .await
            .unwrap()
    }

    async fn send(&self, method: reqwest::Method, path: &str, body: &Value) -> (u16, Value) {
        let response = self.raw(method, path, body).await;
        (
            response.status().as_u16(),
            response.json().await.unwrap_or(Value::Null),
        )
    }

    async fn post(&self, path: &str) -> (u16, Value) {
        self.send(reqwest::Method::POST, path, &json!({})).await
    }

    async fn update(&self, yaml: String, revision: i64) -> (u16, Value) {
        self.send(
            reqwest::Method::PUT,
            "/api/v1/policies/db",
            &json!({ "yaml": yaml, "base_revision": revision }),
        )
        .await
    }
}

async fn settle_jobs(ts: &TestServer) {
    sqlx::query(
        "UPDATE jobs SET state = 'succeeded', finished_at = $1 \
         WHERE state IN ('queued', 'assigned', 'running')",
    )
    .bind(now_ms())
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
}

async fn run_now(token: &Token, ts: &TestServer) -> (u16, Value) {
    let got = token.post("/api/v1/policies/db/run").await;
    settle_jobs(ts).await;
    got
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_gates_run_now_limit_retry_after() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let repo = ts.create_local_repository("main").await;
        let session = Session {
            api: ts.api().await,
            ts: &ts,
        };
        session.enroll(&repo).await;
        session.create(yaml(None, true, "/etc")).await;
        let token = Token::new(&ts, &["jobs:run"]).await;
        let what = backend.name();

        for n in 1..=6 {
            let (status, body) = run_now(&token, &ts).await;
            assert_eq!(status, 200, "[{what}] run {n}: {body}");
            assert_eq!(body["jobs"][0]["created"], true, "[{what}] run {n}");
        }
        let response = token
            .raw(reqwest::Method::POST, "/api/v1/policies/db/run", &json!({}))
            .await;
        assert_eq!(response.status(), 429, "[{what}]");
        let retry_after: u64 = response
            .headers()
            .get("retry-after")
            .expect("Retry-After")
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!((1..=3600).contains(&retry_after), "[{what}] {retry_after}");
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["code"], "rate_limited", "[{what}]");
        assert!(
            body["message"].as_str().unwrap().contains("host-a"),
            "{body}"
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_gates_run_now_limit_covers_host_and_rerun() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let repo = ts.create_local_repository("main").await;
        let session = Session {
            api: ts.api().await,
            ts: &ts,
        };
        let host = session.enroll(&repo).await;
        session.create(yaml(None, true, "/etc")).await;
        let token = Token::new(&ts, &["jobs:run"]).await;
        let what = backend.name();

        let mut last = String::new();
        for n in 1..=6 {
            let (status, body) = run_now(&token, &ts).await;
            assert_eq!(status, 200, "[{what}] run {n}: {body}");
            last = body["jobs"][0]["id"].as_str().unwrap().to_string();
        }
        let (status, body) = token.post(&format!("/api/v1/hosts/{host}/run")).await;
        assert_eq!(
            (status, &body["code"]),
            (429, &json!("rate_limited")),
            "[{what}]"
        );
        let (status, body) = token.post(&format!("/api/v1/jobs/{last}/rerun")).await;
        assert_eq!(
            (status, &body["code"]),
            (429, &json!("rate_limited")),
            "[{what}]"
        );
        let (status, _) = session.post("/api/v1/policies/db/run").await;
        assert_eq!(status, 200, "[{what}] a session is not limited");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_gates_session_unlimited() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let repo = ts.create_local_repository("main").await;
        let session = Session {
            api: ts.api().await,
            ts: &ts,
        };
        session.enroll(&repo).await;
        session.create(yaml(None, true, "/etc")).await;
        for n in 1..=8 {
            let (status, body) = session.post("/api/v1/policies/db/run").await;
            settle_jobs(&ts).await;
            assert_eq!(status, 200, "[{}] run {n}: {body}", backend.name());
        }
        ts.stop().await;
    }
}

fn assert_schedule_refused(got: &(u16, Value), what: &str) {
    assert_eq!(got.0, 403, "{what}: {}", got.1);
    assert_eq!(got.1["code"], "forbidden", "{what}");
    assert_eq!(got.1["message"], SCHEDULE_REFUSED, "{what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_gates_faster_cron_needs_snapshots_delete() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        ts.create_local_repository("main").await;
        let session = Session {
            api: ts.api().await,
            ts: &ts,
        };
        session.create(yaml(Some(DAILY), true, "/etc")).await;
        let plain = Token::new(&ts, &["policies:read", "policies:write"]).await;
        let full = Token::new(
            &ts,
            &["policies:read", "policies:write", "snapshots:delete"],
        )
        .await;
        let what = backend.name();

        let got = plain.update(yaml(Some("* * * * *"), true, "/etc"), 1).await;
        assert_schedule_refused(&got, what);
        let got = plain
            .update(yaml(Some("0 */6 * * *"), true, "/etc"), 1)
            .await;
        assert_schedule_refused(&got, what);
        let got = full
            .update(yaml(Some("0 */6 * * *"), true, "/etc"), 1)
            .await;
        assert_eq!(got.0, 200, "[{what}] {}", got.1);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_gates_month_restricted_cron_needs_snapshots_delete() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        ts.create_local_repository("main").await;
        let session = Session {
            api: ts.api().await,
            ts: &ts,
        };
        session.create(yaml(Some("0 3 * * 0"), true, "/etc")).await;
        let plain = Token::new(&ts, &["policies:read", "policies:write"]).await;
        let got = plain
            .update(yaml(Some("0 3 * 12 *"), true, "/etc"), 1)
            .await;
        assert_schedule_refused(&got, backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_gates_slower_or_shifted_cron_allowed() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        ts.create_local_repository("main").await;
        let session = Session {
            api: ts.api().await,
            ts: &ts,
        };
        session.create(yaml(Some(DAILY), true, "/etc")).await;
        let plain = Token::new(&ts, &["policies:read", "policies:write"]).await;
        let what = backend.name();

        let mut revision = 1;
        for cron in ["30 4 * * *", "0 3 * * 1"] {
            let (status, body) = plain.update(yaml(Some(cron), true, "/etc"), revision).await;
            assert_eq!(status, 200, "[{what}] {cron}: {body}");
            revision = body["revision"].as_i64().unwrap();
        }
        let (status, body) = plain
            .update(yaml(Some("0 3 * * 1"), true, "/srv"), revision)
            .await;
        assert_eq!(status, 200, "[{what}] unchanged schedule: {body}");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_gates_cron_without_retention_allowed() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        ts.create_local_repository("main").await;
        let session = Session {
            api: ts.api().await,
            ts: &ts,
        };
        session.create(yaml(Some(DAILY), false, "/etc")).await;
        let plain = Token::new(&ts, &["policies:read", "policies:write"]).await;
        let (status, body) = plain
            .update(yaml(Some("* * * * *"), false, "/etc"), 1)
            .await;
        assert_eq!(status, 200, "[{}] {body}", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_gates_apply_faster_cron_dry_run() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        ts.create_local_repository("main").await;
        let session = Session {
            api: ts.api().await,
            ts: &ts,
        };
        session.create(yaml(Some(DAILY), true, "/etc")).await;
        let plain = Token::new(&ts, &["policies:read", "policies:write"]).await;
        let apply = |cron: &str, dry_run: bool| {
            json!({
                "documents": [{ "source": "p.yaml", "yaml": yaml(Some(cron), true, "/etc") }],
                "adopt": true,
                "dry_run": dry_run,
            })
        };
        for dry_run in [true, false] {
            let got = plain
                .send(
                    reqwest::Method::POST,
                    "/api/v1/policies/apply",
                    &apply("* * * * *", dry_run),
                )
                .await;
            assert_schedule_refused(&got, &format!("dry_run={dry_run} [{}]", backend.name()));
        }
        let got = plain
            .send(
                reqwest::Method::POST,
                "/api/v1/policies/apply",
                &apply("30 4 * * *", true),
            )
            .await;
        assert_eq!(got.0, 200, "[{}] {}", backend.name(), got.1);
        ts.stop().await;
    }
}
