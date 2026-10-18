mod common;

use common::{TestServer, backends, bearer_client};
use reqwest::Client;
use serde_json::{Value, json};
use sqlx::Row;

fn doc(name: &str, extra: &str) -> String {
    format!(
        "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: {name}\n\
         spec:\n  repository: main\n  selector:\n    matchLabels:\n      env: prod\n  \
         paths:\n    - /etc\n{extra}"
    )
}

fn file(source: &str, docs: &[String]) -> Value {
    json!({ "source": source, "yaml": docs.join("---\n") })
}

struct Fixture {
    ts: TestServer,
    client: Client,
    limited: Client,
    base: String,
}

impl Fixture {
    async fn start(backend: &common::Backend) -> Fixture {
        let ts = TestServer::start(backend).await;
        ts.create_local_repository("main").await;
        let token = ts
            .issue_token(&["policies:read", "policies:write", "snapshots:delete"])
            .await;
        let limited = ts.issue_token(&["policies:read", "policies:write"]).await;
        let base = ts.rest_url();
        Fixture {
            ts,
            client: bearer_client(&token),
            limited: bearer_client(&limited),
            base,
        }
    }

    async fn apply(&self, body: Value) -> (u16, Value) {
        let response = self
            .client
            .post(format!("{}/api/v1/policies/apply", self.base))
            .json(&body)
            .send()
            .await
            .unwrap();
        (
            response.status().as_u16(),
            response.json().await.unwrap_or(Value::Null),
        )
    }

    async fn get(&self, name: &str) -> (u16, Value) {
        let response = self
            .client
            .get(format!("{}/api/v1/policies/{name}", self.base))
            .send()
            .await
            .unwrap();
        (
            response.status().as_u16(),
            response.json().await.unwrap_or(Value::Null),
        )
    }

    async fn create_in_ui(&self, name: &str) {
        let response = self
            .ts
            .api()
            .await
            .post("/api/v1/policies", &json!({ "yaml": doc(name, "") }))
            .await;
        assert_eq!(response.status(), 201);
    }

    async fn managed_by(&self, name: &str) -> Option<String> {
        sqlx::query("SELECT managed_by FROM policies WHERE name = $1")
            .bind(name)
            .fetch_one(&self.ts.state().db.pool)
            .await
            .unwrap()
            .try_get("managed_by")
            .unwrap()
    }

    async fn count(&self) -> i64 {
        sqlx::query("SELECT COUNT(*) AS n FROM policies")
            .fetch_one(&self.ts.state().db.pool)
            .await
            .unwrap()
            .try_get("n")
            .unwrap()
    }

    async fn revision(&self, name: &str) -> i64 {
        sqlx::query("SELECT revision FROM policies WHERE name = $1")
            .bind(name)
            .fetch_one(&self.ts.state().db.pool)
            .await
            .unwrap()
            .try_get("revision")
            .unwrap()
    }
}

fn names(changes: &Value, action: &str) -> Vec<String> {
    changes
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["action"] == action)
        .map(|c| c["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apply_schema() {
    for backend in backends() {
        let f = Fixture::start(&backend).await;
        f.create_in_ui("ui-made").await;
        assert_eq!(f.managed_by("ui-made").await, None, "[{}]", backend.name());
        let (_, detail) = f.get("ui-made").await;
        assert!(detail["managed_by"].is_null());
        f.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apply_creates_managed_policy() {
    for backend in backends() {
        let f = Fixture::start(&backend).await;
        let body = json!({
            "documents": [file("ci/policies.yaml", &[doc("alpha", ""), doc("beta", "")])],
            "dry_run": true,
        });
        let (status, plan) = f.apply(body).await;
        assert_eq!(status, 200, "[{}] {plan}", backend.name());
        assert_eq!(plan["dry_run"], true);
        assert_eq!(plan["manager"], "lode-apply");
        assert_eq!(names(&plan["changes"], "create"), ["alpha", "beta"]);
        assert_eq!(plan["changes"][0]["source"], "ci/policies.yaml");
        assert_eq!(f.count().await, 0);

        let body = json!({ "documents": [file("ci/policies.yaml", &[doc("alpha", ""), doc("beta", "")])] });
        let (status, applied) = f.apply(body).await;
        assert_eq!(status, 200, "{applied}");
        assert_eq!(applied["dry_run"], false);
        assert_eq!(f.count().await, 2);
        for name in ["alpha", "beta"] {
            let (status, detail) = f.get(name).await;
            assert_eq!(status, 200);
            assert_eq!(detail["managed_by"], "lode-apply");
            assert_eq!(detail["revision"], 1);
            assert_eq!(f.managed_by(name).await.as_deref(), Some("lode-apply"));
        }
        f.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apply_update_field_paths() {
    for backend in backends() {
        let f = Fixture::start(&backend).await;
        let before = doc("nightly", "  schedule:\n    cron: \"0 2 * * *\"\n");
        let (status, _) = f
            .apply(json!({ "documents": [file("a.yaml", &[before])] }))
            .await;
        assert_eq!(status, 200);

        let after = format!(
            "{}  retention:\n    keepDaily: 14\n",
            doc("nightly", "  schedule:\n    cron: \"30 2 * * *\"\n")
                .replace("    - /etc\n", "    - /etc\n    - /var\n")
        );
        let (status, plan) = f
            .apply(json!({ "documents": [file("a.yaml", &[after])], "dry_run": true }))
            .await;
        assert_eq!(status, 200, "[{}] {plan}", backend.name());
        let change = &plan["changes"][0];
        assert_eq!(change["action"], "update");
        assert_eq!(change["name"], "nightly");
        assert_eq!(change["adopted"], false);
        let fields = change["fields"].as_array().unwrap();
        let find = |path: &str| fields.iter().find(|f| f["path"] == path).cloned();
        let cron = find("spec.schedule.cron").expect("cron changed");
        assert_eq!(cron["from"], "0 2 * * *");
        assert_eq!(cron["to"], "30 2 * * *");
        let keep = find("spec.retention.keepDaily").expect("retention added");
        assert!(keep["from"].is_null());
        assert_eq!(keep["to"], 14);
        let paths = find("spec.paths").expect("paths changed");
        assert_eq!(paths["from"], json!(["/etc"]));
        assert_eq!(paths["to"], json!(["/etc", "/var"]));
        assert_eq!(fields.len(), 3);
        assert_eq!(f.revision("nightly").await, 1);
        f.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apply_prune_only_managed() {
    for backend in backends() {
        let f = Fixture::start(&backend).await;
        f.create_in_ui("ui-made").await;
        let (status, _) = f
            .apply(
                json!({ "documents": [file("a.yaml", &[doc("old-test", ""), doc("keep", "")])] }),
            )
            .await;
        assert_eq!(status, 200);
        let (status, _) = f
            .apply(json!({
                "documents": [file("b.yaml", &[doc("argo-made", "")])],
                "manager": "argo",
            }))
            .await;
        assert_eq!(status, 200);

        let body = json!({
            "documents": [file("a.yaml", &[doc("keep", "")])],
            "prune": true,
            "dry_run": true,
        });
        let (status, plan) = f.apply(body).await;
        assert_eq!(status, 200, "[{}] {plan}", backend.name());
        assert_eq!(names(&plan["changes"], "delete"), ["old-test"]);
        assert_eq!(plan["changes"].as_array().unwrap().len(), 1);
        assert_eq!(plan["changes"][0]["managed_by"], "lode-apply");
        assert_eq!(plan["unchanged"], 1);
        assert_eq!(f.count().await, 4);

        let body = json!({ "documents": [file("a.yaml", &[doc("keep", "")])], "prune": true });
        let (status, _) = f.apply(body).await;
        assert_eq!(status, 200);
        assert_eq!(f.get("old-test").await.0, 404);
        for name in ["keep", "ui-made", "argo-made"] {
            assert_eq!(f.get(name).await.0, 200, "{name} must stay");
        }
        f.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apply_conflict_unmanaged_needs_adopt() {
    for backend in backends() {
        let f = Fixture::start(&backend).await;
        f.create_in_ui("ui-made").await;
        let body =
            json!({ "documents": [file("a.yaml", &[doc("fresh", ""), doc("ui-made", "")])] });
        let (status, error) = f.apply(body).await;
        assert_eq!(status, 422, "[{}] {error}", backend.name());
        assert_eq!(error["code"], "validation");
        assert_eq!(
            error["errors"][0]["message"],
            "policy/ui-made exists and is not managed by lode-apply. Use --adopt to take it over."
        );
        assert_eq!(error["errors"][0]["source"], "a.yaml");
        assert_eq!(f.get("fresh").await.0, 404);
        assert_eq!(f.managed_by("ui-made").await, None);
        f.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apply_adopt_takes_over() {
    for backend in backends() {
        let f = Fixture::start(&backend).await;
        f.create_in_ui("ui-made").await;
        let body = json!({ "documents": [file("a.yaml", &[doc("ui-made", "")])], "adopt": true });
        let (status, plan) = f.apply(body.clone()).await;
        assert_eq!(status, 200, "[{}] {plan}", backend.name());
        assert_eq!(plan["changes"][0]["action"], "update");
        assert_eq!(plan["changes"][0]["adopted"], true);
        assert!(plan["changes"][0]["fields"].as_array().unwrap().is_empty());
        assert_eq!(f.managed_by("ui-made").await.as_deref(), Some("lode-apply"));
        assert_eq!(f.get("ui-made").await.0, 200);

        let (_, again) = f.apply(body).await;
        assert!(again["changes"].as_array().unwrap().is_empty());
        assert_eq!(again["unchanged"], 1);
        f.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apply_other_manager_refused() {
    for backend in backends() {
        let f = Fixture::start(&backend).await;
        let (status, _) = f
            .apply(
                json!({ "documents": [file("a.yaml", &[doc("shared", "")])], "manager": "argo" }),
            )
            .await;
        assert_eq!(status, 200);
        let body = json!({
            "documents": [file("b.yaml", &[doc("shared", "  exclude:\n    - '*.tmp'\n")])],
            "adopt": true,
        });
        let (status, error) = f.apply(body).await;
        assert_eq!(status, 422, "[{}] {error}", backend.name());
        assert_eq!(
            error["errors"][0]["message"],
            "policy/shared is managed by argo."
        );
        assert_eq!(f.managed_by("shared").await.as_deref(), Some("argo"));
        assert_eq!(f.revision("shared").await, 1);

        let (status, error) = f
            .apply(json!({ "documents": [], "manager": "Bad Name" }))
            .await;
        assert_eq!(status, 422);
        assert_eq!(error["errors"][0]["path"], "manager");
        f.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apply_validation_errors_with_lines() {
    for backend in backends() {
        let f = Fixture::start(&backend).await;
        let first = doc("first", "");
        let second = doc("second", "  schedule:\n    crn: \"0 2 * * *\"\n");
        let text = format!("# catalog\n---\n{first}---\n{second}");
        let line = text
            .lines()
            .position(|l| l.contains("crn:"))
            .map(|i| i + 1)
            .unwrap();
        let (status, error) = f
            .apply(json!({ "documents": [{ "source": "ci/policies.yaml", "yaml": text }] }))
            .await;
        assert_eq!(status, 422, "[{}] {error}", backend.name());
        assert_eq!(error["code"], "validation");
        let first_error = &error["errors"][0];
        assert_eq!(first_error["source"], "ci/policies.yaml");
        assert_eq!(first_error["line"], line, "{error}");
        assert!(first_error["column"].is_number());
        assert_eq!(f.count().await, 0);

        let (status, error) = f
            .apply(json!({ "documents": [
                file("a.yaml", &[doc("dup", "")]),
                file("b.yaml", &[doc("dup", "")]),
            ] }))
            .await;
        assert_eq!(status, 422);
        assert!(
            error["errors"][0]["message"]
                .as_str()
                .unwrap()
                .contains("a.yaml")
        );

        let (status, error) = f
            .apply(json!({ "documents": [file("c.yaml", &[doc("x", "").replace("main", "nowhere")])] }))
            .await;
        assert_eq!(status, 422);
        assert_eq!(error["errors"][0]["path"], "spec.repository");

        let many = vec!["kind: BackupPolicy\n"; 1001].join("---\n");
        let (status, error) = f
            .apply(json!({ "documents": [{ "source": "big.yaml", "yaml": many }] }))
            .await;
        assert_eq!(status, 422);
        assert!(
            error["errors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["message"].as_str().unwrap().contains("1000"))
        );
        assert_eq!(f.count().await, 0);
        f.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apply_idempotent_second_run() {
    for backend in backends() {
        let f = Fixture::start(&backend).await;
        let body = json!({
            "documents": [file("a.yaml", &[doc("alpha", "  schedule:\n    cron: \"0 2 * * *\"\n"), doc("beta", "")])],
            "prune": true,
        });
        let (status, first) = f.apply(body.clone()).await;
        assert_eq!(status, 200);
        assert_eq!(first["changes"].as_array().unwrap().len(), 2);
        let (status, second) = f.apply(body).await;
        assert_eq!(status, 200, "[{}] {second}", backend.name());
        assert!(second["changes"].as_array().unwrap().is_empty());
        assert_eq!(second["unchanged"], 2);
        assert_eq!(f.revision("alpha").await, 1);
        assert_eq!(f.revision("beta").await, 1);
        f.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apply_atomic_on_active_jobs() {
    for backend in backends() {
        let f = Fixture::start(&backend).await;
        let (status, _) = f
            .apply(json!({ "documents": [file("a.yaml", &[doc("busy", ""), doc("idle", "")])] }))
            .await;
        assert_eq!(status, 200);

        let pool = &f.ts.state().db.pool;
        let repo: String = sqlx::query("SELECT id FROM repositories WHERE name = 'main'")
            .fetch_one(pool)
            .await
            .unwrap()
            .try_get("id")
            .unwrap();
        sqlx::query(
            "INSERT INTO agents (id, hostname, repository_id, labels_json, os, arch, version, \
             status, enrolled_at, cert_serial, cert_issued_at, cert_not_after) \
             VALUES ('h1', 'h1', $1, '{}', 'linux', 'x86_64', 'test', 'active', 0, 'aa', 0, 0)",
        )
        .bind(&repo)
        .execute(pool)
        .await
        .unwrap();
        let busy: String = sqlx::query("SELECT id FROM policies WHERE name = 'busy'")
            .fetch_one(pool)
            .await
            .unwrap()
            .try_get("id")
            .unwrap();
        sqlx::query(
            "INSERT INTO jobs (id, host_id, policy_id, source, state, spec, queued_at) \
             VALUES ('j1', 'h1', $1, 'manual', 'running', $2, 0)",
        )
        .bind(&busy)
        .bind(Vec::<u8>::new())
        .execute(pool)
        .await
        .unwrap();

        let body = json!({
            "documents": [file("a.yaml", &[
                doc("idle", "  schedule:\n    cron: \"0 2 * * *\"\n"),
                doc("fresh", ""),
            ])],
            "prune": true,
        });
        let (status, error) = f.apply(body).await;
        assert_eq!(status, 409, "[{}] {error}", backend.name());
        assert_eq!(error["code"], "conflict");
        assert!(error["message"].as_str().unwrap().contains("busy"));
        assert_eq!(f.count().await, 2);
        assert_eq!(f.managed_by("busy").await.as_deref(), Some("lode-apply"));
        assert_eq!(f.revision("idle").await, 1);
        f.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apply_drift_after_ui_edit() {
    for backend in backends() {
        let f = Fixture::start(&backend).await;
        let body = json!({ "documents": [file("a.yaml", &[doc("nightly", "")])], "dry_run": true });
        let applied = json!({ "documents": [file("a.yaml", &[doc("nightly", "")])] });
        assert_eq!(f.apply(applied).await.0, 200);
        let (_, clean) = f.apply(body.clone()).await;
        assert!(clean["changes"].as_array().unwrap().is_empty());

        let edited = doc("nightly", "  schedule:\n    cron: \"15 3 * * *\"\n");
        let response = f
            .client
            .put(format!("{}/api/v1/policies/nightly", f.base))
            .json(&json!({ "base_revision": 1, "yaml": edited }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "[{}]", backend.name());
        let saved: Value = response.json().await.unwrap();
        assert_eq!(saved["managed_by"], "lode-apply");

        let (_, drift) = f.apply(body).await;
        let change = &drift["changes"][0];
        assert_eq!(change["action"], "update");
        let field = &change["fields"][0];
        assert_eq!(field["path"], "spec.schedule.cron");
        assert_eq!(field["from"], "15 3 * * *");
        assert!(field["to"].is_null());
        f.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apply_requires_write_scope() {
    for backend in backends() {
        let f = Fixture::start(&backend).await;
        let token = f.ts.issue_token(&["policies:read"]).await;
        let response = bearer_client(&token)
            .post(format!("{}/api/v1/policies/apply", f.base))
            .json(&json!({ "documents": [file("a.yaml", &[doc("x", "")])], "dry_run": true }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "[{}]", backend.name());
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["message"], "The token needs the policies:write scope.");
        f.ts.stop().await;
    }
}

const RETENTION: &str = "  retention:\n    keepLast: 5\n";
const NEEDS_DELETE: &str = "The token needs the snapshots:delete scope to set or change retention.";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apply_retention_needs_delete_scope() {
    for backend in backends() {
        let f = Fixture::start(&backend).await;
        let name = backend.name();
        let send = |dry_run: bool, extra: &str| {
            let body = json!({
                "documents": [file("a.yaml", &[doc("nightly", extra)])],
                "dry_run": dry_run,
            });
            let request = f
                .limited
                .post(format!("{}/api/v1/policies/apply", f.base))
                .json(&body);
            async move {
                let response = request.send().await.unwrap();
                (
                    response.status().as_u16(),
                    response.json::<Value>().await.unwrap_or(Value::Null),
                )
            }
        };

        let (status, body) = send(false, RETENTION).await;
        assert_eq!(status, 403, "[{name}] {body}");
        assert_eq!(body["message"], NEEDS_DELETE);
        assert_eq!(f.count().await, 0);
        assert_eq!(send(true, RETENTION).await.0, 403, "[{name}]");

        assert_eq!(send(false, "").await.0, 200, "[{name}]");
        assert_eq!(f.count().await, 1);
        let (status, _) = send(false, RETENTION).await;
        assert_eq!(status, 403, "[{name}]");
        assert_eq!(f.revision("nightly").await, 1);

        let (status, body) = f
            .apply(json!({ "documents": [file("a.yaml", &[doc("nightly", RETENTION)])] }))
            .await;
        assert_eq!(status, 200, "[{name}] {body}");
        assert_eq!(send(false, RETENTION).await.0, 200, "[{name}]");
        let (status, _) = send(false, "  retention:\n    keepLast: 1\n").await;
        assert_eq!(status, 403, "[{name}]");
        f.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policies_retention_needs_delete_scope() {
    for backend in backends() {
        let f = Fixture::start(&backend).await;
        let name = backend.name();
        let url = format!("{}/api/v1/policies", f.base);

        let denied = f
            .limited
            .post(&url)
            .json(&json!({ "yaml": doc("p", RETENTION) }))
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), 403, "[{name}]");
        let body: Value = denied.json().await.unwrap();
        assert_eq!(body["message"], NEEDS_DELETE);
        assert_eq!(f.count().await, 0);

        let created = f
            .limited
            .post(&url)
            .json(&json!({ "yaml": doc("p", "") }))
            .send()
            .await
            .unwrap();
        assert_eq!(created.status(), 201, "[{name}]");

        let put = |client: &Client, revision: i64, extra: &str| {
            client
                .put(format!("{url}/p"))
                .json(&json!({ "base_revision": revision, "yaml": doc("p", extra) }))
                .send()
        };
        let denied = put(&f.limited, 1, RETENTION).await.unwrap();
        assert_eq!(denied.status(), 403, "[{name}]");
        assert_eq!(f.revision("p").await, 1);
        let same = put(&f.limited, 1, "  exclude:\n    - '*.tmp'\n")
            .await
            .unwrap();
        assert_eq!(same.status(), 200, "[{name}]");
        let allowed = put(&f.client, 2, RETENTION).await.unwrap();
        assert_eq!(allowed.status(), 200, "[{name}]");
        let unchanged = put(&f.limited, 3, RETENTION).await.unwrap();
        assert_eq!(unchanged.status(), 200, "[{name}]");

        let session =
            f.ts.api()
                .await
                .post(
                    "/api/v1/policies",
                    &json!({ "yaml": doc("by-session", RETENTION) }),
                )
                .await;
        assert_eq!(session.status(), 201, "[{name}]");
        f.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apply_prune_refuses_empty_input() {
    for backend in backends() {
        let f = Fixture::start(&backend).await;
        let name = backend.name();
        let (status, _) = f
            .apply(json!({ "documents": [file("a.yaml", &[doc("keep", "")])] }))
            .await;
        assert_eq!(status, 200);

        for documents in [
            json!([]),
            json!([{ "source": "empty.yaml", "yaml": "" }]),
            json!([{ "source": "notes.yaml", "yaml": "# nothing here\n---\n" }]),
        ] {
            for dry_run in [true, false] {
                let (status, error) = f
                    .apply(json!({ "documents": documents, "prune": true, "dry_run": dry_run }))
                    .await;
                assert_eq!(status, 422, "[{name}] {error}");
                assert!(
                    error["errors"][0]["message"]
                        .as_str()
                        .unwrap()
                        .contains("at least one policy"),
                    "{error}"
                );
            }
        }
        assert_eq!(f.count().await, 1, "[{name}]");

        let (status, _) = f.apply(json!({ "documents": [], "prune": false })).await;
        assert_eq!(status, 200, "[{name}]");
        f.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apply_syntax_error_is_reported_once() {
    for backend in backends() {
        let f = Fixture::start(&backend).await;
        let name = backend.name();
        let broken = "a: [1\n---\nb: 2\n";
        let (status, error) = f
            .apply(json!({ "documents": [
                { "source": "broken.yaml", "yaml": broken },
                file("second.yaml", &[doc("x", "").replace("main", "nowhere")]),
            ] }))
            .await;
        assert_eq!(status, 422, "[{name}] {error}");
        let errors = error["errors"].as_array().unwrap();
        let in_broken = errors
            .iter()
            .filter(|e| {
                e["source"] == "broken.yaml"
                    && e["message"]
                        .as_str()
                        .unwrap()
                        .contains("did not find expected")
            })
            .count();
        assert_eq!(in_broken, 1, "[{name}] {error}");
        assert!(
            errors
                .iter()
                .any(|e| e["source"] == "second.yaml" && e["path"] == "spec.repository"),
            "[{name}] {error}"
        );
        assert_eq!(f.count().await, 0);
        f.ts.stop().await;
    }
}
