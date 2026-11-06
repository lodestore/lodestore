//! A compromised host reports snapshots that the server's retention and
//! "last backup" bookkeeping must not trust. Only the public paths of the
//! first release are used (REST, the agent's own gRPC session, the bucket),
//! so the scenarios run unchanged against that release.

#[path = "../../lodestore-format/tests/adv_support/mod.rs"]
mod adv_support;
mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use adv_support::attacker_keys;
use common::fake_agent::{Fake, Host, finished, is_ack};
use common::{AgentEnv, Backend};
use lodestore_format::crypto::Entropy;
use lodestore_format::index::hex32;
use lodestore_format::repo::Repository;
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::{JobOutcome, JobSpec};
use lodestore_server::db::now_ms;
use prost::Message;
use serde_json::{Value, json};
use sqlx::Row;

const PASSWORD: &[u8] = b"test-password";
const POLICY: &str = "keep-three";
const FORGED: usize = 100;
const THREE_DAYS_MS: i64 = 3 * 24 * 3600 * 1000;

async fn create_policy(env: &AgentEnv, data: &Path) {
    let yaml = format!(
        "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: {POLICY}\n\
         spec:\n  repository: {}\n  selector:\n    matchLabels:\n      agent: {}\n  \
         paths:\n    - {}\n  retention:\n    keepLast: 3\n",
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

fn files_below(dir: &Path, out: &mut BTreeSet<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files_below(&path, out);
        } else {
            out.insert(path);
        }
    }
}

/// Writes `FORGED` snapshots that name the host and carry data of their
/// own, then ruins the packs that data went to: the snapshots exist in the
/// bucket but nothing they point at resolves.
async fn forge_unresolvable_snapshots(env: &AgentEnv) -> Vec<String> {
    let root = env.ts().local_repo_root("main");
    let store = LocalFsStore::new(root.clone());
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let source = env.work.path().join("forged");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("file.txt"), b"data of the forger").unwrap();
    let mut before = BTreeSet::new();
    files_below(&root.join("packs"), &mut before);

    let mut ids = Vec::new();
    for i in 0..FORGED {
        let mut writer = Writer::new(&store, attacker_keys(&repo), repo.config())
            .await
            .unwrap();
        let options = BackupOptions {
            hostname: Some(env.hostname.as_bytes().to_vec()),
            agent_id: Some(env.host.as_bytes().to_vec()),
            tags: vec![format!("policy={POLICY}"), format!("forged-{i}")],
            ..Default::default()
        };
        let outcome = writer
            .backup(std::slice::from_ref(&source), options, &|_| {})
            .await
            .unwrap();
        ids.push(hex32(&outcome.snapshot_id));
    }

    let mut after = BTreeSet::new();
    files_below(&root.join("packs"), &mut after);
    let planted: Vec<_> = after.difference(&before).collect();
    assert!(!planted.is_empty(), "the forger's data went nowhere");
    for pack in planted {
        let mut junk = vec![0u8; std::fs::metadata(pack).unwrap().len() as usize];
        Entropy::os().fill(&mut junk);
        std::fs::write(pack, junk).unwrap();
    }
    ids
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_hundred_unverified_snapshots_do_not_displace_retention() {
    let mut env = AgentEnv::start(&Backend::Sqlite).await;
    let data = env.work.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    create_policy(&env, &data).await;
    let mut real = Vec::new();
    for n in 1..=3 {
        real.push(real_backup(&env, &data, n).await);
    }

    env.stop_agent().await;
    let forged = forge_unresolvable_snapshots(&env).await;
    let host = Host {
        id: env.host.clone(),
        paths: env.paths.clone(),
    };
    let mut fake = Fake::connect(env.ts(), &host, vec![]).await;
    let pool = &env.ts().state().db.pool;
    let spec = JobSpec {
        policy_name: POLICY.to_string(),
        paths: vec![data.display().to_string()],
        ..Default::default()
    };
    for (i, snapshot) in forged.iter().enumerate() {
        let job = format!("forged-job-{i}");
        sqlx::query(
            "INSERT INTO jobs (id, host_id, source, state, spec, queued_at, assigned_at, \
             policy_name) VALUES ($1, $2, 'cli', 'running', $3, $4, $4, $5)",
        )
        .bind(&job)
        .bind(&env.host)
        .bind(spec.encode_to_vec())
        .bind(now_ms())
        .bind(POLICY)
        .execute(pool)
        .await
        .unwrap();
        fake.send(AgentBody::JobFinished(finished(
            &job,
            JobOutcome::Succeeded,
            snapshot,
        )))
        .await;
        assert_eq!(fake.recv(is_ack).await, job);
    }
    for i in 0..FORGED {
        env.wait_job_settled(&format!("forged-job-{i}")).await;
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
        "keep-last 3 must keep the three newest real backups"
    );
    let forged_left = forged.iter().filter(|id| catalog.contains(*id)).count();
    assert_eq!(forged_left, FORGED, "no forged snapshot is deleted");
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_forged_snapshot_does_not_mask_overdue_backup() {
    let env = AgentEnv::start(&Backend::Sqlite).await;
    let data = env.work.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    create_policy(&env, &data).await;
    let honest = real_backup(&env, &data, 1).await;
    sqlx::query("UPDATE snapshots SET received_at = received_at - $1 WHERE id = $2")
        .bind(THREE_DAYS_MS)
        .bind(&honest)
        .execute(&env.ts().state().db.pool)
        .await
        .unwrap();
    let received: i64 = sqlx::query("SELECT received_at FROM snapshots WHERE id = $1")
        .bind(&honest)
        .fetch_one(&env.ts().state().db.pool)
        .await
        .unwrap()
        .try_get("received_at")
        .unwrap();

    let store = LocalFsStore::new(env.ts().local_repo_root("main"));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let source = env.work.path().join("forged");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("file.txt"), b"data of the forger").unwrap();
    let mut writer = Writer::new(&store, attacker_keys(&repo), repo.config())
        .await
        .unwrap();
    let options = BackupOptions {
        hostname: Some(env.hostname.as_bytes().to_vec()),
        agent_id: Some(env.host.as_bytes().to_vec()),
        ..Default::default()
    };
    writer.backup(&[source], options, &|_| {}).await.unwrap();
    let response = env
        .api
        .post(
            &format!("/api/v1/repositories/{}/catalog/sync", env.repo),
            &json!({}),
        )
        .await;
    assert_eq!(response.status(), 200);

    let fleet = env.json("/api/v1/fleet").await;
    let host = fleet["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["id"] == env.host)
        .expect("the victim host");
    assert_eq!(
        host["last_snapshot"]["time"].as_i64(),
        Some(received),
        "a planted snapshot must not make the host look freshly backed up"
    );
    env.stop().await;
}
