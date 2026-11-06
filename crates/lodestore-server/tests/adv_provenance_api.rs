//! A host with writer credentials plants a snapshot that claims to be the
//! newest snapshot of a live host. What the REST API offers as the host's
//! latest snapshot, and what it lets an operator restore in place, must stay
//! on the agent's real backup. Only public paths are used, so the scenarios
//! run unchanged against the first release.

mod common;
mod forge_support;

use common::{AgentEnv, Backend};
use forge_support::{YEAR_9999, forge_snapshot, signed_backup};
use serde_json::{Value, json};

fn host_dir(env: &AgentEnv) -> std::path::PathBuf {
    let dir = env.work.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("file.txt"), b"the agent's real data").unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_forged_snapshot_year_9999_other_host_not_latest_in_api() {
    let env = AgentEnv::start(&Backend::Sqlite).await;
    let honest = signed_backup(&env, &host_dir(&env)).await;
    let forged = forge_snapshot(&env, YEAR_9999, "forged").await;

    let hosts = env.json("/api/v1/restore/hosts").await;
    let victim = hosts
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["key"] == env.host)
        .expect("the victim host");
    let forged_ms = YEAR_9999 * 1000;
    assert_ne!(
        victim["latest_snapshot_ms"].as_i64(),
        Some(forged_ms),
        "the forged snapshot must not make the host look fresh"
    );
    let received = env.json(&format!("/api/v1/snapshots/{honest}")).await["received_at"].clone();
    assert_eq!(victim["latest_snapshot_ms"], received);

    let page = env
        .json(&format!("/api/v1/hosts/{}/snapshots", env.host))
        .await;
    let items = page["items"].as_array().unwrap();
    let by_id = |id: &str| items.iter().find(|i| i["id"] == id).unwrap();
    assert_eq!(by_id(&honest)["verified_by"], "server");
    assert!(by_id(&forged)["verified_by"].is_null());
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_unverified_not_restorable_in_place() {
    let env = AgentEnv::start(&Backend::Sqlite).await;
    signed_backup(&env, &host_dir(&env)).await;
    let forged = forge_snapshot(&env, YEAR_9999, "forged").await;

    let response = env
        .api
        .post(
            "/api/v1/restores",
            &json!({
                "snapshot_id": forged,
                "items": ["Lw"],
                "target_host_id": env.host,
                "destination": "/",
                "in_place": true,
                "conflict": "overwrite_changed",
                "restore_owners": false,
                "accept_unverified": true,
            }),
        )
        .await;
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["code"], "restore_unverified_in_place", "{body}");
    env.stop().await;
}
