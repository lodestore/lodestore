#[path = "../../lodestore-format/tests/adv_support/mod.rs"]
mod adv_support;
mod common;

use std::collections::HashSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use adv_support::{attacker_keys, plant_blobs};
use common::{AgentEnv, TestServer, WAIT, backends, wait_until};
use lodestore_format::attest::{ATTEST_PREFIX, put_attestation, read_attestations};
use lodestore_format::chunker::blob_id;
use lodestore_format::crypto::{BLOB_TYPE_DATA, BlobId, Entropy};
use lodestore_format::index::{hex32, index_object_key, open_shard, parse_hex32};
use lodestore_format::pack::pack_object_key;
use lodestore_format::repo::Repository;
use lodestore_format::snapshot::snapshot_object_key;
use lodestore_format::store::{BlobStore, LocalFsStore};
use lodestore_format::verify_snapshot::snapshot_index_refs;
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_server::alerts::inputs::gather;
use lodestore_server::catalog::open_repository;
use lodestore_server::catalog::retention::after_backup;
use lodestore_server::db::now_ms;
use lodestore_server::maintenance::attest::verify::{SWEEP, verify_then_retain};
use lodestore_server::maintenance::attest::{
    ensure_upgraded, record_verified, resume_interrupted, tick_once,
};
use lodestore_server::maintenance::gc::cycle::{StepOutcome, step};
use lodestore_server::maintenance::gc::{GcOrigin, GcSettings, GcState, db as gc_db, start_cycle};
use lodestore_server::state::AppState;
use serde_json::Value;
use sqlx::Row;

const PASSWORD: &[u8] = b"test-password";
const HOST: &str = "h1";
const POLICY: &str = "daily";

struct Env {
    ts: TestServer,
    repo: String,
    store: LocalFsStore,
}

#[derive(Clone, Copy, PartialEq)]
enum Plant {
    None,
    AfterUpgrade,
    BeforeUpgrade,
}

impl Env {
    async fn new(backend: &common::Backend) -> Env {
        let ts = TestServer::start_with(backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        let store = LocalFsStore::new(ts.local_repo_root("main"));
        let env = Env { ts, repo, store };
        env.host().await;
        env.policy("  retention:\n    keepLast: 1\n").await;
        env
    }

    fn pool(&self) -> &sqlx::AnyPool {
        &self.ts.state().db.pool
    }

    async fn host(&self) {
        sqlx::query(
            "INSERT INTO agents (id, hostname, repository_id, labels_json, os, arch, version, \
             status, enrolled_at, cert_serial, cert_issued_at, cert_not_after, last_seen_at) \
             VALUES ($1, $1, $2, '[]', 'linux', 'x86_64', '0.1.0', 'active', $3, 'serial', $3, $4, $3)",
        )
        .bind(HOST)
        .bind(&self.repo)
        .bind(now_ms() - 3_600_000)
        .bind(now_ms() + 86_400_000_000)
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn policy(&self, extra: &str) {
        let yaml = format!(
            "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: {POLICY}\n\
             spec:\n  repository: main\n  selector:\n    hosts: [{HOST}]\n  paths: [/etc]\n{extra}"
        );
        let policy = lodestore_policy::parse_yaml(&yaml).unwrap();
        sqlx::query(
            "INSERT INTO policies (id, name, repository_id, yaml, spec_json, revision, \
             created_at, updated_at) VALUES ($1, $1, $2, $3, $4, 1, $5, $5)",
        )
        .bind(POLICY)
        .bind(&self.repo)
        .bind(&yaml)
        .bind(serde_json::to_string(&policy).unwrap())
        .bind(now_ms() - 86_400_000)
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn server_keys(&self) -> lodestore_format::keys::ReadKeys {
        open_repository(self.ts.state(), &self.repo)
            .await
            .unwrap()
            .keys
    }

    async fn upgrade(&self) {
        ensure_upgraded(self.ts.state(), &self.repo).await.unwrap();
    }

    async fn writer_keys(&self) -> (Repository, lodestore_format::keys::WriteKeys) {
        let repo = Repository::open(&self.store, PASSWORD).await.unwrap();
        let keys = repo.read_keys().write_keys(repo.config_hash());
        (repo, keys)
    }

    fn source(&self, label: &str) -> (PathBuf, Vec<u8>) {
        let dir = self.ts.dir.path().join(format!("src-{label}"));
        std::fs::create_dir_all(&dir).unwrap();
        let content = format!("content of {label}, unique to this test").into_bytes();
        std::fs::write(dir.join("file.txt"), &content).unwrap();
        (dir, content)
    }

    async fn snapshot(&self, label: &str, plant: Plant) -> (String, Option<BlobId>) {
        let (dir, content) = self.source(label);
        let (repo, keys) = self.writer_keys().await;
        let mut planted = None;
        if plant != Plant::None {
            let id = blob_id(repo.read_keys().k_id(), &content);
            planted = Some(
                plant_blobs(&self.store, &attacker_keys(&repo), &[(id, BLOB_TYPE_DATA)]).await,
            );
        }
        if plant == Plant::BeforeUpgrade {
            self.upgrade().await;
        }
        let mut writer = if plant == Plant::BeforeUpgrade {
            Writer::connect_attested(&self.store, keys, &HashSet::new()).await
        } else {
            Writer::connect(&self.store, keys).await
        }
        .unwrap();
        let out = writer
            .backup(&[dir], BackupOptions::default(), &|_| {})
            .await
            .expect("backup");
        let refs = snapshot_index_refs(&self.store, repo.read_keys(), &out.snapshot_id)
            .await
            .unwrap();
        if let Some(planted) = planted {
            assert!(refs.contains(&planted), "the writer did not deduplicate");
        }
        (hex32(&out.snapshot_id), planted)
    }

    async fn job(&self, id: &str, snapshot_id: &str, verify_state: &str) {
        sqlx::query(
            "INSERT INTO jobs (id, host_id, policy_name, source, state, spec, queued_at, \
             finished_at, snapshot_id, verify_state) \
             VALUES ($1, $2, $3, 'schedule', 'succeeded', $4, $5, $5, $6, $7)",
        )
        .bind(id)
        .bind(HOST)
        .bind(POLICY)
        .bind(Vec::<u8>::new())
        .bind(now_ms())
        .bind(snapshot_id)
        .bind(verify_state)
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn column(&self, job: &str, column: &str) -> Option<String> {
        sqlx::query(
            "SELECT state, verify_state, error_code, error_message, retention_json \
             FROM jobs WHERE id = $1",
        )
        .bind(job)
        .fetch_one(self.pool())
        .await
        .unwrap()
        .try_get(column)
        .unwrap()
    }

    async fn events(&self, job: &str, kind: &str) -> Vec<String> {
        sqlx::query("SELECT message FROM job_events WHERE job_id = $1 AND kind = $2")
            .bind(job)
            .bind(kind)
            .fetch_all(self.pool())
            .await
            .unwrap()
            .iter()
            .map(|r| r.try_get("message").unwrap())
            .collect()
    }

    async fn snapshot_json(&self, id: &str) -> Value {
        let response = self
            .ts
            .api()
            .await
            .get(&format!("/api/v1/snapshots/{id}"))
            .await;
        assert_eq!(response.status(), 200);
        response.json().await.unwrap()
    }

    async fn attested(&self) -> HashSet<BlobId> {
        read_attestations(&self.store, &self.server_keys().await)
            .await
            .unwrap()
            .shards
    }

    async fn verify(&self, job: &str) {
        verify_then_retain(self.ts.state().clone(), job.to_string()).await;
    }

    async fn assert_failed(&self, job: &str, snapshot_id: &str) {
        assert_eq!(self.column(job, "state").await.unwrap(), "failed");
        assert_eq!(
            self.column(job, "error_code").await.unwrap(),
            "verification_failed"
        );
        assert_eq!(self.column(job, "verify_state").await.unwrap(), "failed");
        let events = self.events(job, "verification_failed").await;
        assert_eq!(events.len(), 1);
        let snapshot = self.snapshot_json(snapshot_id).await;
        assert!(
            snapshot["unverified_reason"]
                .as_str()
                .is_some_and(|r| !r.is_empty()),
            "{snapshot}"
        );
        assert_eq!(snapshot["job_id"], job);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_verify_attests_finished_backup() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        ensure_upgraded(env.ts().state(), &env.repo).await.unwrap();
        let dir = env.work.path().join("data");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"alpha alpha alpha").unwrap();
        let snapshot_id = env.backup(&[&dir]).await;

        let pool = &env.ts().state().db.pool;
        let verified = wait_until(WAIT, || async {
            sqlx::query("SELECT verify_state FROM jobs WHERE snapshot_id = $1")
                .bind(&snapshot_id)
                .fetch_one(pool)
                .await
                .unwrap()
                .try_get::<Option<String>, _>("verify_state")
                .unwrap()
                .as_deref()
                == Some("verified")
        })
        .await;
        assert!(verified, "{}", backend.name());

        let source = open_repository(env.ts().state(), &env.repo).await.unwrap();
        let id: BlobId = parse_hex32(&snapshot_id).unwrap();
        let refs = snapshot_index_refs(&source.store, &source.keys, &id)
            .await
            .unwrap();
        let attested = read_attestations(&source.store, &source.keys)
            .await
            .unwrap()
            .shards;
        assert!(!refs.is_empty());
        assert!(refs.iter().all(|r| attested.contains(r)));

        let job = sqlx::query("SELECT state FROM jobs WHERE snapshot_id = $1")
            .bind(&snapshot_id)
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(job.try_get::<String, _>("state").unwrap(), "succeeded");
        let snapshot = env.json(&format!("/api/v1/snapshots/{snapshot_id}")).await;
        assert!(snapshot["unverified_reason"].is_null(), "{snapshot}");
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_verify_fails_job_deduped_against_planted_shard() {
    for backend in backends() {
        let env = Env::new(&backend).await;
        env.upgrade().await;
        let (snapshot_id, planted) = env.snapshot("planted", Plant::AfterUpgrade).await;
        env.job("j1", &snapshot_id, "pending").await;

        env.verify("j1").await;

        env.assert_failed("j1", &snapshot_id).await;
        assert!(
            !env.attested().await.contains(&planted.unwrap()),
            "{}",
            backend.name()
        );
        let reason = env.events("j1", "verification_failed").await.remove(0);
        assert!(reason.contains("could not verify"));
        let inputs = gather(env.ts.state(), now_ms()).await.unwrap();
        let failure = inputs
            .hosts
            .iter()
            .flat_map(|h| &h.pairs)
            .find(|p| p.policy == POLICY)
            .and_then(|p| p.last_failure.as_ref());
        assert!(failure.is_some());
        env.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_verify_fails_job_deduped_against_shard_planted_before_upgrade() {
    for backend in backends() {
        let env = Env::new(&backend).await;
        let (snapshot_id, planted) = env.snapshot("before-upgrade", Plant::BeforeUpgrade).await;
        let planted = planted.unwrap();
        assert!(env.attested().await.contains(&planted));
        env.job("j1", &snapshot_id, "pending").await;

        env.verify("j1").await;

        env.assert_failed("j1", &snapshot_id).await;
        let verified =
            lodestore_server::maintenance::attest::load_verified(env.ts.state(), &env.repo)
                .await
                .unwrap();
        assert!(!verified.contains(&planted), "{}", backend.name());
        env.ts.stop().await;
    }
}

/// A job that names a snapshot the catalog already links to another job
/// fails, and leaves that job's snapshot alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_verify_fails_job_naming_another_jobs_snapshot() {
    for backend in backends() {
        let env = Env::new(&backend).await;
        env.upgrade().await;
        let (snapshot_id, _) = env.snapshot("claimed", Plant::None).await;
        env.job("j0", &snapshot_id, "verified").await;
        let link = lodestore_server::catalog::JobLink {
            job_id: "j0",
            host_id: HOST,
        };
        lodestore_server::catalog::ingest_snapshot(
            env.ts.state(),
            &env.repo,
            &snapshot_id,
            Some(link),
        )
        .await
        .unwrap();
        env.job("j1", &snapshot_id, "pending").await;

        env.verify("j1").await;

        assert_eq!(
            env.column("j1", "verify_state").await.as_deref(),
            Some("failed"),
            "{}",
            backend.name()
        );
        assert_eq!(
            env.column("j1", "error_code").await.as_deref(),
            Some("verification_failed")
        );
        let snapshot = env.snapshot_json(&snapshot_id).await;
        assert_eq!(snapshot["job_id"], "j0");
        assert!(snapshot["unverified_reason"].is_null(), "{snapshot}");
        env.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_verify_resumes_after_restart() {
    for backend in backends() {
        let env = Env::new(&backend).await;
        env.upgrade().await;
        let (snapshot_id, _) = env.snapshot("resume", Plant::None).await;
        let keys = env.server_keys().await;
        let id: BlobId = parse_hex32(&snapshot_id).unwrap();
        let refs = snapshot_index_refs(&env.store, &keys, &id).await.unwrap();
        assert!(!refs.is_empty());
        record_verified(env.ts.state(), &env.repo, &refs[..1])
            .await
            .unwrap();
        env.job("j1", &snapshot_id, "running").await;

        resume_interrupted(env.ts.state()).await.unwrap();
        assert_eq!(env.column("j1", "verify_state").await.unwrap(), "pending");
        tick_once(env.ts.state()).await.unwrap();

        assert_eq!(env.column("j1", "verify_state").await.unwrap(), "verified");
        let attested = env.attested().await;
        assert!(
            refs.iter().all(|r| attested.contains(r)),
            "{}",
            backend.name()
        );
        env.ts.stop().await;
    }
}

/// Checks cut short by a restart are queued again before the server
/// accepts agents, so a job finishing meanwhile is never checked twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_verify_restart_requeues_before_serving() {
    for backend in backends() {
        let mut env = Env::new(&backend).await;
        env.upgrade().await;
        let (snapshot_id, _) = env.snapshot("requeue", Plant::None).await;
        env.job("j1", &snapshot_id, "running").await;

        env.ts.restart().await;

        assert_eq!(
            env.column("j1", "verify_state").await.as_deref(),
            Some("pending"),
            "{}",
            backend.name()
        );
        env.ts.stop().await;
    }
}

/// A job whose log is full still gets an hour of retries counted on the
/// server's clock, not from the finish time its agent reported.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_verify_retry_window_ignores_agent_finish_time() {
    for backend in backends() {
        let env = Env::new(&backend).await;
        let keys = env.server_keys().await;
        put_attestation(&env.store, &keys, &[], &[], Vec::new(), &mut Entropy::os())
            .await
            .unwrap();
        let (snapshot_id, _) = env.snapshot("full-log", Plant::None).await;
        env.job("j1", &snapshot_id, "pending").await;
        sqlx::query("UPDATE jobs SET finished_at = $1, log_lines = 5000 WHERE id = 'j1'")
            .bind(now_ms() - 86_400_000)
            .execute(env.pool())
            .await
            .unwrap();

        env.verify("j1").await;

        assert_eq!(
            env.column("j1", "verify_state").await.as_deref(),
            Some("pending"),
            "[{}] {:?}",
            backend.name(),
            env.column("j1", "error_message").await
        );
        assert_eq!(env.column("j1", "state").await.unwrap(), "succeeded");
        env.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_verify_retention_waits_for_verification() {
    for backend in backends() {
        let env = Env::new(&backend).await;
        env.upgrade().await;
        let (snapshot_id, _) = env.snapshot("retain", Plant::None).await;
        env.job("j1", &snapshot_id, "pending").await;
        assert!(env.column("j1", "retention_json").await.is_none());

        env.verify("j1").await;

        assert_eq!(env.column("j1", "verify_state").await.unwrap(), "verified");
        assert!(
            env.column("j1", "retention_json").await.is_some(),
            "{}",
            backend.name()
        );
        env.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_verify_skips_unchecked_job_and_keeps_retention() {
    for backend in backends() {
        let env = Env::new(&backend).await;
        env.upgrade().await;
        let (snapshot_id, _) = env.snapshot("legacy", Plant::None).await;
        sqlx::query(
            "INSERT INTO jobs (id, host_id, policy_name, source, state, spec, queued_at, \
             finished_at, snapshot_id) VALUES ('j0', $1, $2, 'schedule', 'succeeded', $3, $4, $4, $5)",
        )
        .bind(HOST)
        .bind(POLICY)
        .bind(Vec::<u8>::new())
        .bind(now_ms())
        .bind(&snapshot_id)
        .execute(env.pool())
        .await
        .unwrap();
        env.verify("j0").await;
        assert!(env.column("j0", "retention_json").await.is_some());
        after_backup(env.ts.state(), "j0").await.unwrap();
        env.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_verify_gives_up_after_retry_window() {
    for backend in backends() {
        let env = Env::new(&backend).await;
        let keys = env.server_keys().await;
        put_attestation(&env.store, &keys, &[], &[], Vec::new(), &mut Entropy::os())
            .await
            .unwrap();
        assert_eq!(
            lodestore_format::store::BlobStore::list(&env.store, ATTEST_PREFIX)
                .await
                .unwrap()
                .len(),
            1
        );
        let (snapshot_id, _) = env.snapshot("giveup", Plant::None).await;
        env.job("j1", &snapshot_id, "pending").await;

        env.verify("j1").await;
        env.verify("j1").await;

        assert_eq!(env.column("j1", "state").await.unwrap(), "succeeded");
        assert_eq!(env.column("j1", "verify_state").await.unwrap(), "pending");
        assert_eq!(env.events("j1", "verification_retry").await.len(), 1);

        sqlx::query(
            "UPDATE job_events SET at = at - 3600001 WHERE job_id = 'j1' AND kind = 'verification_retry'",
        )
        .execute(env.pool())
        .await
        .unwrap();
        env.verify("j1").await;

        env.assert_failed("j1", &snapshot_id).await;
        let message = env.column("j1", "error_message").await.unwrap();
        assert!(message.contains("1 hour"), "{message}");
        assert!(
            message.contains("refusing to sign its shards again"),
            "{message}"
        );
        env.ts.stop().await;
    }
}

impl Env {
    async fn enable_gc(&self) {
        sqlx::query("UPDATE agents SET capabilities_json = '[\"gc_v1\"]' WHERE id = $1")
            .bind(HOST)
            .execute(self.pool())
            .await
            .unwrap();
        let settings = GcSettings {
            enabled: true,
            ..GcSettings::default()
        };
        gc_db::save_settings(self.ts.state(), &self.repo, &settings)
            .await
            .unwrap();
    }

    /// Removes the snapshot, so that its packs hold nothing alive, and makes
    /// every pack in the bucket old enough for cleanup.
    fn forget_and_age(&self, snapshot_id: &str) {
        let root = self.ts.local_repo_root("main");
        let id = parse_hex32(snapshot_id).unwrap();
        std::fs::remove_file(root.join(snapshot_object_key(&id))).unwrap();
        let old = SystemTime::now() - Duration::from_secs(3 * 86_400);
        for path in walk(&root.join("packs")) {
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
    }

    /// Steps a new cleanup cycle on the real clock until it waits out the
    /// pause; returns its id and a time past the pause.
    async fn gc_marked(&self) -> (String, i64) {
        let id = start_cycle(
            self.ts.state(),
            &self.repo,
            false,
            GcOrigin::Manual,
            "admin",
        )
        .await
        .unwrap();
        loop {
            match step(self.ts.state(), &id, now_ms()).await.unwrap() {
                StepOutcome::Advanced(_) => {}
                StepOutcome::Waiting { until_ms } => return (id, until_ms + 1_000),
                other => panic!("the cycle was expected to wait out the pause: {other:?}"),
            }
        }
    }
}

/// Steps the cycle on a clock past the pause until it ends.
async fn gc_finish(state: AppState, id: String, later: i64) {
    loop {
        match step(&state, &id, later).await.unwrap() {
            StepOutcome::Advanced(_) => {}
            StepOutcome::Finished(state) => {
                assert_eq!(state, GcState::Done);
                return;
            }
            other => panic!("the cycle stopped on {other:?}"),
        }
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// A backup whose verification has not run yet when a cleanup cycle
/// compacts the index and sweeps: the cycle must not move the backup's own
/// shard into one the server does not vouch for, or the verification that
/// follows fails an honest job.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_verify_backup_pending_through_gc_cycle_is_verified() {
    for backend in backends() {
        let env = Env::new(&backend).await;
        env.upgrade().await;
        env.enable_gc().await;
        let (gone, _) = env.snapshot("gone", Plant::None).await;
        env.forget_and_age(&gone);
        let (snapshot_id, _) = env.snapshot("fresh", Plant::None).await;
        env.job("j1", &snapshot_id, "pending").await;

        let (cycle, later) = env.gc_marked().await;
        gc_finish(env.ts.state().clone(), cycle, later).await;
        env.verify("j1").await;

        assert_eq!(
            env.column("j1", "verify_state").await.as_deref(),
            Some("verified"),
            "[{}] {:?}",
            backend.name(),
            env.column("j1", "error_message").await
        );
        assert_eq!(env.column("j1", "state").await.unwrap(), "succeeded");
        env.ts.stop().await;
    }
}

/// A verification that is reading the bucket holds a cleanup back from
/// deleting: the sweep starts only once the verification lets go.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_verify_gc_sweep_waits_for_running_verification() {
    for backend in backends() {
        let env = Env::new(&backend).await;
        env.upgrade().await;
        env.enable_gc().await;
        let (gone, _) = env.snapshot("gone", Plant::None).await;
        env.forget_and_age(&gone);
        let (cycle, later) = env.gc_marked().await;
        let index = env.ts.local_repo_root("main").join("index");

        let reading = SWEEP.read().await;
        let before = walk(&index);
        let mut finish = tokio::spawn(gc_finish(env.ts.state().clone(), cycle.clone(), later));
        let deciding = wait_until(WAIT, || async {
            let state: String = sqlx::query("SELECT state FROM gc_cycles WHERE id = $1")
                .bind(&cycle)
                .fetch_one(env.pool())
                .await
                .unwrap()
                .get("state");
            state != "waiting" && state != "verifying"
        })
        .await;
        assert!(
            deciding,
            "[{}] the cycle never got past verifying",
            backend.name()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(300), &mut finish)
                .await
                .is_err(),
            "[{}] the sweep ran while a verification was reading",
            backend.name()
        );
        assert!(
            before.iter().all(|p| p.exists()),
            "[{}] a shard went while a verification was reading",
            backend.name()
        );

        drop(reading);
        finish.await.unwrap();
        assert!(
            before.iter().any(|p| !p.exists()),
            "[{}] the cycle deleted no shard, so it proved nothing",
            backend.name()
        );
        env.ts.stop().await;
    }
}

/// A shard that names a pack which never reached the bucket fails the job
/// at once; it is not mistaken for data a cleanup moved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_verify_fails_job_deduped_against_shard_of_missing_pack() {
    for backend in backends() {
        let env = Env::new(&backend).await;
        env.upgrade().await;
        let (snapshot_id, planted) = env.snapshot("no-pack", Plant::AfterUpgrade).await;
        let planted = planted.unwrap();
        let keys = env.server_keys().await;
        let bytes = BlobStore::get(&env.store, &index_object_key(&planted))
            .await
            .unwrap();
        let shard = open_shard(keys.k_idx(), keys.repo_id(), &planted, &bytes).unwrap();
        for record in &shard.packs {
            let pack: BlobId = record.pack_id.as_slice().try_into().unwrap();
            std::fs::remove_file(env.ts.local_repo_root("main").join(pack_object_key(&pack)))
                .unwrap();
        }
        env.job("j1", &snapshot_id, "pending").await;

        env.verify("j1").await;

        env.assert_failed("j1", &snapshot_id).await;
        let message = env.column("j1", "error_message").await.unwrap();
        assert!(
            message.contains("pack is missing"),
            "[{}] {message}",
            backend.name()
        );
        assert!(env.events("j1", "verification_retry").await.is_empty());
        env.ts.stop().await;
    }
}
