//! Hooks run as root on every host a policy reaches, so a token that may only
//! manage policies or host labels must not be able to add a command or to pull
//! more hosts under an existing one. Only public paths of the first release
//! are used (REST with scopes as JSON strings), so the scenarios run unchanged
//! against that release.

mod common;

use common::{TestServer, backends, bearer_client};
use serde_json::{Value, json};

const HOOKS_REFUSED: &str = "The token needs the hooks:write scope to change hooks or the hosts they run on (policies: nightly).";

fn policy(label: &str, hooks: &str) -> String {
    format!(
        "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: nightly\n\
         spec:\n  repository: main\n  selector:\n    matchLabels:\n      role: {label}\n  \
         paths:\n    - /etc\n{hooks}"
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_policies_write_token_cannot_add_hooks() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        ts.create_local_repository("main").await;
        let api = ts.api().await;
        let created = api
            .post("/api/v1/policies", &json!({ "yaml": policy("db", "") }))
            .await;
        assert_eq!(created.status(), 201);

        let token = ts.issue_token(&["policies:read", "policies:write"]).await;
        let response = bearer_client(&token)
            .put(format!("{}/api/v1/policies/nightly", ts.rest_url()))
            .json(&json!({
                "yaml": policy("db", "  hooks:\n    before:\n      - script: dump-db\n"),
                "base_revision": 1,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "[{}]", backend.name());
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["code"], "forbidden");
        assert_eq!(body["message"], HOOKS_REFUSED);

        let stored: Value = api
            .get("/api/v1/policies/nightly")
            .await
            .json()
            .await
            .unwrap();
        assert!(stored["policy"]["spec"].get("hooks").is_none(), "{stored}");
        ts.stop().await;
    }
}
