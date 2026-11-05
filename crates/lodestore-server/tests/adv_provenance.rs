//! A host with writer credentials plants a snapshot that claims to be the
//! newest snapshot of a live host. `latest`, as the server's own repository
//! sees it, must stay on the agent's real backup. Only the public path of a
//! job is used, so the scenario runs unchanged against the first release.

#[path = "../../lodestore-format/tests/adv_support/mod.rs"]
mod adv_support;
mod common;

use adv_support::attacker_keys;
use common::{AgentEnv, Backend};
use lodestore_format::index::parse_hex32;
use lodestore_format::pb;
use lodestore_format::reader::Reader;
use lodestore_format::repo::Repository;
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, Writer};
use serde_json::{Value, json};

const PASSWORD: &[u8] = b"test-password";
const YEAR_9999: i64 = 253_370_764_800;

async fn run_backup(env: &AgentEnv, dir: &std::path::Path) -> String {
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
    body["jobs"][0]["id"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_forged_snapshot_year_9999_not_latest_for_resolve_snapshot() {
    let env = AgentEnv::start(&Backend::Sqlite).await;
    let dir = env.work.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("file.txt"), b"the agent's real data").unwrap();
    let job = run_backup(&env, &dir).await;
    let job = env.wait_job_settled(&job).await;
    let honest = job["snapshot_id"]
        .as_str()
        .expect("a snapshot id")
        .to_string();

    let store = LocalFsStore::new(env.ts().local_repo_root("main"));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let forged_src = env.work.path().join("forged");
    std::fs::create_dir_all(&forged_src).unwrap();
    std::fs::write(forged_src.join("file.txt"), b"forged").unwrap();
    let mut writer = Writer::new(&store, attacker_keys(&repo), repo.config())
        .await
        .unwrap();
    let options = BackupOptions {
        hostname: Some(env.hostname.as_bytes().to_vec()),
        agent_id: Some(env.host.as_bytes().to_vec()),
        time: Some(pb::Timestamp {
            seconds: YEAR_9999,
            nanos: 0,
        }),
        ..Default::default()
    };
    writer
        .backup(&[forged_src], options, &|_| {})
        .await
        .unwrap();

    let reader = Reader::open(&store, repo.read_keys().duplicate(), repo.config()).unwrap();
    let latest = reader.resolve_snapshot("latest").await.unwrap();
    assert_eq!(
        latest.id,
        parse_hex32(&honest).unwrap(),
        "latest must be the agent's snapshot, not the forged one"
    );
    env.stop().await;
}
