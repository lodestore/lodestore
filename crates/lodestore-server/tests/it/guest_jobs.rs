use crate::common;

use std::time::Duration;

use common::fake_agent::{Fake, Host, finished, is_ack};
use common::{Api, TestServer, backends, wait_until};
use lodestore_agent::{EnrollArgs, NetOptions};
use lodestore_format::attest::read_attestations;
use lodestore_format::index::{hex32, parse_hex32};
use lodestore_format::provenance::ProvenanceSet;
use lodestore_format::repo::Repository;
use lodestore_format::source::{self, Meta, SourceKey};
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_policy::BackupPolicy;
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{
    GuestKind, JobAccepted, JobOutcome, ProxmoxFacts, ProxmoxGuest, SourceFacts, SourceInstance,
};
use lodestore_server::db::now_ms;
use lodestore_server::jobs::{enqueue, job_spec};
use sqlx::{AnyPool, Row};

const PASSWORD: &[u8] = b"test-password";
const CT_CAPABILITY: &str = "source:proxmox-ct";
const MINUTE_MS: i64 = 60 * 1000;

async fn enroll(ts: &TestServer, api: &Api, repo: &str, hostname: &str) -> Host {
    let token = api.join_token(repo, serde_json::json!([])).await;
    let paths = ts.agent_paths(hostname);
    let enrolled = lodestore_agent::enroll(EnrollArgs {
        token,
        paths: paths.clone(),
        hostname: Some(hostname.to_string()),
        net: NetOptions::default(),
    })
    .await
    .expect("enroll");
    Host {
        id: enrolled.agent_id,
        paths,
    }
}

async fn column(pool: &AnyPool, sql: &'static str, id: &str) -> Option<String> {
    sqlx::query(sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap()
        .and_then(|row| row.try_get::<Option<String>, _>(0).unwrap())
}

async fn count(pool: &AnyPool, sql: &'static str, a: &str, b: &str) -> i64 {
    sqlx::query(sql)
        .bind(a)
        .bind(b)
        .fetch_one(pool)
        .await
        .unwrap()
        .try_get::<i64, _>(0)
        .unwrap()
}

async fn state_of(pool: &AnyPool, id: &str) -> String {
    column(pool, "SELECT state FROM jobs WHERE id = $1", id)
        .await
        .unwrap_or_else(|| "<missing>".to_string())
}

async fn wait_state(pool: &AnyPool, id: &str, want: &str) {
    let reached = wait_until(Duration::from_secs(10), || async {
        state_of(pool, id).await == want
    })
    .await;
    assert!(
        reached,
        "job {id} is '{}', expected '{want}'",
        state_of(pool, id).await
    );
}

fn key_of(vmid: u32) -> String {
    format!("proxmox-ct/pve-prod/{}", uuid_of(vmid))
}

fn uuid_of(vmid: u32) -> String {
    format!("{vmid:08}-1111-4111-8111-111111111111")
}

fn ct_facts(age_ms: u64, plugin_installed: bool, vmids: &[u32]) -> SourceFacts {
    SourceFacts {
        age_ms,
        proxmox: Some(ProxmoxFacts {
            cluster: "pve-prod".to_string(),
            node: "pve1".to_string(),
            plugin_installed,
            guests: vmids
                .iter()
                .map(|vmid| ProxmoxGuest {
                    kind: GuestKind::Ct as i32,
                    vmid: *vmid,
                    name: format!("ct-{vmid}"),
                    uuid: uuid_of(*vmid),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

async fn seed_facts(ts: &TestServer, host: &Host, facts: &SourceFacts) {
    lodestore_server::sources::record(ts.state(), &host.id, facts, &mut None, now_ms())
        .await
        .unwrap();
}

async fn seed_policy(
    ts: &TestServer,
    repo: &str,
    name: &str,
    hosts: &[&Host],
    vmids: &str,
) -> BackupPolicy {
    let ids: Vec<&str> = hosts.iter().map(|h| h.id.as_str()).collect();
    let yaml = format!(
        "apiVersion: lodestore.io/v1alpha2\nkind: BackupPolicy\nmetadata:\n  name: {name}\n\
         spec:\n  repository: main\n  selector:\n    hosts: [{}]\n  source:\n    \
         type: proxmox-ct\n    vmids: {vmids}\n",
        ids.join(", ")
    );
    let policy = lodestore_policy::parse_yaml(&yaml).unwrap();
    sqlx::query(
        "INSERT INTO policies (id, name, repository_id, yaml, spec_json, revision, \
         created_at, updated_at) VALUES ($1, $1, $2, $3, $4, 1, $5, $5)",
    )
    .bind(name)
    .bind(repo)
    .bind(&yaml)
    .bind(serde_json::to_string(&policy).unwrap())
    .bind(now_ms())
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
    policy
}

async fn run_now(api: &Api, name: &str) -> String {
    let run = api
        .post(
            &format!("/api/v1/policies/{name}/run"),
            &serde_json::json!({}),
        )
        .await;
    assert_eq!(run.status(), 200);
    let body: serde_json::Value = run.json().await.unwrap();
    body["jobs"][0]["id"].as_str().unwrap().to_string()
}

fn assigned(body: ServerBody) -> Option<(String, Option<SourceInstance>)> {
    match body {
        ServerBody::JobAssign(a) => Some((a.job_id, a.spec.and_then(|s| s.instance))),
        _ => None,
    }
}

async fn instance_job(agent: &mut Fake) -> (String, SourceInstance) {
    let (job, instance) = agent.recv(assigned).await;
    (job, instance.expect("the job names no instance"))
}

async fn start(agent: &Fake, job: &str) {
    agent
        .send(AgentBody::JobAccepted(JobAccepted {
            job_id: job.to_string(),
            started_at_ms: now_ms(),
        }))
        .await;
}

async fn succeed(agent: &mut Fake, job: &str, snapshot: &str) {
    start(agent, job).await;
    agent
        .send(AgentBody::JobFinished(finished(
            job,
            JobOutcome::Succeeded,
            snapshot,
        )))
        .await;
    assert_eq!(agent.recv(is_ack).await, job);
}

async fn assert_refused(ts: &TestServer, agent: &mut Fake, job: &str, code: &str, backend: &str) {
    let pool = &ts.state().db.pool;
    wait_state(pool, job, "failed").await;
    assert_eq!(
        column(pool, "SELECT error_code FROM jobs WHERE id = $1", job)
            .await
            .as_deref(),
        Some(code),
        "[{backend}]"
    );
    assert!(
        agent
            .drain(Duration::from_millis(300))
            .await
            .into_iter()
            .all(|m| assigned(m).is_none()),
        "[{backend}] the agent got the job"
    );
}

async fn write_snapshot(ts: &TestServer, host: &Host, key: &str, meta: &Meta) -> String {
    let store = LocalFsStore::new(ts.local_repo_root("main"));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    let mut writer = Writer::new(&store, keys, repo.config()).await.unwrap();
    let data = ts.dir.path().join("data-guest");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("file.txt"), "guest").unwrap();
    let outcome = writer
        .backup(
            &[data],
            BackupOptions {
                hostname: Some(b"pve1".to_vec()),
                agent_id: Some(host.id.as_bytes().to_vec()),
                source: source::encode(&SourceKey::parse(key).unwrap(), meta).unwrap(),
                ..BackupOptions::default()
            },
            &|_| {},
        )
        .await
        .expect("backup");
    hex32(&outcome.snapshot_id)
}

async fn signed_source(ts: &TestServer, repo: &str, snapshot: &str) -> Option<(SourceKey, Meta)> {
    let keys = lodestore_server::catalog::open_repository(ts.state(), repo)
        .await
        .unwrap()
        .keys;
    let store = LocalFsStore::new(ts.local_repo_root("main"));
    let attested = read_attestations(&store, &keys).await.unwrap();
    let set = ProvenanceSet::from_attested(&attested, &keys);
    let record = set.get(&parse_hex32(snapshot).unwrap())?;
    Some(source::decode(record.source.as_ref()).unwrap())
}

async fn per_host(pool: &AnyPool, host: &str) -> i64 {
    count(
        pool,
        "SELECT COUNT(*) FROM jobs WHERE policy_id = $1 AND host_id = $2",
        "cts",
        host,
    )
    .await
}

async fn refs(pool: &AnyPool, policy: &str) -> Vec<(String, String)> {
    sqlx::query(
        "SELECT source_ref, state FROM jobs WHERE policy_id = $1 ORDER BY source_ref, queued_at, id",
    )
    .bind(policy)
    .fetch_all(pool)
    .await
    .unwrap()
    .iter()
    .map(|r| (r.get("source_ref"), r.get("state")))
    .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_jobs_expand_per_instance() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "pve1").await;
        seed_policy(&ts, &repo, "cts", &[&host], "[200, 201]").await;
        run_now(&api, "cts").await;
        seed_facts(&ts, &host, &ct_facts(0, true, &[200, 201])).await;

        let mut agent = Fake::connect_with(&ts, &host, &[CT_CAPABILITY]).await;
        let (first, one) = instance_job(&mut agent).await;
        succeed(&mut agent, &first, "").await;
        let (second, two) = instance_job(&mut agent).await;
        succeed(&mut agent, &second, "").await;

        assert_eq!((one.vmid, one.key.as_str()), (200, key_of(200).as_str()));
        assert_eq!((two.vmid, two.key.as_str()), (201, key_of(201).as_str()));
        let pool = &ts.state().db.pool;
        assert_eq!(
            refs(pool, "cts").await,
            [
                (key_of(200), "succeeded".to_string()),
                (key_of(201), "succeeded".to_string())
            ],
            "[{}]",
            backend.name()
        );
        let meta = column(
            pool,
            "SELECT source_meta_json FROM jobs WHERE id = $1",
            &first,
        )
        .await
        .unwrap();
        assert_eq!(meta, r#"{"name":"ct-200","vmid":"200"}"#);
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_jobs_snapshot_bound_to_instance() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "pve1").await;
        seed_policy(&ts, &repo, "cts", &[&host], "[200]").await;
        run_now(&api, "cts").await;
        seed_facts(&ts, &host, &ct_facts(0, true, &[200])).await;

        let mut agent = Fake::connect_with(&ts, &host, &[CT_CAPABILITY]).await;
        let (job, instance) = instance_job(&mut agent).await;
        let meta = Meta::from([
            ("vmid".to_string(), "200".to_string()),
            ("name".to_string(), "ct-200".to_string()),
        ]);
        let snapshot = write_snapshot(&ts, &host, &instance.key, &meta).await;
        succeed(&mut agent, &job, &snapshot).await;

        let pool = &ts.state().db.pool;
        let row = sqlx::query(
            "SELECT source_type, source_ref, source_label FROM snapshots WHERE id = $1",
        )
        .bind(&snapshot)
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("source_type"), "proxmox-ct");
        assert_eq!(row.get::<String, _>("source_ref"), key_of(200));
        assert_eq!(
            row.get::<Option<String>, _>("source_label").as_deref(),
            Some("CT 200 · ct-200")
        );
        let signed = wait_until(Duration::from_secs(10), || async {
            signed_source(&ts, &repo, &snapshot).await.is_some()
        })
        .await;
        assert!(signed, "[{}] the snapshot was not signed", backend.name());
        let (key, _) = signed_source(&ts, &repo, &snapshot).await.unwrap();
        assert_eq!(key.as_str(), key_of(200));
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_jobs_coalesce_per_instance() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "pve1").await;
        seed_policy(&ts, &repo, "cts", &[&host], "[200, 201]").await;
        run_now(&api, "cts").await;
        seed_facts(&ts, &host, &ct_facts(0, true, &[201, 200])).await;
        let pool = &ts.state().db.pool;

        let mut agent = Fake::connect_with(&ts, &host, &[CT_CAPABILITY]).await;
        let (done, one) = instance_job(&mut agent).await;
        assert_eq!(one.vmid, 201);
        succeed(&mut agent, &done, "").await;
        let (running, two) = instance_job(&mut agent).await;
        assert_eq!(two.vmid, 200);
        start(&agent, &running).await;
        wait_state(pool, &running, "running").await;

        let second = run_now(&api, "cts").await;
        seed_facts(&ts, &host, &ct_facts(0, true, &[201, 200])).await;
        let expanded = wait_until(Duration::from_secs(10), || async {
            column(pool, "SELECT source_ref FROM jobs WHERE id = $1", &second)
                .await
                .is_some_and(|r| !r.is_empty())
        })
        .await;
        assert!(
            expanded,
            "[{}] the second run was not expanded",
            backend.name()
        );
        assert_eq!(
            column(pool, "SELECT source_ref FROM jobs WHERE id = $1", &second)
                .await
                .as_deref(),
            Some(key_of(201).as_str())
        );

        assert_eq!(
            count(
                pool,
                "SELECT COUNT(*) FROM jobs WHERE policy_id = $1 AND source_ref = $2",
                "cts",
                &key_of(200)
            )
            .await,
            1,
            "[{}]",
            backend.name()
        );

        let solo = enroll(&ts, &api, &repo, "pve2").await;
        seed_policy(&ts, &repo, "solo", &[&solo], "[200]").await;
        let first = run_now(&api, "solo").await;
        seed_facts(&ts, &solo, &ct_facts(0, true, &[200])).await;
        let mut other = Fake::connect_with(&ts, &solo, &[CT_CAPABILITY]).await;
        let (sent, _) = instance_job(&mut other).await;
        assert_eq!(sent, first);
        start(&other, &sent).await;
        wait_state(pool, &sent, "running").await;
        let again = run_now(&api, "solo").await;
        seed_facts(&ts, &solo, &ct_facts(0, true, &[200])).await;
        wait_state(pool, &again, "cancelled").await;
        assert_eq!(
            column(pool, "SELECT error_code FROM jobs WHERE id = $1", &again)
                .await
                .as_deref(),
            Some("coalesced"),
            "[{}]",
            backend.name()
        );
        drop(other);
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_jobs_wait_for_fresh_facts() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let waits = enroll(&ts, &api, &repo, "waits").await;
        let late = enroll(&ts, &api, &repo, "late").await;
        seed_policy(&ts, &repo, "waits", &[&waits], "[200]").await;
        seed_policy(&ts, &repo, "late", &[&late], "[200]").await;
        let pool = &ts.state().db.pool;

        let job = run_now(&api, "waits").await;
        seed_facts(&ts, &waits, &ct_facts(5000, true, &[200])).await;
        let mut agent = Fake::connect_with(&ts, &waits, &[CT_CAPABILITY]).await;
        assert!(
            agent
                .drain(Duration::from_millis(1500))
                .await
                .into_iter()
                .all(|m| assigned(m).is_none()),
            "[{}] the job went out on a list older than the job",
            backend.name()
        );
        assert_eq!(state_of(pool, &job).await, "queued");
        seed_facts(&ts, &waits, &ct_facts(0, true, &[200])).await;
        let (sent, instance) = instance_job(&mut agent).await;
        assert_eq!((sent.as_str(), instance.vmid), (job.as_str(), 200));

        let stuck = run_now(&api, "late").await;
        sqlx::query("UPDATE jobs SET queued_at = $1 WHERE id = $2")
            .bind(now_ms() - 31 * MINUTE_MS)
            .bind(&stuck)
            .execute(pool)
            .await
            .unwrap();
        let mut other = Fake::connect_with(&ts, &late, &[CT_CAPABILITY]).await;
        assert_refused(&ts, &mut other, &stuck, "source_unknown", backend.name()).await;
        assert_eq!(
            column(pool, "SELECT error_message FROM jobs WHERE id = $1", &stuck)
                .await
                .as_deref(),
            Some("The agent has not listed its containers since the job was queued.")
        );
        drop((agent, other));
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_jobs_not_found_unchanged() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "pve1").await;
        seed_policy(&ts, &repo, "cts", &[&host], "[200]").await;
        let job = run_now(&api, "cts").await;
        seed_facts(&ts, &host, &ct_facts(0, true, &[202])).await;

        let mut agent = Fake::connect_with(&ts, &host, &[CT_CAPABILITY]).await;
        assert_refused(&ts, &mut agent, &job, "source_not_found", backend.name()).await;
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_jobs_plugin_missing() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "pve1").await;
        seed_policy(&ts, &repo, "cts", &[&host], "[200]").await;
        let job = run_now(&api, "cts").await;
        seed_facts(&ts, &host, &ct_facts(0, false, &[200])).await;

        let mut agent = Fake::connect_with(&ts, &host, &[CT_CAPABILITY]).await;
        assert_refused(
            &ts,
            &mut agent,
            &job,
            "guest_plugin_missing",
            backend.name(),
        )
        .await;
        assert_eq!(
            column(
                &ts.state().db.pool,
                "SELECT error_message FROM jobs WHERE id = $1",
                &job
            )
            .await
            .as_deref(),
            Some(
                "The Lodestore plugin is missing or was changed on pve1. \
                 Run lodestore-agent pve install there."
            )
        );
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_not_local_redispatch() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let a = enroll(&ts, &api, &repo, "pve1").await;
        let b = enroll(&ts, &api, &repo, "pve2").await;
        let policy = seed_policy(&ts, &repo, "cts", &[&a, &b], "[200]").await;
        let pool = &ts.state().db.pool;
        enqueue(
            ts.state(),
            "cts",
            "cts",
            &[(a.id.clone(), "pve1".to_string())],
            &job_spec(&policy),
        )
        .await
        .unwrap();
        seed_facts(&ts, &a, &ct_facts(0, true, &[200])).await;
        seed_facts(&ts, &b, &ct_facts(0, true, &[200])).await;

        let mut agent_a = Fake::connect_with(&ts, &a, &[CT_CAPABILITY]).await;
        let (job_a, _) = instance_job(&mut agent_a).await;
        start(&agent_a, &job_a).await;
        let mut moved = finished(&job_a, JobOutcome::Failed, "");
        moved.error_code = "guest_not_local".to_string();
        moved.error_message = "Container 200 is on node pve2, not on pve1.".to_string();
        agent_a.send(AgentBody::JobFinished(moved.clone())).await;
        assert_eq!(agent_a.recv(is_ack).await, job_a);

        assert_eq!(state_of(pool, &job_a).await, "failed");
        assert_eq!(
            column(pool, "SELECT error_code FROM jobs WHERE id = $1", &job_a)
                .await
                .as_deref(),
            Some("guest_not_local")
        );
        assert_eq!(
            column(pool, "SELECT error_message FROM jobs WHERE id = $1", &job_a)
                .await
                .as_deref(),
            Some("Container 200 is on node pve2, not on pve1.")
        );
        agent_a.send(AgentBody::JobFinished(moved)).await;
        assert_eq!(agent_a.recv(is_ack).await, job_a);

        let mut agent_b = Fake::connect_with(&ts, &b, &[CT_CAPABILITY]).await;
        let (job_b, instance) = instance_job(&mut agent_b).await;
        assert_eq!(
            (instance.vmid, instance.key.as_str()),
            (200, key_of(200).as_str())
        );
        assert_eq!(per_host(pool, &b.id).await, 1, "[{}]", backend.name());

        start(&agent_b, &job_b).await;
        let mut back = finished(&job_b, JobOutcome::Failed, "");
        back.error_code = "guest_not_local".to_string();
        agent_b.send(AgentBody::JobFinished(back)).await;
        assert_eq!(agent_b.recv(is_ack).await, job_b);
        assert_eq!(
            per_host(pool, &a.id).await,
            1,
            "[{}] passed back",
            backend.name()
        );
        drop((agent_a, agent_b));
        ts.stop().await;
    }
}
