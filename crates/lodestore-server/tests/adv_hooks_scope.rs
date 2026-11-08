//! Hooks run as root on every host a policy reaches, so a token that may only
//! manage policies or host labels must not be able to add a command or to pull
//! more hosts under an existing one. Only public paths of the first release
//! are used (REST with scopes as JSON strings), so the scenarios run unchanged
//! against that release.

mod common;

use common::{TestServer, backends, bearer_client};
use lodestore_agent::{EnrollArgs, NetOptions};
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_hosts_write_token_cannot_widen_hook_coverage() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let repo = ts.create_local_repository("main").await;
        let api = ts.api().await;
        let created = api
            .post(
                "/api/v1/policies",
                &json!({ "yaml": policy("db", "  hooks:\n    before:\n      - script: dump-db\n") }),
            )
            .await;
        assert_eq!(created.status(), 201);

        let join = api.join_token(&repo, json!([])).await;
        let host = lodestore_agent::enroll(EnrollArgs {
            token: join,
            paths: ts.agent_paths("victim"),
            hostname: Some("victim".into()),
            net: NetOptions::default(),
        })
        .await
        .expect("enroll")
        .agent_id;

        let token = ts.issue_token(&["hosts:read", "hosts:write"]).await;
        let response = bearer_client(&token)
            .put(format!("{}/api/v1/hosts/{host}/labels", ts.rest_url()))
            .json(&json!({ "labels": [{ "key": "role", "value": "db" }] }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "[{}]", backend.name());
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["code"], "forbidden");
        assert_eq!(body["message"], HOOKS_REFUSED);

        let hosts = api.hosts().await;
        let labels = &hosts.iter().find(|h| h["id"] == host.as_str()).unwrap()["labels"];
        assert_eq!(labels, &json!([]));
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_policies_write_token_cannot_widen_hooked_selector_or_repository() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        ts.create_local_repository("main").await;
        ts.create_local_repository("spare").await;
        let api = ts.api().await;
        let hooks = "  hooks:\n    before:\n      - script: dump-db\n";
        let created = api
            .post("/api/v1/policies", &json!({ "yaml": policy("db", hooks) }))
            .await;
        assert_eq!(created.status(), 201);
        let token = bearer_client(&ts.issue_token(&["policies:read", "policies:write"]).await);
        let put = |yaml: String| {
            token
                .put(format!("{}/api/v1/policies/nightly", ts.rest_url()))
                .json(&json!({ "yaml": yaml, "base_revision": 1 }))
                .send()
        };

        let everyone = policy("db", hooks).replace(
            "  selector:\n    matchLabels:\n      role: db\n",
            "  selector: {}\n",
        );
        let ghost = policy("db", hooks).replace(
            "  selector:\n    matchLabels:\n      role: db\n",
            "  selector:\n    hosts: [agent-that-enrolls-later]\n",
        );
        let elsewhere = policy("db", hooks).replace("repository: main", "repository: spare");
        for (what, yaml) in [
            ("empty selector", everyone),
            ("host id not enrolled yet", ghost),
            ("another repository", elsewhere),
        ] {
            let response = put(yaml).await.unwrap();
            assert_eq!(response.status(), 403, "{what} [{}]", backend.name());
            let body: Value = response.json().await.unwrap();
            assert_eq!(body["message"], HOOKS_REFUSED, "{what}");
        }
        let stored: Value = api
            .get("/api/v1/policies/nightly")
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(stored["revision"], 1, "{stored}");

        let narrower = policy("db", hooks).replace("role: db\n", "role: db\n      tier: prod\n");
        let response = put(narrower).await.unwrap();
        assert_eq!(response.status(), 200, "[{}]", backend.name());
        ts.stop().await;
    }
}
