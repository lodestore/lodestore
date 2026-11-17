//! A compromised host reports a flood of finished stdin CLI jobs of a policy
//! with `includeStdin` when it reconnects. The run-now backups an admin made
//! under that policy must survive. REST, the agent's own gRPC session, the
//! bucket and SQL over the first release's columns are used, but `includeStdin`
//! does not exist there, so on that release the scenario fails at policy
//! creation (422).

#[path = "../../lodestore-format/tests/it/adv_support/mod.rs"]
mod adv_support;
mod common;

use std::collections::BTreeSet;
use std::path::Path;

use adv_support::attacker_keys;
use common::fake_agent::{Fake, Host, finished, report};
use common::{AgentEnv, Backend};
use lodestore_format::index::hex32;
use lodestore_format::repo::Repository;
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_proto::agent::v1::{JobCreated, JobOutcome, JobReportState, JobSpec};
use lodestore_server::db::now_ms;
use serde_json::{Value, json};
use sqlx::Row;

const PASSWORD: &[u8] = b"test-password";
const POLICY: &str = "keep-three";
const FLOOD: usize = 3;

async fn create_policy(env: &AgentEnv, data: &Path) {
    let yaml = format!(
        "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: {POLICY}\n\
         spec:\n  repository: {}\n  selector:\n    matchLabels:\n      agent: {}\n  \
         paths:\n    - {}\n  retention:\n    keepLast: 3\n    includeStdin: true\n",
        env.repo_name,
        env.hostname,
        data.display()
    );
    let response = env
        .api
        .post("/api/v1/policies", &json!({ "yaml": yaml }))
        .await;
    assert_eq!(response.status(), 201);
}

async fn real_backup(env: &AgentEnv, data: &Path, n: usize) -> String {
    std::fs::write(data.join("file.txt"), format!("the agent's real data {n}")).unwrap();
    let response = env
        .api
        .post(&format!("/api/v1/policies/{POLICY}/run"), &json!({}))
        .await;
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["jobs"][0]["created"], true, "{body}");
    let job = env
        .wait_job_settled(body["jobs"][0]["id"].as_str().unwrap())
        .await;
    assert!(
        job["state"] == "succeeded" || job["state"] == "warnings",
        "{job}"
    );
    job["snapshot_id"]
        .as_str()
        .expect("a snapshot id")
        .to_string()
}

async fn catalog_ids(env: &AgentEnv) -> BTreeSet<String> {
    sqlx::query("SELECT id FROM snapshots")
        .fetch_all(&env.ts().state().db.pool)
        .await
        .unwrap()
        .iter()
        .map(|r| r.try_get("id").unwrap())
        .collect()
}

async fn plant(env: &AgentEnv, data: &Path, i: usize) -> String {
    let store = LocalFsStore::new(env.ts().local_repo_root("main"));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let mut writer = Writer::new(&store, attacker_keys(&repo), repo.config())
        .await
        .unwrap();
    let options = BackupOptions {
        hostname: Some(env.hostname.as_bytes().to_vec()),
        agent_id: Some(env.host.as_bytes().to_vec()),
        tags: vec![
            format!("policy={POLICY}"),
            format!("planted-{i}"),
            "stdin=app.dump".to_string(),
        ],
        ..Default::default()
    };
    let outcome = writer
        .backup(&[data.to_path_buf()], options, &|_| {})
        .await
        .unwrap();
    hex32(&outcome.snapshot_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_reconnect_flood_of_stdin_jobs_does_not_evict_run_now_backups() {
    let mut env = AgentEnv::start(&Backend::Sqlite).await;
    let data = env.work.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    create_policy(&env, &data).await;
    let mut real = Vec::new();
    for n in 1..=3 {
        real.push(real_backup(&env, &data, n).await);
    }

    env.stop_agent().await;
    let mut planted = Vec::new();
    let mut reports = Vec::new();
    for i in 0..FLOOD {
        let snapshot = plant(&env, &data, i).await;
        let job = format!("stdin-flood-{i}");
        let mut r = report(&job, JobReportState::Finished);
        r.created = Some(JobCreated {
            job_id: job.clone(),
            spec: Some(JobSpec {
                policy_name: POLICY.to_string(),
                stdin_name: "app.dump".to_string(),
                paths: vec![data.display().to_string()],
                ..Default::default()
            }),
            caller_uid: 0,
            caller_gid: 0,
            created_at_ms: now_ms(),
        });
        r.finished = Some(finished(&job, JobOutcome::Succeeded, &snapshot));
        reports.push(r);
        planted.push((job, snapshot));
    }
    let host = Host {
        id: env.host.clone(),
        paths: env.paths.clone(),
    };
    let fake = Fake::connect(env.ts(), &host, reports).await;
    for (job, _) in &planted {
        let settled = env.wait_job_settled(job).await;
        assert!(
            settled["state"] == "succeeded" || settled["state"] == "warnings",
            "{settled}"
        );
    }
    drop(fake);

    env.start_agent().await;
    real.push(real_backup(&env, &data, 4).await);

    let catalog = catalog_ids(&env).await;
    let surviving: BTreeSet<String> = real
        .iter()
        .filter(|id| catalog.contains(*id))
        .cloned()
        .collect();
    let expected: BTreeSet<String> = real[1..].iter().cloned().collect();
    assert_eq!(
        surviving, expected,
        "a reconnect flood of stdin jobs must not evict run-now backups"
    );
    for (job, snapshot) in &planted {
        assert!(catalog.contains(snapshot), "planted {snapshot} is gone");
        let linked: Option<String> = sqlx::query("SELECT job_id FROM snapshots WHERE id = $1")
            .bind(snapshot)
            .fetch_one(&env.ts().state().db.pool)
            .await
            .unwrap()
            .try_get("job_id")
            .unwrap();
        assert_eq!(linked.as_deref(), Some(job.as_str()));
    }
    env.stop().await;
}
