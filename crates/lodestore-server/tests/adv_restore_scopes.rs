//! An API token that was issued for one job must not start a different, more
//! dangerous one: a token for downloads cannot write to a host, and a token for
//! restoring into a new folder cannot restore in place or plant setuid files.
//! Only public paths of the first release are used (REST with scopes as JSON
//! strings), so the scenarios run unchanged against that release.

mod common;

use common::{Api, TestServer, backends, bearer_client};
use serde_json::{Value, json};

async fn token(api: &Api, scopes: &[&str]) -> reqwest::Client {
    let response = api
        .post(
            "/api/v1/tokens",
            &json!({ "name": "adv", "scopes": scopes, "expires_in_days": null }),
        )
        .await;
    assert_eq!(response.status(), 201, "issuing a token with {scopes:?}");
    let body: Value = response.json().await.unwrap();
    bearer_client(body["token"].as_str().unwrap())
}

async fn restore(
    client: &reqwest::Client,
    ts: &TestServer,
    destination: &str,
    in_place: bool,
    restore_owners: bool,
) -> reqwest::Response {
    client
        .post(format!("{}/api/v1/restores", ts.rest_url()))
        .json(&json!({
            "snapshot_id": "0".repeat(64),
            "items": ["Lw"],
            "target_host_id": "host",
            "destination": destination,
            "in_place": in_place,
            "conflict": "skip_existing",
            "restore_owners": restore_owners,
        }))
        .send()
        .await
        .expect("restore request")
}

async fn refused_with(response: reqwest::Response, message: &str) {
    assert_eq!(response.status(), 403);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["code"], "forbidden");
    assert_eq!(body["message"], message);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_download_token_cannot_restore_to_host() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let client = token(&ts.api().await, &["restore:download"]).await;
        let response = restore(&client, &ts, "/srv/out", false, false).await;
        refused_with(
            response,
            "The token needs the restore:host or restore:in-place scope.",
        )
        .await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_host_token_cannot_restore_in_place() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let client = token(&ts.api().await, &["restore:host"]).await;
        let response = restore(&client, &ts, "/", true, false).await;
        refused_with(
            response,
            "The token needs the restore:in-place scope to restore in place.",
        )
        .await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_restore_host_token_cannot_plant_setuid() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let client = token(&ts.api().await, &["restore:host"]).await;
        let response = restore(&client, &ts, "/srv/out", false, true).await;
        refused_with(
            response,
            "The token needs the restore:in-place scope to restore file owners, setuid/setgid bits or file capabilities.",
        )
        .await;
        let response = restore(&client, &ts, "/srv/out", false, false).await;
        let status = response.status().as_u16();
        assert!(
            status != 401 && status != 403,
            "without owners the gate must pass, got {status}"
        );
        ts.stop().await;
    }
}
