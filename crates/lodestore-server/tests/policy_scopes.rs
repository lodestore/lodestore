mod common;

use common::{Api, TestServer, backends, bearer_client};
use lodestore_agent::{EnrollArgs, NetOptions};
use serde_json::{Value, json};

const DUMP: &str = "  hooks:\n    before:\n      - script: dump-db\n";
const DUMP_ARGS: &str =
    "  hooks:\n    before:\n      - script: dump-db\n        args: [\"--all\"]\n";
const AFTER_ONLY: &str = "  hooks:\n    after:\n      - script: notify\n";

fn needs(names: &str) -> String {
    format!(
        "The token needs the hooks:write scope to change hooks or the hosts they run on (policies: {names})."
    )
}

fn yaml(name: &str, repo: &str, label: &str, extra: &str) -> String {
    format!(
        "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: {name}\n\
         spec:\n  repository: {repo}\n  selector:\n    matchLabels:\n      role: {label}\n  \
         paths:\n    - /etc\n{extra}"
    )
}

struct Session<'a> {
    api: Api,
    ts: &'a TestServer,
}

impl Session<'_> {
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> (u16, Value) {
        let mut request = reqwest::Client::new()
            .request(method, format!("{}{path}", self.ts.rest_url()))
            .header("cookie", self.api.cookie())
            .header("x-csrf-token", &self.api.csrf);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.unwrap();
        (
            response.status().as_u16(),
            response.json().await.unwrap_or(Value::Null),
        )
    }

    async fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        self.send(reqwest::Method::POST, path, Some(body)).await
    }

    async fn put(&self, path: &str, body: &Value) -> (u16, Value) {
        self.send(reqwest::Method::PUT, path, Some(body)).await
    }

    async fn create(&self, yaml: String) -> Value {
        let (status, body) = self
            .post("/api/v1/policies", &json!({ "yaml": yaml }))
            .await;
        assert_eq!(status, 201, "{body}");
        body
    }

    async fn enroll(&self, repo: &str, hostname: &str, role: &str) -> String {
        let labels = if role.is_empty() {
            json!([])
        } else {
            json!([{ "key": "role", "value": role }])
        };
        let token = self.api.join_token(repo, labels).await;
        lodestore_agent::enroll(EnrollArgs {
            token,
            paths: self.ts.agent_paths(hostname),
            hostname: Some(hostname.to_string()),
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

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> (u16, Value) {
        let mut request = self.client.request(method, format!("{}{path}", self.base));
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.unwrap();
        (
            response.status().as_u16(),
            response.json().await.unwrap_or(Value::Null),
        )
    }

    async fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        self.send(reqwest::Method::POST, path, Some(body)).await
    }

    async fn put(&self, path: &str, body: &Value) -> (u16, Value) {
        self.send(reqwest::Method::PUT, path, Some(body)).await
    }
}

fn assert_refused(got: &(u16, Value), names: &str, what: &str) {
    assert_eq!(got.0, 403, "{what}: {}", got.1);
    assert_eq!(got.1["code"], "forbidden", "{what}");
    assert_eq!(got.1["message"], needs(names), "{what}");
}

fn update(yaml: String, revision: i64) -> Value {
    json!({ "yaml": yaml, "base_revision": revision })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_scopes_create_with_hooks_needs_hooks_write() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        ts.create_local_repository("main").await;
        let plain = Token::new(&ts, &["policies:read", "policies:write"]).await;
        let with = Token::new(&ts, &["policies:write", "hooks:write"]).await;
        let body = json!({ "yaml": yaml("db", "main", "db", DUMP) });
        let got = plain.post("/api/v1/policies", &body).await;
        assert_refused(&got, "db", backend.name());
        let got = with.post("/api/v1/policies", &body).await;
        assert_eq!(got.0, 201, "[{}] {}", backend.name(), got.1);
        let got = plain
            .post(
                "/api/v1/policies",
                &json!({ "yaml": yaml("plain", "main", "db", "") }),
            )
            .await;
        assert_eq!(got.0, 201, "a policy without hooks needs no scope");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_scopes_change_or_remove_hooks_needs_hooks_write() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        ts.create_local_repository("main").await;
        let session = Session {
            api: ts.api().await,
            ts: &ts,
        };
        session.create(yaml("db", "main", "db", DUMP)).await;
        let plain = Token::new(&ts, &["policies:read", "policies:write"]).await;
        let what = backend.name();

        for extra in [DUMP_ARGS, "", AFTER_ONLY] {
            let got = plain
                .put(
                    "/api/v1/policies/db",
                    &update(yaml("db", "main", "db", extra), 1),
                )
                .await;
            assert_refused(&got, "db", what);
        }

        let moved = yaml("db", "main", "db", DUMP).replace("- /etc", "- /srv");
        let got = plain.put("/api/v1/policies/db", &update(moved, 1)).await;
        assert_eq!(got.0, 200, "[{what}] unchanged hooks: {}", got.1);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_scopes_session_unrestricted() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        ts.create_local_repository("main").await;
        let session = Session {
            api: ts.api().await,
            ts: &ts,
        };
        session.create(yaml("db", "main", "db", DUMP)).await;
        let (status, body) = session
            .put(
                "/api/v1/policies/db",
                &update(yaml("db", "main", "db", DUMP_ARGS), 1),
            )
            .await;
        assert_eq!(status, 200, "[{}] {body}", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_scopes_selector_widening_needs_hooks_write() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let repo = ts.create_local_repository("main").await;
        let session = Session {
            api: ts.api().await,
            ts: &ts,
        };
        session.enroll(&repo, "db-1", "db").await;
        session.enroll(&repo, "web-1", "web").await;
        session.create(yaml("db", "main", "db", DUMP)).await;
        let plain = Token::new(&ts, &["policies:read", "policies:write"]).await;
        let what = backend.name();

        let got = plain
            .put(
                "/api/v1/policies/db",
                &update(yaml("db", "main", "web", DUMP), 1),
            )
            .await;
        assert_refused(&got, "db", what);

        let narrowed =
            yaml("db", "main", "db", DUMP).replace("role: db\n", "role: db\n      tier: prod\n");
        let got = plain.put("/api/v1/policies/db", &update(narrowed, 1)).await;
        assert_eq!(got.0, 200, "[{what}] narrowing: {}", got.1);
        ts.stop().await;
    }
}

fn with_selector(yaml: String, selector: &str) -> String {
    yaml.replace(
        "  selector:\n    matchLabels:\n      role: db\n",
        &format!("  selector:{selector}\n"),
    )
}

const HOSTS_A: &str = "\n    hosts: [host-a]";
const HOSTS_AB: &str = "\n    hosts: [host-a, host-b]";
const NO_HOSTS: &str = " {}";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_scopes_widening_counts_without_current_hosts() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        ts.create_local_repository("main").await;
        let session = Session {
            api: ts.api().await,
            ts: &ts,
        };
        let plain = Token::new(&ts, &["policies:read", "policies:write"]).await;
        let what = backend.name();
        let base = || yaml("db", "main", "db", DUMP);

        session.create(base()).await;
        let got = plain
            .put(
                "/api/v1/policies/db",
                &update(with_selector(base(), NO_HOSTS), 1),
            )
            .await;
        assert_refused(&got, "db", &format!("empty selector [{what}]"));
        let got = plain
            .put(
                "/api/v1/policies/db",
                &update(with_selector(base(), HOSTS_A), 1),
            )
            .await;
        assert_refused(&got, "db", &format!("labels to host list [{what}]"));
        let relabelled = base().replace("role: db\n", "role: web\n");
        let got = plain
            .put("/api/v1/policies/db", &update(relabelled, 1))
            .await;
        assert_refused(&got, "db", &format!("changed label [{what}]"));
        let dropped = base().replace("role: db\n", "tier: prod\n");
        let got = plain.put("/api/v1/policies/db", &update(dropped, 1)).await;
        assert_refused(&got, "db", &format!("removed label [{what}]"));

        let (_, stored) = session
            .send(reqwest::Method::GET, "/api/v1/policies/db", None)
            .await;
        assert_eq!(stored["revision"], 1, "[{what}] nothing was written");

        let narrowed = base().replace("role: db\n", "role: db\n      tier: prod\n");
        let got = plain.put("/api/v1/policies/db", &update(narrowed, 1)).await;
        assert_eq!(got.0, 200, "[{what}] added label: {}", got.1);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_scopes_host_list_changes() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        ts.create_local_repository("main").await;
        let session = Session {
            api: ts.api().await,
            ts: &ts,
        };
        let plain = Token::new(&ts, &["policies:read", "policies:write"]).await;
        let what = backend.name();
        let listed = |hosts: &str| with_selector(yaml("db", "main", "db", DUMP), hosts);

        session.create(listed(HOSTS_AB)).await;
        let got = plain
            .put("/api/v1/policies/db", &update(listed(NO_HOSTS), 1))
            .await;
        assert_refused(&got, "db", &format!("list to everything [{what}]"));
        let got = plain
            .put(
                "/api/v1/policies/db",
                &update(listed("\n    hosts: [host-a, host-b, host-c]"), 1),
            )
            .await;
        assert_refused(&got, "db", &format!("unenrolled host id [{what}]"));
        let got = plain
            .put("/api/v1/policies/db", &update(listed(HOSTS_A), 1))
            .await;
        assert_eq!(got.0, 200, "[{what}] fewer hosts: {}", got.1);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_scopes_move_to_empty_repository_needs_hooks_write() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        ts.create_local_repository("main").await;
        ts.create_local_repository("empty").await;
        let session = Session {
            api: ts.api().await,
            ts: &ts,
        };
        session.create(yaml("db", "main", "db", DUMP)).await;
        let plain = Token::new(&ts, &["policies:read", "policies:write"]).await;

        let got = plain
            .put(
                "/api/v1/policies/db",
                &update(yaml("db", "empty", "db", DUMP), 1),
            )
            .await;
        assert_refused(&got, "db", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_scopes_apply_widening_without_current_hosts() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        ts.create_local_repository("main").await;
        let session = Session {
            api: ts.api().await,
            ts: &ts,
        };
        session.create(yaml("db", "main", "db", DUMP)).await;
        let plain = Token::new(&ts, &["policies:read", "policies:write"]).await;
        let widened = with_selector(yaml("db", "main", "db", DUMP), NO_HOSTS);
        for dry_run in [true, false] {
            let got = plain
                .post(
                    "/api/v1/policies/apply",
                    &json!({
                        "documents": [{ "source": "p.yaml", "yaml": widened }],
                        "adopt": true,
                        "dry_run": dry_run,
                    }),
                )
                .await;
            assert_refused(
                &got,
                "db",
                &format!("dry_run={dry_run} [{}]", backend.name()),
            );
        }
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_scopes_stale_revision_conflicts_before_gates() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        ts.create_local_repository("main").await;
        let session = Session {
            api: ts.api().await,
            ts: &ts,
        };
        session.create(yaml("db", "main", "db", DUMP)).await;
        let (status, _) = session
            .put(
                "/api/v1/policies/db",
                &update(
                    yaml("db", "main", "db", DUMP).replace("- /etc", "- /srv"),
                    1,
                ),
            )
            .await;
        assert_eq!(status, 200);
        let plain = Token::new(&ts, &["policies:read", "policies:write"]).await;

        let got = plain
            .put(
                "/api/v1/policies/db",
                &update(with_selector(yaml("db", "main", "db", DUMP), NO_HOSTS), 1),
            )
            .await;
        assert_eq!(got.0, 409, "[{}] {}", backend.name(), got.1);
        assert_eq!(got.1["code"], "policy_conflict");
        let (_, stored) = session
            .send(reqwest::Method::GET, "/api/v1/policies/db", None)
            .await;
        assert_eq!(stored["revision"], 2);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_scopes_repository_change_needs_hooks_write() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let main = ts.create_local_repository("main").await;
        ts.create_local_repository("other").await;
        let session = Session {
            api: ts.api().await,
            ts: &ts,
        };
        session.enroll(&main, "db-1", "db").await;
        session.create(yaml("db", "other", "db", DUMP)).await;
        let plain = Token::new(&ts, &["policies:read", "policies:write"]).await;

        let got = plain
            .put(
                "/api/v1/policies/db",
                &update(yaml("db", "main", "db", DUMP), 1),
            )
            .await;
        assert_refused(&got, "db", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_scopes_apply_dry_run_names_policies() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        ts.create_local_repository("main").await;
        let session = Session {
            api: ts.api().await,
            ts: &ts,
        };
        for name in ["b", "a", "c"] {
            session.create(yaml(name, "main", "db", DUMP)).await;
        }
        let plain = Token::new(&ts, &["policies:read", "policies:write"]).await;
        let docs = [
            yaml("a", "main", "db", DUMP_ARGS),
            yaml("b", "main", "db", ""),
            yaml("c", "main", "db", DUMP),
        ]
        .join("---\n");
        let body = |dry_run: bool| {
            json!({
                "documents": [{ "source": "p.yaml", "yaml": docs }],
                "adopt": true,
                "dry_run": dry_run,
            })
        };
        for dry_run in [true, false] {
            let got = plain.post("/api/v1/policies/apply", &body(dry_run)).await;
            assert_refused(
                &got,
                "a, b",
                &format!("dry_run={dry_run} [{}]", backend.name()),
            );
        }
        let (_, stored) = session
            .send(reqwest::Method::GET, "/api/v1/policies/a", None)
            .await;
        assert_eq!(
            stored["policy"]["spec"]["hooks"]["before"][0].get("args"),
            None
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_scopes_delete_policy_with_hooks_allowed() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        ts.create_local_repository("main").await;
        let session = Session {
            api: ts.api().await,
            ts: &ts,
        };
        session.create(yaml("db", "main", "db", DUMP)).await;
        session.create(yaml("web", "main", "web", "")).await;
        let plain = Token::new(&ts, &["policies:read", "policies:write"]).await;
        let what = backend.name();

        let (status, body) = plain
            .send(reqwest::Method::DELETE, "/api/v1/policies/db", None)
            .await;
        assert_eq!(status, 204, "[{what}] {body}");

        let hooked = Token::new(&ts, &["policies:read", "policies:write", "hooks:write"]).await;
        let apply = json!({
            "documents": [{ "source": "p.yaml", "yaml": yaml("kept", "main", "db", DUMP) }],
        });
        let got = hooked.post("/api/v1/policies/apply", &apply).await;
        assert_eq!(got.0, 200, "[{what}] {}", got.1);
        let prune = json!({
            "documents": [{ "source": "p.yaml", "yaml": yaml("other", "main", "db", "") }],
            "prune": true,
        });
        let got = plain.post("/api/v1/policies/apply", &prune).await;
        assert_eq!(got.0, 200, "[{what}] prune a hooked policy: {}", got.1);
        let removed = got.1["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["action"] == "delete" && c["name"] == "kept");
        assert!(removed, "{}", got.1);
        ts.stop().await;
    }
}

fn role(value: &str) -> Value {
    json!({ "labels": [{ "key": "role", "value": value }] })
}

async fn labels_of(session: &Session<'_>, id: &str) -> Value {
    let (_, host) = session
        .send(reqwest::Method::GET, &format!("/api/v1/hosts/{id}"), None)
        .await;
    host["labels"].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_scopes_labels_widening_needs_hooks_write() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let repo = ts.create_local_repository("main").await;
        let session = Session {
            api: ts.api().await,
            ts: &ts,
        };
        let id = session.enroll(&repo, "web-1", "web").await;
        session.create(yaml("db", "main", "db", DUMP)).await;
        let path = format!("/api/v1/hosts/{id}/labels");
        let what = backend.name();

        let plain = Token::new(&ts, &["hosts:read", "hosts:write"]).await;
        let got = plain.put(&path, &role("db")).await;
        assert_refused(&got, "db", what);
        assert_eq!(labels_of(&session, &id).await, role("web")["labels"]);

        let with = Token::new(&ts, &["hosts:read", "hosts:write", "hooks:write"]).await;
        let got = with.put(&path, &role("db")).await;
        assert_eq!(got.0, 200, "[{what}] {}", got.1);

        let (status, body) = session.put(&path, &role("web")).await;
        assert_eq!(status, 200, "[{what}] session: {body}");
        let (status, body) = session.put(&path, &role("db")).await;
        assert_eq!(status, 200, "[{what}] session: {body}");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_scopes_labels_not_widening_allowed() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let repo = ts.create_local_repository("main").await;
        let session = Session {
            api: ts.api().await,
            ts: &ts,
        };
        let id = session.enroll(&repo, "db-1", "db").await;
        session.create(yaml("db", "main", "db", DUMP)).await;
        session.create(yaml("web", "main", "web", "")).await;
        let path = format!("/api/v1/hosts/{id}/labels");
        let plain = Token::new(&ts, &["hosts:read", "hosts:write"]).await;
        let what = backend.name();

        let got = plain.put(&path, &role("web")).await;
        assert_eq!(got.0, 200, "[{what}] to a policy without hooks: {}", got.1);
        let got = plain.put(&path, &json!({ "labels": [] })).await;
        assert_eq!(got.0, 200, "[{what}] dropping labels: {}", got.1);
        ts.stop().await;
    }
}
