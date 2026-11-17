//! A compromised host reports snapshots that name the policy of another
//! job. The retention pool of that policy must stay what its own jobs made.
//! Only the public paths of the first release are used (REST, the agent's
//! own gRPC session, the bucket, SQL by v0.1.0 column names,
//! `schedule::tick`), so the scenarios run unchanged against that release.

#[path = "../../lodestore-format/tests/it/adv_support/mod.rs"]
mod adv_support;
mod common;

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

use adv_support::attacker_keys;
use common::fake_agent::{Fake, Host, finished, is_ack, report};
use common::{AgentEnv, Backend, TestServer, wait_until};
use lodestore_format::index::hex32;
use lodestore_format::repo::Repository;
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_policy::jiff::Timestamp;
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{JobCreated, JobOutcome, JobReportState, JobSpec};
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
    let tags = vec![
        format!("policy={policy}"),
        format!("planted-{i}"),
        "stdin=app.dump".to_string(),
    ];
    write_snapshot(ts, env, data, tags).await
}

async fn write_snapshot(ts: &TestServer, env: &AgentEnv, data: &Path, tags: Vec<String>) -> String {
    let (store, repo) = open_repository(ts).await;
    let mut writer = Writer::new(&store, attacker_keys(&repo), repo.config())
        .await
        .unwrap();
    let options = BackupOptions {
        hostname: Some(env.hostname.as_bytes().to_vec()),
        agent_id: Some(env.host.as_bytes().to_vec()),
        tags,
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

async fn hang_up(fake: Fake, ts: &TestServer, host: &str) {
    drop(fake);
    assert!(
        wait_until(Duration::from_secs(10), || async {
            !ts.state().registry.is_online(host)
        })
        .await,
        "the server still counts the host as online"
    );
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
    hang_up(fake, server(&env, standalone), &env.host).await;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_known_snapshot_tagged_for_another_policy_is_not_linked_into_its_retention() {
    let env = AgentEnv::start(&Backend::Sqlite).await;
    planted_tag_scenario(env, None, Pool::Manual, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_snapshot_of_an_unknown_host_is_not_linked_into_a_scheduled_retention() {
    let ts = TestServer::start_with(&Backend::Sqlite, |c| c.schedule_tick_secs = 0).await;
    let repo = ts.create_local_repository("main").await;
    let mut env = AgentEnv::start_on(&ts, &repo, "main", "host-a").await;
    let data = env.work.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    create_policy(
        &env.api,
        &env.repo_name,
        &env.hostname,
        "p",
        &data,
        &format!("{SCHEDULE}{KEEP_THREE}"),
    )
    .await;
    tick(ts.state(), "2026-06-01T10:02:00Z".parse().unwrap())
        .await
        .unwrap();

    let victim_data = env.work.path().join("victim");
    std::fs::create_dir_all(&victim_data).unwrap();
    std::fs::write(victim_data.join("file.txt"), b"data of another host").unwrap();
    let (store, opened) = open_repository(&ts).await;
    let keys = opened.read_keys().write_keys(opened.config_hash());
    let mut writer = Writer::new(&store, keys, opened.config()).await.unwrap();
    let options = BackupOptions {
        hostname: Some(b"laptop".to_vec()),
        agent_id: Some(b"ghost-host".to_vec()),
        tags: vec!["policy=p".to_string()],
        ..Default::default()
    };
    let victim = hex32(
        &writer
            .backup(&[victim_data], options, &|_| {})
            .await
            .unwrap()
            .snapshot_id,
    );
    sync_catalog(&env).await;
    let pool = &ts.state().db.pool;
    assert!(catalog_ids(pool).await.contains(&victim));

    for n in 1..=3 {
        let slot = format!("2026-06-01T10:{:02}:10Z", 5 * n);
        scheduled_backup(&ts, &env, &data, n, &slot).await;
    }
    env.stop_agent().await;
    let host = Host {
        id: env.host.clone(),
        paths: env.paths.clone(),
    };
    let mut fake = Fake::connect(&ts, &host, vec![]).await;
    report_finished(&mut fake, pool, &env.host, "atk", "p", "schedule", &victim).await;
    env.wait_job("atk").await;
    hang_up(fake, &ts, &env.host).await;
    env.start_agent().await;
    scheduled_backup(&ts, &env, &data, 4, "2026-06-01T10:20:10Z").await;

    assert!(
        catalog_ids(pool).await.contains(&victim),
        "a snapshot of an unknown host must not be forgotten by the retention of an attacker's job"
    );
    let linked: i64 = sqlx::query("SELECT COUNT(*) AS n FROM snapshots WHERE job_id = 'atk'")
        .fetch_one(pool)
        .await
        .unwrap()
        .try_get("n")
        .unwrap();
    assert_eq!(linked, 0);
    env.stop().await;
    ts.stop().await;
}

const KEEP_THREE_WITH_STDIN: &str = "  retention:\n    keepLast: 3\n    includeStdin: true\n";

async fn wait_job_exists(env: &AgentEnv, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if env.api.get(&format!("/api/v1/jobs/{id}")).await.status() == 200 {
            return;
        }
        assert!(Instant::now() < deadline, "job {id} was not recorded");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn cli_job(fake: &Fake, env: &AgentEnv, id: &str, policy: &str, data: &Path) {
    fake.send(AgentBody::JobCreated(JobCreated {
        job_id: id.to_string(),
        spec: Some(JobSpec {
            policy_name: policy.to_string(),
            stdin_name: "app.dump".to_string(),
            paths: vec![data.display().to_string()],
            ..Default::default()
        }),
        caller_uid: 0,
        caller_gid: 0,
        created_at_ms: now_ms(),
    }))
    .await;
    wait_job_exists(env, id).await;
}

async fn finish_cli(fake: &mut Fake, env: &AgentEnv, id: &str, snapshot: &str) -> Value {
    fake.send(AgentBody::JobFinished(finished(
        id,
        JobOutcome::Succeeded,
        snapshot,
    )))
    .await;
    assert_eq!(fake.recv(is_ack).await, id);
    env.wait_job_settled(id).await
}

async fn relabel(env: &AgentEnv, value: &str) {
    let response = env
        .api
        .put(
            &format!("/api/v1/hosts/{}/labels", env.host),
            &json!({ "labels": [{ "key": "agent", "value": value }] }),
        )
        .await;
    assert_eq!(response.status(), 200);
}

async fn stdin_pool_scenario(relabel_before_create: bool) {
    let mut env = AgentEnv::start(&Backend::Sqlite).await;
    let data = env.work.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    create_policy(
        &env.api,
        &env.repo_name,
        &env.hostname,
        "p",
        &data,
        KEEP_THREE_WITH_STDIN,
    )
    .await;
    env.stop_agent().await;
    let host = Host {
        id: env.host.clone(),
        paths: env.paths.clone(),
    };
    let mut fake = Fake::connect(env.ts(), &host, vec![]).await;

    let mut honest = Vec::new();
    for i in 0..PLANTED {
        let snapshot = plant(env.ts(), &env, &data, "p", i).await;
        let id = format!("honest-{i}");
        cli_job(&fake, &env, &id, "p", &data).await;
        let settled = finish_cli(&mut fake, &env, &id, &snapshot).await;
        assert!(
            settled["state"] == "succeeded" || settled["state"] == "warnings",
            "{settled}"
        );
        honest.push(snapshot);
    }

    let ids: Vec<String> = (0..PLANTED).map(|i| format!("cli-{i}")).collect();
    let mut snapshots = Vec::new();
    if relabel_before_create {
        relabel(&env, "elsewhere").await;
        for (i, id) in ids.iter().enumerate() {
            snapshots.push(plant(env.ts(), &env, &data, "p", 10 + i).await);
            cli_job(&fake, &env, id, "p", &data).await;
            finish_cli(&mut fake, &env, id, &snapshots[i]).await;
        }
    } else {
        for id in &ids {
            cli_job(&fake, &env, id, "p", &data).await;
            let created = env.json(&format!("/api/v1/jobs/{id}")).await;
            assert_eq!(created["policy_name"], "p", "{created}");
        }
        relabel(&env, "elsewhere").await;
        for (i, id) in ids.iter().enumerate() {
            snapshots.push(plant(env.ts(), &env, &data, "p", 10 + i).await);
            finish_cli(&mut fake, &env, id, &snapshots[i]).await;
        }
    }

    let catalog = catalog_ids(&env.ts().state().db.pool).await;
    for snapshot in &honest {
        assert!(
            catalog.contains(snapshot),
            "retention of policy p must not run from a CLI job of a host it no longer selects"
        );
    }
    for id in &ids {
        let settled = env.json(&format!("/api/v1/jobs/{id}")).await;
        assert_eq!(settled["state"], "failed", "{settled}");
        assert_eq!(
            settled["error_code"], "snapshot_policy_mismatch",
            "{settled}"
        );
        assert!(settled["policy_name"].is_null(), "{settled}");
    }
    drop(fake);
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_cli_job_naming_a_policy_that_left_the_host_does_not_run_its_retention() {
    stdin_pool_scenario(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_cli_job_whose_policy_left_the_host_before_it_finished_does_not_run_its_retention() {
    stdin_pool_scenario(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_reconnect_flood_of_cli_jobs_without_a_stream_does_not_evict_run_now_backups() {
    let mut env = AgentEnv::start(&Backend::Sqlite).await;
    let data = env.work.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    create_policy(
        &env.api,
        &env.repo_name,
        &env.hostname,
        "p",
        &data,
        KEEP_THREE,
    )
    .await;
    let mut real = Vec::new();
    for n in 1..=3 {
        real.push(real_backup(&env, "p", &data, n).await);
    }

    env.stop_agent().await;
    let mut planted = Vec::new();
    let mut reports = Vec::new();
    for i in 0..PLANTED {
        let snapshot = plant(env.ts(), &env, &data, "p", i).await;
        let id = format!("cli-flood-{i}");
        let mut r = report(&id, JobReportState::Finished);
        r.created = Some(JobCreated {
            job_id: id.clone(),
            spec: Some(JobSpec {
                policy_name: "p".to_string(),
                paths: vec![data.display().to_string()],
                ..Default::default()
            }),
            caller_uid: 0,
            caller_gid: 0,
            created_at_ms: now_ms(),
        });
        r.finished = Some(finished(&id, JobOutcome::Succeeded, &snapshot));
        reports.push(r);
        planted.push((id, snapshot));
    }
    let host = Host {
        id: env.host.clone(),
        paths: env.paths.clone(),
    };
    let mut fake = Fake::connect(env.ts(), &host, reports).await;
    let seen = fake.drain(Duration::from_millis(1500)).await;
    for (id, _) in &planted {
        if env.api.get(&format!("/api/v1/jobs/{id}")).await.status() == 200 {
            env.wait_job_settled(id).await;
        }
    }
    hang_up(fake, env.ts(), &env.host).await;

    env.start_agent().await;
    real.push(real_backup(&env, "p", &data, 4).await);

    let pool = &env.ts().state().db.pool;
    let catalog = catalog_ids(pool).await;
    let surviving: BTreeSet<String> = real
        .iter()
        .filter(|id| catalog.contains(*id))
        .cloned()
        .collect();
    let expected: BTreeSet<String> = real[1..].iter().cloned().collect();
    assert_eq!(
        surviving, expected,
        "a reconnect flood of CLI jobs without a stream must not evict run-now backups"
    );
    for (id, snapshot) in &planted {
        assert!(
            seen.iter()
                .any(|b| matches!(b, ServerBody::JobForget(f) if &f.job_id == id)),
            "{id} was not forgotten"
        );
        for (sql, what) in [
            ("SELECT COUNT(*) AS n FROM jobs WHERE id = $1", "a job row"),
            (
                "SELECT COUNT(*) AS n FROM snapshots WHERE job_id = $1",
                "a linked snapshot",
            ),
        ] {
            let n: i64 = sqlx::query(sql)
                .bind(id)
                .fetch_one(pool)
                .await
                .unwrap()
                .try_get("n")
                .unwrap();
            assert_eq!(n, 0, "{id} must not leave {what} ({snapshot})");
        }
    }
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_cli_stream_job_cannot_pull_a_backup_of_its_host_into_the_stdin_pool() {
    let mut env = AgentEnv::start(&Backend::Sqlite).await;
    let data = env.work.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    create_policy(
        &env.api,
        &env.repo_name,
        &env.hostname,
        "p",
        &data,
        KEEP_THREE_WITH_STDIN,
    )
    .await;
    std::fs::write(data.join("file.txt"), b"the host's real data").unwrap();
    let victim = write_snapshot(env.ts(), &env, &data, vec!["policy=p".to_string()]).await;
    sync_catalog(&env).await;
    assert!(
        catalog_ids(&env.ts().state().db.pool)
            .await
            .contains(&victim)
    );

    env.stop_agent().await;
    let host = Host {
        id: env.host.clone(),
        paths: env.paths.clone(),
    };
    let mut fake = Fake::connect(env.ts(), &host, vec![]).await;
    cli_job(&fake, &env, "atk", "p", &data).await;
    let settled = finish_cli(&mut fake, &env, "atk", &victim).await;
    assert_eq!(settled["state"], "failed", "{settled}");
    for i in 0..PLANTED {
        let snapshot = plant(env.ts(), &env, &data, "p", i).await;
        let id = format!("stream-{i}");
        cli_job(&fake, &env, &id, "p", &data).await;
        finish_cli(&mut fake, &env, &id, &snapshot).await;
    }

    let pool = &env.ts().state().db.pool;
    assert!(
        catalog_ids(pool).await.contains(&victim),
        "a CLI stream job must not pull a backup that is not its stream into the stdin pool"
    );
    let linked: i64 = sqlx::query("SELECT COUNT(*) AS n FROM snapshots WHERE job_id = 'atk'")
        .fetch_one(pool)
        .await
        .unwrap()
        .try_get("n")
        .unwrap();
    assert_eq!(linked, 0);
    drop(fake);
    env.stop().await;
}
