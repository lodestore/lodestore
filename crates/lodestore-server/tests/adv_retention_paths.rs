//! A token that may edit policies must not empty a policy with retention by
//! changing what or where it backs up. Only public REST paths of the first
//! release are used, so the scenario runs unchanged against that release.

mod common;

use common::{TestServer, backends, bearer_client};
use reqwest::Method;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};

const CONTENT_REFUSED: &str = "The token needs the snapshots:delete scope to change what or where a policy with retention backs up.";
const PATHS: &str = "  paths:\n    - /etc\n    - /srv\n";
const EXCLUDE: &str = "  exclude:\n    - \"*.log\"\n    - \"!keep.log\"\n";

fn policy(name: &str, repository: &str, retention: bool, body: &str) -> String {
    let retention = if retention {
        "  retention:\n    keepLast: 3\n"
    } else {
        ""
    };
    format!(
        "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: {name}\n\
         spec:\n  repository: {repository}\n  selector:\n    matchLabels:\n      role: db\n\
         {body}{retention}"
    )
}

fn kept(repository: &str, body: &str) -> String {
    policy("kept", repository, true, body)
}

struct Caller {
    client: reqwest::Client,
    base: String,
}

impl Caller {
    async fn session(ts: &TestServer) -> Caller {
        let api = ts.api().await;
        let headers = HeaderMap::from_iter([
            (
                reqwest::header::COOKIE,
                HeaderValue::from_str(api.cookie()).unwrap(),
            ),
            (
                reqwest::header::HeaderName::from_static("x-csrf-token"),
                HeaderValue::from_str(&api.csrf).unwrap(),
            ),
        ]);
        Caller {
            client: reqwest::Client::builder()
                .default_headers(headers)
                .build()
                .unwrap(),
            base: ts.rest_url(),
        }
    }

    async fn token(ts: &TestServer, scopes: &[&str]) -> Caller {
        Caller {
            client: bearer_client(&ts.issue_token(scopes).await),
            base: ts.rest_url(),
        }
    }

    async fn send(&self, method: Method, path: &str, body: Option<&Value>) -> (u16, Value) {
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

    async fn create(&self, yaml: String) {
        let got = self
            .send(
                Method::POST,
                "/api/v1/policies",
                Some(&json!({ "yaml": yaml })),
            )
            .await;
        assert_eq!(got.0, 201, "{}", got.1);
    }

    async fn put(&self, name: &str, yaml: String, revision: i64) -> (u16, Value) {
        let body = json!({ "yaml": yaml, "base_revision": revision });
        self.send(
            Method::PUT,
            &format!("/api/v1/policies/{name}"),
            Some(&body),
        )
        .await
    }

    async fn apply(&self, documents: &[String], dry_run: bool) -> (u16, Value) {
        let documents: Vec<Value> = documents
            .iter()
            .map(|yaml| json!({ "source": "p.yaml", "yaml": yaml }))
            .collect();
        let body = json!({ "documents": documents, "adopt": true, "dry_run": dry_run });
        self.send(Method::POST, "/api/v1/policies/apply", Some(&body))
            .await
    }

    async fn revision(&self, name: &str) -> Value {
        let got = self
            .send(Method::GET, &format!("/api/v1/policies/{name}"), None)
            .await;
        assert_eq!(got.0, 200, "{}", got.1);
        got.1["revision"].clone()
    }
}

fn assert_refused(got: &(u16, Value), message: &str, what: &str) {
    assert_eq!(got.0, 403, "{what}: {}", got.1);
    assert_eq!(got.1["code"], "forbidden", "{what}");
    assert_eq!(got.1["message"], message, "{what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_policies_write_cannot_change_what_a_retention_policy_backs_up() {
    for backend in backends() {
        let what = backend.name();
        let ts = TestServer::start(&backend).await;
        ts.create_local_repository("main").await;
        ts.create_local_repository("other").await;
        let session = Caller::session(&ts).await;
        session
            .create(kept("main", &format!("{PATHS}{EXCLUDE}")))
            .await;
        session
            .create(policy("other-p", "main", false, PATHS))
            .await;
        let weak = Caller::token(&ts, &["policies:read", "policies:write"]).await;

        let emptied = "  paths:\n    - /srv/empty\n";
        let edits = [
            ("paths", kept("main", &format!("{emptied}{EXCLUDE}"))),
            (
                "trailing slash",
                kept(
                    "main",
                    &format!("  paths:\n    - /etc/\n    - /srv\n{EXCLUDE}"),
                ),
            ),
            (
                "exclude everything",
                kept("main", &format!("{PATHS}  exclude:\n    - \"**\"\n")),
            ),
            (
                "exclude order",
                kept(
                    "main",
                    &format!("{PATHS}  exclude:\n    - \"!keep.log\"\n    - \"*.log\"\n"),
                ),
            ),
            (
                "oneFileSystem",
                kept("main", &format!("{PATHS}{EXCLUDE}  oneFileSystem: true\n")),
            ),
            (
                "excludeCaches",
                kept("main", &format!("{PATHS}{EXCLUDE}  excludeCaches: true\n")),
            ),
            ("repository", kept("other", &format!("{PATHS}{EXCLUDE}"))),
        ];
        for (field, yaml) in &edits {
            let got = weak.put("kept", yaml.clone(), 1).await;
            assert_refused(&got, CONTENT_REFUSED, &format!("[{what}] PUT {field}"));
        }

        let emptied_kept = kept("main", &format!("{emptied}{EXCLUDE}"));
        for dry_run in [true, false] {
            let got = weak
                .apply(std::slice::from_ref(&emptied_kept), dry_run)
                .await;
            assert_refused(
                &got,
                CONTENT_REFUSED,
                &format!("[{what}] apply dry_run={dry_run}"),
            );
        }
        let harmless = policy("other-p", "main", false, emptied);
        let got = weak
            .apply(&[harmless.clone(), emptied_kept.clone()], false)
            .await;
        assert_refused(&got, CONTENT_REFUSED, &format!("[{what}] apply batch"));
        assert_eq!(weak.revision("kept").await, 1, "[{what}]");
        assert_eq!(weak.revision("other-p").await, 1, "[{what}]");

        let got = weak.put("other-p", harmless, 1).await;
        assert_eq!(got.0, 200, "[{what}] no retention: {}", got.1);
        let reordered = kept(
            "main",
            &format!("  paths:\n    - /srv\n    - /etc\n{EXCLUDE}"),
        );
        let got = weak.put("kept", reordered, 1).await;
        assert_eq!(got.0, 200, "[{what}] same set of paths: {}", got.1);

        let strong = Caller::token(
            &ts,
            &["policies:read", "policies:write", "snapshots:delete"],
        )
        .await;
        let got = strong.put("kept", emptied_kept.clone(), 2).await;
        assert_eq!(got.0, 200, "[{what}] snapshots:delete: {}", got.1);
        let got = strong
            .apply(&[kept("other", &format!("{PATHS}{EXCLUDE}"))], false)
            .await;
        assert_eq!(got.0, 200, "[{what}] snapshots:delete apply: {}", got.1);
        ts.stop().await;
    }
}
