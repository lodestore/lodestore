mod common;

use common::{Api, TestServer, backends, bearer_client};
use lodestore_agent::{EnrollArgs, NetOptions};
use lodestore_server::db::now_ms;
use lodestore_server::policies::changes_retained_content;
use serde_json::{Value, json};

const DAILY: &str = "0 3 * * *";
const SCHEDULE_REFUSED: &str =
    "The token needs the snapshots:delete scope to run a policy with retention more often.";
const CONTENT_REFUSED: &str = "The token needs the snapshots:delete scope to change what or where a policy with retention backs up.";
const RETENTION_REFUSED: &str =
    "The token needs the snapshots:delete scope to set or change retention.";

const BODIES: [(char, &str); 13] = [
    ('a', "  repository: main\n  paths: [/etc]\n"),
    ('b', "  repository: main\n  paths: [/etc, /srv]\n"),
    ('c', "  repository: main\n  paths: [/srv, /etc]\n"),
    ('d', "  repository: main\n  paths: [/etc/]\n"),
    (
        'e',
        "  repository: main\n  paths: [/etc]\n  exclude: [\"*.log\"]\n",
    ),
    (
        'f',
        "  repository: main\n  paths: [/etc]\n  exclude: [\"*.log\", \"*.log\"]\n",
    ),
    (
        'g',
        "  repository: main\n  paths: [/etc]\n  oneFileSystem: true\n",
    ),
    (
        'h',
        "  repository: main\n  paths: [/etc]\n  excludeCaches: true\n",
    ),
    (
        'i',
        "  repository: main\n  paths: [/etc]\n  limits:\n    nice: 10\n",
    ),
    (
        'j',
        "  repository: main\n  paths: [/etc]\n  exclude: [\"*.log\", \"!keep.log\"]\n",
    ),
    (
        'k',
        "  repository: main\n  paths: [/etc]\n  exclude: [\"!keep.log\", \"*.log\"]\n",
    ),
    (
        'l',
        "  repository: main\n  paths: [/etc]\n  exclude: [\"*.log\", \"!keep.log\", \"*.log\"]\n",
    ),
    ('m', "  repository: other\n  paths: [/etc]\n"),
];

fn body(id: char) -> &'static str {
    BODIES.iter().find(|(b, _)| *b == id).unwrap().1
}

fn content_yaml(name: &str, retention: bool, body: &str) -> String {
    let retention = if retention {
        "  retention:\n    keepLast: 3\n"
    } else {
        ""
    };
    format!(
        "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: {name}\n\
         spec:\n{body}{retention}"
    )
}

fn oracle(old: (bool, &str), new: (bool, &str)) -> bool {
    let parse = |(retention, body)| {
        lodestore_policy::parse_yaml(&content_yaml("x", retention, body)).unwrap()
    };
    changes_retained_content(&parse(old), &parse(new))
}

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

    async fn put(&self, name: &str, yaml: String, revision: i64) -> (u16, Value) {
        self.send(
            reqwest::Method::PUT,
            &format!("/api/v1/policies/{name}"),
            &json!({ "yaml": yaml, "base_revision": revision }),
        )
        .await
    }

    async fn apply(&self, yaml: String, dry_run: bool) -> (u16, Value) {
        self.send(
            reqwest::Method::POST,
            "/api/v1/policies/apply",
            &json!({
                "documents": [{ "source": "p.yaml", "yaml": yaml }],
                "adopt": true,
                "dry_run": dry_run,
            }),
        )
        .await
    }

    async fn revision(&self, name: &str) -> Value {
        let (status, body) = self
            .send(
                reqwest::Method::GET,
                &format!("/api/v1/policies/{name}"),
                &Value::Null,
            )
            .await;
        assert_eq!(status, 200, "{name}: {body}");
        body["revision"].clone()
    }
}

/// Two copies of `old`, `pa-{tag}` and `pb-{tag}`, then `new` through apply
/// dry-run on `pb`, PUT on `pa` and apply on `pb`, in that order.
async fn write_paths(
    session: &Session<'_>,
    token: &Token,
    tag: &str,
    old: (bool, &str),
    new: (bool, &str),
) -> [(u16, Value); 3] {
    let (pa, pb) = (format!("pa-{tag}"), format!("pb-{tag}"));
    session.create(content_yaml(&pa, old.0, old.1)).await;
    session.create(content_yaml(&pb, old.0, old.1)).await;
    let dry_run = token.apply(content_yaml(&pb, new.0, new.1), true).await;
    let put = token.put(&pa, content_yaml(&pa, new.0, new.1), 1).await;
    let applied = token.apply(content_yaml(&pb, new.0, new.1), false).await;
    [dry_run, put, applied]
}

async fn assert_all_refused(token: &Token, tag: &str, got: &[(u16, Value); 3], messages: &[&str]) {
    for (path, (status, body)) in ["apply dry-run", "PUT", "apply"].iter().zip(got) {
        assert_eq!(*status, 403, "{tag} {path}: {body}");
        assert_eq!(body["code"], "forbidden", "{tag} {path}");
        let message = body["message"].as_str().unwrap_or_default();
        assert!(messages.contains(&message), "{tag} {path}: {body}");
    }
    for name in [format!("pa-{tag}"), format!("pb-{tag}")] {
        assert_eq!(token.revision(&name).await, 1, "{name}");
    }
}

async fn policy_count(ts: &TestServer) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM policies")
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
}

async fn assert_apply_settled(token: &Token, tag: &str, new: (bool, &str)) {
    let (status, body) = token
        .apply(content_yaml(&format!("pb-{tag}"), new.0, new.1), false)
        .await;
    assert_eq!(status, 200, "{tag} repeated apply: {body}");
    assert_eq!(body["unchanged"], 1, "{tag} repeated apply: {body}");
    assert_eq!(body["changes"], json!([]), "{tag} repeated apply: {body}");
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
        let message = body["message"].as_str().unwrap();
        assert!(
            message.starts_with("This policy was started on host-a 6 times in the last hour."),
            "{body}"
        );
        assert!(!message.contains("token"), "{body}");
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
async fn retention_gates_cron_without_two_runs_in_a_year_counts_as_faster() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        ts.create_local_repository("main").await;
        let session = Session {
            api: ts.api().await,
            ts: &ts,
        };
        session.create(yaml(Some("0 2 * * *"), true, "/etc")).await;
        let plain = Token::new(&ts, &["policies:read", "policies:write"]).await;
        let got = plain
            .update(yaml(Some("* * 29 2 *"), true, "/etc"), 1)
            .await;
        assert_schedule_refused(&got, backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_gates_dense_cron_stops_scanning_and_counts_as_faster() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        ts.create_local_repository("main").await;
        let session = Session {
            api: ts.api().await,
            ts: &ts,
        };
        session
            .create(yaml(Some("*/2 * * * *"), true, "/etc"))
            .await;
        let plain = Token::new(&ts, &["policies:read", "policies:write"]).await;
        let started = std::time::Instant::now();
        let got = plain
            .update(yaml(Some("0-58/2 * * * *"), true, "/etc"), 1)
            .await;
        assert_schedule_refused(&got, backend.name());
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
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
        let relabelled = yaml(Some("0 3 * * 1"), true, "/etc").replace("role: db", "role: web");
        let (status, body) = plain.update(relabelled, revision).await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_gates_content_rule_is_the_same_on_every_path() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        ts.create_local_repository("main").await;
        ts.create_local_repository("other").await;
        let session = Session {
            api: ts.api().await,
            ts: &ts,
        };
        let plain = Token::new(&ts, &["policies:read", "policies:write"]).await;
        let what = backend.name();

        for (i, old) in BODIES {
            for (j, new) in BODIES {
                let tag = format!("{i}-{j}");
                let changed = oracle((true, old), (true, new));
                let got = write_paths(&session, &plain, &tag, (true, old), (true, new)).await;
                let expected = if changed { 403 } else { 200 };
                assert_eq!(
                    got.each_ref().map(|g| g.0),
                    [expected; 3],
                    "[{what}] ({tag}) apply dry-run, PUT, apply: {got:?}"
                );
                if changed {
                    assert_all_refused(&plain, &tag, &got, &[CONTENT_REFUSED]).await;
                } else {
                    assert_apply_settled(&plain, &tag, (true, new)).await;
                }
            }
        }

        for (tag, old, new) in [("drop", true, false), ("add", false, true)] {
            let got = write_paths(&session, &plain, tag, (old, body('a')), (new, body('b'))).await;
            assert_all_refused(&plain, tag, &got, &[RETENTION_REFUSED, CONTENT_REFUSED]).await;
        }
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_gates_content_rule_without_retention_and_on_create() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        ts.create_local_repository("main").await;
        ts.create_local_repository("other").await;
        let session = Session {
            api: ts.api().await,
            ts: &ts,
        };
        let plain = Token::new(&ts, &["policies:read", "policies:write"]).await;
        let what = backend.name();

        for (i, j) in [
            ('a', 'b'),
            ('a', 'd'),
            ('a', 'g'),
            ('a', 'h'),
            ('a', 'm'),
            ('j', 'k'),
        ] {
            let (old, new) = ((false, body(i)), (false, body(j)));
            assert!(!oracle(old, new), "({i}0,{j}0)");
            let tag = format!("{i}0-{j}0");
            let got = write_paths(&session, &plain, &tag, old, new).await;
            assert_eq!(
                got.each_ref().map(|g| g.0),
                [200; 3],
                "[{what}] ({tag}) apply dry-run, PUT, apply: {got:?}"
            );
        }

        let before = policy_count(&ts).await;
        let created = plain
            .send(
                reqwest::Method::POST,
                "/api/v1/policies",
                &json!({ "yaml": content_yaml("fresh", true, body('a')) }),
            )
            .await;
        let applied = plain
            .apply(content_yaml("fresh", true, body('a')), false)
            .await;
        for (path, (status, body)) in [("POST", &created), ("apply", &applied)] {
            assert_eq!(*status, 403, "[{what}] create via {path}: {body}");
            assert_eq!(body["message"], RETENTION_REFUSED, "[{what}] {path}");
        }
        assert_eq!(policy_count(&ts).await, before, "[{what}]");

        session.create(content_yaml("stale", true, body('a'))).await;
        let (status, body) = plain
            .put("stale", content_yaml("stale", true, body('b')), 7)
            .await;
        assert_eq!(status, 409, "[{what}] {body}");
        assert_eq!(body["code"], "policy_conflict", "[{what}]");
        ts.stop().await;
    }
}
