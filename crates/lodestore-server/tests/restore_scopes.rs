mod common;

use common::{Api, TestServer, backends, bearer_client};
use serde_json::{Value, json};

const NEEDS_WRITE: &str = "The token needs the restore:host or restore:in-place scope.";
const NEEDS_IN_PLACE: &str = "The token needs the restore:in-place scope to restore in place.";
const NEEDS_HOST: &str =
    "The token needs the restore:host scope to restore into a folder on the host.";
const NEEDS_OWNERS: &str = "The token needs the restore:in-place scope to restore file owners, setuid/setgid bits or file capabilities.";
const NEEDS_DOWNLOAD: &str = "The token needs the restore:download scope.";

async fn token(api: &Api, scopes: &[&str]) -> reqwest::Client {
    let response = api
        .post(
            "/api/v1/tokens",
            &json!({ "name": "t", "scopes": scopes, "expires_in_days": null }),
        )
        .await;
    assert_eq!(response.status(), 201);
    let body: Value = response.json().await.unwrap();
    bearer_client(body["token"].as_str().unwrap())
}

fn restore_body(in_place: bool, restore_owners: bool) -> Value {
    json!({
        "snapshot_id": "0".repeat(64),
        "items": ["Lw"],
        "target_host_id": "host",
        "destination": if in_place { "/" } else { "/srv/out" },
        "in_place": in_place,
        "conflict": "skip_existing",
        "restore_owners": restore_owners,
    })
}

async fn post(client: &reqwest::Client, ts: &TestServer, path: &str, body: Value) -> (u16, String) {
    let response = client
        .post(format!("{}{path}", ts.rest_url()))
        .json(&body)
        .send()
        .await
        .expect("request");
    let status = response.status().as_u16();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    (status, body["message"].as_str().unwrap_or("").to_string())
}

fn assert_refused(got: (u16, String), message: &str, what: &str) {
    assert_eq!(got.0, 403, "{what}");
    assert_eq!(got.1, message, "{what}");
}

fn assert_passes_gate(got: (u16, String), what: &str) {
    assert!(got.0 != 401 && got.0 != 403, "{what}: got {got:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_scopes_download_only_cannot_restore() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let download = token(&api, &["restore:download"]).await;
        for in_place in [false, true] {
            let got = post(
                &download,
                &ts,
                "/api/v1/restores",
                restore_body(in_place, false),
            )
            .await;
            assert_refused(
                got,
                NEEDS_WRITE,
                &format!("in_place={in_place} [{}]", backend.name()),
            );
        }
        let got = post(&download, &ts, "/api/v1/downloads", json!({})).await;
        assert_passes_gate(got, "download with restore:download");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_scopes_host_cannot_in_place() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let host = token(&ts.api().await, &["restore:host"]).await;
        let got = post(&host, &ts, "/api/v1/restores", restore_body(true, false)).await;
        assert_refused(got, NEEDS_IN_PLACE, &format!("[{}]", backend.name()));
        let got = post(&host, &ts, "/api/v1/restores", restore_body(false, false)).await;
        assert_passes_gate(got, "new folder with restore:host");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_scopes_in_place_cannot_new_folder() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let in_place = token(&ts.api().await, &["restore:in-place"]).await;
        let got = post(
            &in_place,
            &ts,
            "/api/v1/restores",
            restore_body(false, false),
        )
        .await;
        assert_refused(got, NEEDS_HOST, &format!("[{}]", backend.name()));
        let got = post(
            &in_place,
            &ts,
            "/api/v1/restores",
            restore_body(true, false),
        )
        .await;
        assert_passes_gate(got, "in place with restore:in-place");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_scopes_host_cannot_download() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        for scopes in [&["restore:host"][..], &["restore:in-place"][..]] {
            let client = token(&api, scopes).await;
            let got = post(&client, &ts, "/api/v1/downloads", json!({})).await;
            assert_refused(
                got,
                NEEDS_DOWNLOAD,
                &format!("{scopes:?} [{}]", backend.name()),
            );
        }
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_scopes_host_cannot_restore_owners() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let host = token(&api, &["restore:host"]).await;
        let got = post(&host, &ts, "/api/v1/restores", restore_body(false, true)).await;
        assert_refused(got, NEEDS_OWNERS, &format!("[{}]", backend.name()));
        let got = post(&host, &ts, "/api/v1/restores", restore_body(false, false)).await;
        assert_passes_gate(got, "restore:host without owners");

        let both = token(&api, &["restore:host", "restore:in-place"]).await;
        let got = post(&both, &ts, "/api/v1/restores", restore_body(false, true)).await;
        assert_passes_gate(got, "restore:host + restore:in-place with owners");

        let got = post_session(&api, restore_body(false, true)).await;
        assert!(got != 401 && got != 403, "session with owners: got {got}");
        ts.stop().await;
    }
}

async fn post_session(api: &Api, body: Value) -> u16 {
    api.post("/api/v1/restores", &body).await.status().as_u16()
}

const OUTDATED: &str = "Update the agent on this host to restore to it with an API token.";
const CAPS_V01: &str = r#"["restore_v1","limits_v1","gc_v1"]"#;
const CAPS_ROOTS: &str = r#"["restore_v1","limits_v1","gc_v1","restore_roots_v1"]"#;

async fn enrolled_host(ts: &TestServer, api: &Api, capabilities: &str) -> String {
    let repo = ts.create_local_repository("main").await;
    lodestore_agent::enroll(lodestore_agent::EnrollArgs {
        token: api.join_token(&repo, json!([])).await,
        paths: ts.agent_paths("host-a"),
        hostname: Some("host-a".to_string()),
        net: lodestore_agent::NetOptions::default(),
    })
    .await
    .expect("enroll");
    let host = api.hosts().await[0]["id"].as_str().unwrap().to_string();
    set_capabilities(ts, &host, capabilities).await;
    host
}

async fn set_capabilities(ts: &TestServer, host: &str, capabilities: &str) {
    sqlx::query("UPDATE agents SET capabilities_json = $1 WHERE id = $2")
        .bind(capabilities)
        .bind(host)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
}

async fn code_and_message(
    client: Option<&reqwest::Client>,
    ts: &TestServer,
    api: &Api,
    body: Value,
) -> (u16, String, String) {
    let response = match client {
        Some(client) => client
            .post(format!("{}/api/v1/restores", ts.rest_url()))
            .json(&body)
            .send()
            .await
            .expect("request"),
        None => api.post("/api/v1/restores", &body).await,
    };
    let status = response.status().as_u16();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    (
        status,
        body["code"].as_str().unwrap_or("").to_string(),
        body["message"].as_str().unwrap_or("").to_string(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_scopes_token_needs_new_agent() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let host = enrolled_host(&ts, &api, CAPS_V01).await;
        let client = token(&api, &["restore:host"]).await;
        let mut body = restore_body(false, false);
        body["target_host_id"] = json!(host);

        let (status, code, message) =
            code_and_message(Some(&client), &ts, &api, body.clone()).await;
        assert_eq!(status, 400, "[{}]", backend.name());
        assert_eq!(code, "restore_agent_outdated");
        assert_eq!(message, OUTDATED);

        set_capabilities(&ts, &host, CAPS_ROOTS).await;
        let (status, code, _) = code_and_message(Some(&client), &ts, &api, body).await;
        assert_ne!(code, "restore_agent_outdated", "[{}]", backend.name());
        assert_ne!(status, 400, "[{}]", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_scopes_session_ignores_capability() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let host = enrolled_host(&ts, &api, CAPS_V01).await;
        let mut body = restore_body(false, false);
        body["target_host_id"] = json!(host);
        let (_, code, _) = code_and_message(None, &ts, &api, body).await;
        assert_ne!(code, "restore_agent_outdated", "[{}]", backend.name());
        ts.stop().await;
    }
}
