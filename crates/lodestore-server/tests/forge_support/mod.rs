#![allow(dead_code)]

//! A holder of the repository's writer keys plants a snapshot that names a
//! live agent's host. Only the first release's public API is used.

#[path = "../../../lodestore-format/tests/adv_support/mod.rs"]
mod adv_support;

use adv_support::attacker_keys;
use lodestore_format::index::hex32;
use lodestore_format::pb;
use lodestore_format::repo::Repository;
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, Writer};
use serde_json::{Value, json};

use crate::common::AgentEnv;

pub const PASSWORD: &[u8] = b"test-password";
pub const YEAR_9999: i64 = 253_370_764_800;
pub const YEAR_2000: i64 = 946_684_800;

/// Plants a snapshot in the agent's repository with the agent's hostname and
/// id and the given manifest time, then lets the catalog read it. Returns
/// its snapshot id.
pub async fn forge_snapshot(env: &AgentEnv, seconds: i64, content: &str) -> String {
    let store = LocalFsStore::new(env.ts().local_repo_root("main"));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let source = env.work.path().join(format!("forged-{seconds}-{content}"));
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("file.txt"), content).unwrap();
    let mut writer = Writer::new(&store, attacker_keys(&repo), repo.config())
        .await
        .unwrap();
    let options = BackupOptions {
        hostname: Some(env.hostname.as_bytes().to_vec()),
        agent_id: Some(env.host.as_bytes().to_vec()),
        time: Some(pb::Timestamp { seconds, nanos: 0 }),
        ..Default::default()
    };
    let outcome = writer.backup(&[source], options, &|_| {}).await.unwrap();
    let response = env
        .api
        .post(
            &format!("/api/v1/repositories/{}/catalog/sync", env.repo),
            &json!({}),
        )
        .await;
    assert_eq!(response.status(), 200);
    hex32(&outcome.snapshot_id)
}

/// Runs a backup policy for the agent's host and waits until the server has
/// verified and signed it. Returns the snapshot id.
pub async fn signed_backup(env: &AgentEnv, dir: &std::path::Path) -> String {
    let name = format!("adv-{}", env.hostname);
    let yaml = format!(
        "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: {name}\n\
         spec:\n  repository: {}\n  selector:\n    matchLabels:\n      agent: {}\n  \
         paths:\n    - {}\n",
        env.repo_name,
        env.hostname,
        dir.display()
    );
    let response = env
        .api
        .post("/api/v1/policies", &json!({ "yaml": yaml }))
        .await;
    assert_eq!(response.status(), 201);
    let response = env
        .api
        .post(&format!("/api/v1/policies/{name}/run"), &json!({}))
        .await;
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    let job = env
        .wait_job_settled(body["jobs"][0]["id"].as_str().unwrap())
        .await;
    job["snapshot_id"]
        .as_str()
        .expect("a snapshot id")
        .to_string()
}
