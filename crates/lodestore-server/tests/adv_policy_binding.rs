//! A compromised host reports snapshots that name the policy of another
//! job. The retention pool of that policy must stay what its own jobs made.
//! Only the public paths of the first release are used (REST, the agent's
//! own gRPC session, the bucket, SQL by v0.1.0 column names,
//! `schedule::tick`), so the scenarios run unchanged against that release.

#[path = "../../lodestore-format/tests/adv_support/mod.rs"]
mod adv_support;
mod common;

use std::collections::BTreeSet;
use std::path::Path;

use adv_support::attacker_keys;
use common::fake_agent::{Fake, Host, finished, is_ack};
use common::{AgentEnv, Backend, TestServer};
use lodestore_format::index::hex32;
use lodestore_format::repo::Repository;
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_policy::jiff::Timestamp;
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::{JobOutcome, JobSpec};
use lodestore_server::db::now_ms;
use lodestore_server::schedule::tick;
use prost::Message;
use serde_json::{Value, json};
use sqlx::{AnyPool, Row};

const PASSWORD: &[u8] = b"test-password";
const SCHEDULE: &str = "  schedule:\n    cron: \"*/5 * * * *\"\n    jitter: 0m\n";
const KEEP_THREE: &str = "  retention:\n    keepLast: 3\n";
const PLANTED: usize = 3;

async fn create_policy(
    api: &common::Api,
    repo_name: &str,
    hostname: &str,
    name: &str,
    data: &Path,
    extra: &str,
) {
    let yaml = format!(
        "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: {name}\n\
         spec:\n  repository: {repo_name}\n  selector:\n    matchLabels:\n      agent: {hostname}\n  \
         paths:\n    - {}\n{extra}",
        data.display()
    );
    let response = api.post("/api/v1/policies", &json!({ "yaml": yaml })).await;
    assert_eq!(response.status(), 201);
}

async fn real_backup(env: &AgentEnv, policy: &str, data: &Path, n: usize) -> String {
    std::fs::write(data.join("file.txt"), format!("the agent's real data {n}")).unwrap();
    let response = env
        .api
        .post(&format!("/api/v1/policies/{policy}/run"), &json!({}))
        .await;
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["jobs"][0]["created"], true, "{body}");
    settled_snapshot(env, body["jobs"][0]["id"].as_str().unwrap()).await
}

async fn scheduled_backup(
    ts: &TestServer,
    env: &AgentEnv,
    data: &Path,
    n: usize,
    slot: &str,
) -> String {
    std::fs::write(data.join("file.txt"), format!("the agent's real data {n}")).unwrap();
    let at: Timestamp = slot.parse().unwrap();
    let queued = tick(ts.state(), at).await.unwrap().queued;
    assert_eq!(queued.len(), 1, "slot {slot}");
    settled_snapshot(env, &queued[0].job_id).await
}

async fn settled_snapshot(env: &AgentEnv, job: &str) -> String {
    let job = env.wait_job_settled(job).await;
    assert!(
        job["state"] == "succeeded" || job["state"] == "warnings",
        "{job}"
    );
    job["snapshot_id"]
        .as_str()
        .expect("a snapshot id")
        .to_string()
}

async fn open_repository(ts: &TestServer) -> (LocalFsStore, Repository) {
    let store = LocalFsStore::new(ts.local_repo_root("main"));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    (store, repo)
}

async fn plant(ts: &TestServer, env: &AgentEnv, data: &Path, policy: &str, i: usize) -> String {
    let (store, repo) = open_repository(ts).await;
    let mut writer = Writer::new(&store, attacker_keys(&repo), repo.config())
        .await
        .unwrap();
    let options = BackupOptions {
        hostname: Some(env.hostname.as_bytes().to_vec()),
        agent_id: Some(env.host.as_bytes().to_vec()),
        tags: vec![format!("policy={policy}"), format!("planted-{i}")],
        ..Default::default()
    };
    let outcome = writer
        .backup(&[data.to_path_buf()], options, &|_| {})
        .await
        .unwrap();
    hex32(&outcome.snapshot_id)
}

async fn report_finished(
    fake: &mut Fake,
    pool: &AnyPool,
    host: &str,
    job: &str,
    policy_name: &str,
    origin: &str,
    snapshot: &str,
) {
    let spec = JobSpec {
        policy_name: policy_name.to_string(),
        ..Default::default()
    };
    sqlx::query(
        "INSERT INTO jobs (id, host_id, source, state, spec, queued_at, assigned_at, \
         policy_name, origin) VALUES ($1, $2, 'manual', 'running', $3, $4, $4, $5, $6)",
    )
    .bind(job)
    .bind(host)
    .bind(spec.encode_to_vec())
    .bind(now_ms())
    .bind(policy_name)
    .bind(origin)
    .execute(pool)
    .await
    .unwrap();
    fake.send(AgentBody::JobFinished(finished(
        job,
        JobOutcome::Succeeded,
        snapshot,
    )))
    .await;
    assert_eq!(fake.recv(is_ack).await, job);
}

async fn catalog_ids(pool: &AnyPool) -> BTreeSet<String> {
    sqlx::query("SELECT id FROM snapshots")
        .fetch_all(pool)
        .await
        .unwrap()
        .iter()
        .map(|r| r.try_get("id").unwrap())
        .collect()
}

async fn sync_catalog(env: &AgentEnv) {
    let response = env
        .api
        .post(
            &format!("/api/v1/repositories/{}/catalog/sync", env.repo),
            &json!({}),
        )
        .await;
    assert_eq!(response.status(), 200);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pool {
    Manual,
    Scheduled,
}

fn server<'a>(env: &'a AgentEnv, standalone: Option<&'a TestServer>) -> &'a TestServer {
    standalone.unwrap_or_else(|| env.ts())
}

async fn planted_tag_scenario(
    mut env: AgentEnv,
    standalone: Option<&TestServer>,
    pool_kind: Pool,
    sync_before_report: bool,
) {
    let data = env.work.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let schedule = match pool_kind {
        Pool::Manual => String::new(),
        Pool::Scheduled => SCHEDULE.to_string(),
    };
    create_policy(
        &env.api,
        &env.repo_name,
        &env.hostname,
        "p",
        &data,
        &format!("{schedule}{KEEP_THREE}"),
    )
    .await;
    create_policy(&env.api, &env.repo_name, &env.hostname, "q", &data, "").await;

    let mut real = Vec::new();
    if pool_kind == Pool::Scheduled {
        tick(
            server(&env, standalone).state(),
            "2026-06-01T10:02:00Z".parse().unwrap(),
        )
        .await
        .unwrap();
    }
    for n in 1..=3 {
        let id = match pool_kind {
            Pool::Manual => real_backup(&env, "p", &data, n).await,
            Pool::Scheduled => {
                let slot = format!("2026-06-01T10:{:02}:10Z", 5 * n);
                scheduled_backup(server(&env, standalone), &env, &data, n, &slot).await
            }
        };
        real.push(id);
    }

    env.stop_agent().await;
    let mut planted = Vec::new();
    for i in 0..PLANTED {
        planted.push(plant(server(&env, standalone), &env, &data, "p", i).await);
    }
    if sync_before_report {
        sync_catalog(&env).await;
        let ids = catalog_ids(&server(&env, standalone).state().db.pool).await;
        assert!(planted.iter().all(|id| ids.contains(id)), "synced");
    }
    let host = Host {
        id: env.host.clone(),
        paths: env.paths.clone(),
    };
    let mut fake = Fake::connect(server(&env, standalone), &host, vec![]).await;
    let origin = match pool_kind {
        Pool::Manual => "manual",
        Pool::Scheduled => "schedule",
    };
    let jobs: Vec<String> = (0..PLANTED).map(|i| format!("planted-job-{i}")).collect();
    for (job, snapshot) in jobs.iter().zip(&planted) {
        report_finished(
            &mut fake,
            &server(&env, standalone).state().db.pool,
            &env.host,
            job,
            "q",
            origin,
            snapshot,
        )
        .await;
    }
    for job in &jobs {
        env.wait_job_settled(job).await;
    }
    drop(fake);

    env.start_agent().await;
    match pool_kind {
        Pool::Manual => real.push(real_backup(&env, "p", &data, 4).await),
        Pool::Scheduled => {
            let id = scheduled_backup(
                server(&env, standalone),
                &env,
                &data,
                4,
                "2026-06-01T10:20:10Z",
            )
            .await;
            real.push(id);
        }
    }

    let pool = &server(&env, standalone).state().db.pool;
    let catalog = catalog_ids(pool).await;
    let surviving: BTreeSet<String> = real
        .iter()
        .filter(|id| catalog.contains(*id))
        .cloned()
        .collect();
    let expected: BTreeSet<String> = real[1..].iter().cloned().collect();
    let kind = match pool_kind {
        Pool::Manual => "real backups",
        Pool::Scheduled => "scheduled backups",
    };
    assert_eq!(
        surviving, expected,
        "keep-last 3 of policy p must keep the three newest {kind}"
    );
    for job in &jobs {
        let settled = env.json(&format!("/api/v1/jobs/{job}")).await;
        assert_eq!(settled["state"], "failed", "{settled}");
        assert_eq!(
            settled["error_code"], "snapshot_policy_mismatch",
            "{settled}"
        );
        let events = env.events(job, "").await;
        assert!(
            events
                .iter()
                .any(|e| e["kind"] == "snapshot_policy_mismatch"),
            "{events:?}"
        );
        let linked: i64 = sqlx::query("SELECT COUNT(*) AS n FROM snapshots WHERE job_id = $1")
            .bind(job)
            .fetch_one(pool)
            .await
            .unwrap()
            .try_get("n")
            .unwrap();
        assert_eq!(linked, 0, "{job} must not own a snapshot");
    }
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_snapshot_tagged_for_another_policy_does_not_enter_its_retention() {
    let env = AgentEnv::start(&Backend::Sqlite).await;
    planted_tag_scenario(env, None, Pool::Manual, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_snapshot_tagged_for_another_policy_does_not_enter_its_scheduled_retention() {
    let ts = TestServer::start_with(&Backend::Sqlite, |c| c.schedule_tick_secs = 0).await;
    let repo = ts.create_local_repository("main").await;
    let env = AgentEnv::start_on(&ts, &repo, "main", "host-a").await;
    planted_tag_scenario(env, Some(&ts), Pool::Scheduled, false).await;
    ts.stop().await;
}
