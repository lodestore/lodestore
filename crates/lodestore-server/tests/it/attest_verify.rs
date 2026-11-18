use crate::adv_support;
use crate::common;

use std::cell::RefCell;
use std::collections::{BTreeSet, HashSet};
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
use lodestore_format::pb;
use lodestore_format::provenance::{
    ALL_SIGNED_STORED_IN_FUTURE, FUTURE_TOLERANCE_MS, LatestScope, LatestWarning,
    NO_VERIFIED_SNAPSHOT, ProvenanceSet,
};
use lodestore_format::reader::Reader;
use lodestore_format::repo::Repository;
use lodestore_format::snapshot::snapshot_object_key;
use lodestore_format::source::{self, Meta, SourceKey};
use lodestore_format::store::{BlobError, BlobStore, ListEntry, LocalFsStore, put_named};
use lodestore_format::verify_snapshot::snapshot_index_refs;
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_server::alerts::inputs::gather;
use lodestore_server::catalog::retention::{after_backup, plan_policy};
use lodestore_server::catalog::{open_repository, sync_repository};
use lodestore_server::db::now_ms;
use lodestore_server::maintenance::attest::verify::{SWEEP, verify_then_retain};
use lodestore_server::maintenance::attest::{
    ensure_upgraded, record_verified, resume_interrupted, tick_once,
};
use lodestore_server::maintenance::gc::cycle::{StepOutcome, step};
use lodestore_server::maintenance::gc::{GcOrigin, GcSettings, GcState, db as gc_db, start_cycle};
use lodestore_server::state::AppState;
use proptest::prelude::{Just, ProptestConfig, Strategy, prop_assert, prop_assert_eq, prop_oneof};
use proptest::test_runner::{TestCaseError, TestRunner};
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
            Writer::connect_attested(&self.store, keys, &HashSet::new(), &HashSet::new()).await
        } else {
            Writer::connect(&self.store, keys).await
        }
        .unwrap();
        let out = writer
            .backup(
                &[dir],
                BackupOptions {
                    agent_id: Some(HOST.as_bytes().to_vec()),
                    ..BackupOptions::default()
                },
                &|_| {},
            )
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
        self.job_row(HOST, id, snapshot_id, verify_state).await;
    }

    async fn job_row(&self, host: &str, id: &str, snapshot_id: &str, verify_state: &str) {
        sqlx::query(
            "INSERT INTO jobs (id, host_id, policy_name, source, state, spec, queued_at, \
             finished_at, snapshot_id, verify_state) \
             VALUES ($1, $2, $3, 'schedule', 'succeeded', $4, $5, $5, $6, $7)",
        )
        .bind(id)
        .bind(host)
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

/// A host's shard whose frames open but which lists shards it supersedes
/// may serve its own snapshot, but the server never attests it: that would
/// let a host hide other shards from readers and cleanup.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_verify_does_not_attest_superseding_host_shard() {
    for backend in backends() {
        let env = Env::new(&backend).await;
        env.upgrade().await;
        let (original, _) = env.snapshot("supersedes", Plant::None).await;
        let keys = env.server_keys().await;
        let id: BlobId = parse_hex32(&original).unwrap();
        let bytes = BlobStore::get(&env.store, &snapshot_object_key(&id))
            .await
            .unwrap();
        let mut snapshot = lodestore_format::snapshot::open_snapshot(
            keys.hpke_private_key(),
            keys.repo_id(),
            &id,
            &bytes,
        )
        .unwrap();
        let honest: BlobId = snapshot.index_refs[0].as_slice().try_into().unwrap();
        let bytes = BlobStore::get(&env.store, &index_object_key(&honest))
            .await
            .unwrap();
        let shard = open_shard(keys.k_idx(), keys.repo_id(), &honest, &bytes).unwrap();
        let (forged, bytes) = lodestore_format::index::seal_shard_superseding(
            keys.k_idx(),
            keys.repo_id(),
            shard.packs,
            vec![[7u8; 32]],
            &mut Entropy::os(),
        )
        .unwrap();
        put_named(&env.store, &index_object_key(&forged), bytes)
            .await
            .unwrap();
        snapshot.index_refs[0] = forged.to_vec();
        let (rewritten, bytes) = lodestore_format::snapshot::seal_snapshot(
            &snapshot,
            keys.hpke_public_key(),
            keys.repo_id(),
            &mut Entropy::os(),
        )
        .unwrap();
        put_named(&env.store, &snapshot_object_key(&rewritten), bytes)
            .await
            .unwrap();
        env.job("j1", &hex32(&rewritten), "pending").await;

        env.verify("j1").await;

        assert_eq!(
            env.column("j1", "verify_state").await.as_deref(),
            Some("verified"),
            "[{}] {:?}",
            backend.name(),
            env.column("j1", "error_message").await
        );
        assert!(
            !env.attested().await.contains(&forged),
            "[{}] a host's superseding shard was attested",
            backend.name()
        );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_verify_fails_job_with_foreign_policy_tag() {
    for backend in backends() {
        let env = Env::new(&backend).await;
        env.upgrade().await;
        let (dir, _) = env.source("foreign-tag");
        let (_, keys) = env.writer_keys().await;
        let mut writer = Writer::connect(&env.store, keys).await.unwrap();
        let out = writer
            .backup(
                &[dir],
                BackupOptions {
                    agent_id: Some(HOST.as_bytes().to_vec()),
                    tags: vec!["policy=other".to_string()],
                    ..BackupOptions::default()
                },
                &|_| {},
            )
            .await
            .expect("backup");
        let snapshot_id = hex32(&out.snapshot_id);
        env.job("j1", &snapshot_id, "pending").await;

        env.verify("j1").await;

        assert_eq!(env.column("j1", "state").await.as_deref(), Some("failed"));
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
        let message = env.column("j1", "error_message").await.unwrap();
        assert!(message.contains("tagged for policy"), "{message}");
        let linked: i64 = sqlx::query("SELECT COUNT(*) AS n FROM snapshots WHERE job_id = 'j1'")
            .fetch_one(env.pool())
            .await
            .unwrap()
            .try_get("n")
            .unwrap();
        assert_eq!(linked, 0);
        env.ts.stop().await;
    }
}

#[tokio::test]
async fn attest_verify_clips_a_long_foreign_policy_tag_in_the_job_error() {
    for backend in backends() {
        let env = Env::new(&backend).await;
        env.upgrade().await;
        let (dir, _) = env.source("long-tag");
        let (_, keys) = env.writer_keys().await;
        let mut writer = Writer::connect(&env.store, keys).await.unwrap();
        let out = writer
            .backup(
                &[dir],
                BackupOptions {
                    agent_id: Some(HOST.as_bytes().to_vec()),
                    tags: vec![format!("policy={}", "x".repeat(100_000))],
                    ..BackupOptions::default()
                },
                &|_| {},
            )
            .await
            .expect("backup");
        let snapshot_id = hex32(&out.snapshot_id);
        env.job("j1", &snapshot_id, "pending").await;

        env.verify("j1").await;

        assert_eq!(
            env.column("j1", "error_code").await.as_deref(),
            Some("verification_failed"),
            "{}",
            backend.name()
        );
        let message = env.column("j1", "error_message").await.unwrap();
        assert!(message.contains("tagged for policy"), "{}", backend.name());
        assert!(
            message.len() < 4096 + 256,
            "{}: {}",
            backend.name(),
            message.len()
        );
        env.ts.stop().await;
    }
}

const GUEST_A: &str = "proxmox-vm/pve1/11111111-1111-4111-8111-111111111111";
const GUEST_B: &str = "proxmox-vm/pve1/22222222-2222-4222-8222-222222222222";

fn claim(key: &str) -> Option<pb::Source> {
    Some(pb::Source {
        key: key.to_string(),
        meta: Vec::new(),
    })
}

impl Env {
    async fn snapshot_claiming(&self, label: &str, source: Option<pb::Source>) -> String {
        self.snapshot_on(HOST, label, source, None).await
    }

    async fn snapshot_on(
        &self,
        host: &str,
        label: &str,
        source: Option<pb::Source>,
        time: Option<pb::Timestamp>,
    ) -> String {
        let (dir, _) = self.source(label);
        let (_, keys) = self.writer_keys().await;
        let mut writer = Writer::connect(&self.store, keys).await.unwrap();
        let out = writer
            .backup(
                &[dir],
                BackupOptions {
                    agent_id: Some(host.as_bytes().to_vec()),
                    source,
                    time,
                    ..BackupOptions::default()
                },
                &|_| {},
            )
            .await
            .expect("backup");
        hex32(&out.snapshot_id)
    }

    async fn job_of_source(&self, id: &str, snapshot_id: &str, key: &str, meta: Option<&str>) {
        self.job_on(HOST, id, snapshot_id, key, meta).await;
    }

    async fn job_on(&self, host: &str, id: &str, snapshot_id: &str, key: &str, meta: Option<&str>) {
        self.job_row(host, id, snapshot_id, "pending").await;
        sqlx::query("UPDATE jobs SET source_ref = $1, source_meta_json = $2 WHERE id = $3")
            .bind(key)
            .bind(meta)
            .bind(id)
            .execute(self.pool())
            .await
            .unwrap();
    }

    async fn signed_source(&self, snapshot_id: &str) -> Option<(SourceKey, Meta)> {
        let keys = self.server_keys().await;
        let set = read_attestations(&self.store, &keys).await.unwrap();
        let provenance = ProvenanceSet::from_attested(&set, &keys);
        let record = provenance.get(&parse_hex32(snapshot_id).unwrap())?;
        Some(source::decode(record.source.as_ref()).unwrap())
    }

    async fn catalog_source(&self, snapshot_id: &str) -> (String, String, Option<String>, bool) {
        let row = sqlx::query(
            "SELECT source_type, source_ref, source_label, verified_at FROM snapshots WHERE id = $1",
        )
        .bind(snapshot_id)
        .fetch_one(self.pool())
        .await
        .unwrap();
        (
            row.try_get("source_type").unwrap(),
            row.try_get("source_ref").unwrap(),
            row.try_get("source_label").unwrap(),
            row.try_get::<Option<i64>, _>("verified_at")
                .unwrap()
                .is_some(),
        )
    }

    async fn assert_refused_as_foreign_source(&self, job: &str, snapshot_id: &str) {
        self.assert_failed(job, snapshot_id).await;
        let message = self.column(job, "error_message").await.unwrap();
        assert!(
            message.contains("names another source than its job"),
            "{message}"
        );
        assert_unsigned_and_out_of_retention(self, snapshot_id).await;
    }
}

async fn assert_unsigned_and_out_of_retention(env: &Env, snapshot_id: &str) {
    assert!(
        env.signed_source(snapshot_id).await.is_none(),
        "a signed record exists for {snapshot_id}"
    );
    let (_, _, _, verified) = env.catalog_source(snapshot_id).await;
    assert!(!verified, "{snapshot_id} is marked verified");
    let row = sqlx::query("SELECT spec_json FROM policies WHERE name = $1")
        .bind(POLICY)
        .fetch_one(env.pool())
        .await
        .unwrap();
    let policy: lodestore_policy::BackupPolicy =
        serde_json::from_str(&row.try_get::<String, _>("spec_json").unwrap()).unwrap();
    let rules = policy.spec.retention.clone().unwrap();
    let plan = plan_policy(env.ts.state(), &policy, &rules).await.unwrap();
    assert!(
        !plan
            .hosts
            .iter()
            .flat_map(|h| &h.items)
            .any(|i| i.id == snapshot_id),
        "{snapshot_id} is in the retention plan"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_verify_signs_the_source_of_its_job() {
    for backend in backends() {
        let env = Env::new(&backend).await;
        env.upgrade().await;
        let snapshot_id = env.snapshot_claiming("guest-a", claim(GUEST_A)).await;
        env.job_of_source(
            "j1",
            &snapshot_id,
            GUEST_A,
            Some(r#"{"name":"web","vmid":"101"}"#),
        )
        .await;

        env.verify("j1").await;

        assert_eq!(
            env.column("j1", "state").await.as_deref(),
            Some("succeeded")
        );
        assert_eq!(
            env.column("j1", "verify_state").await.as_deref(),
            Some("verified"),
            "{}",
            backend.name()
        );
        let (kind, key, label, verified) = env.catalog_source(&snapshot_id).await;
        assert_eq!(kind, "proxmox-vm");
        assert_eq!(key, GUEST_A);
        assert_eq!(label.as_deref(), Some("VM 101 · web"));
        assert!(verified);
        let (signed, meta) = env.signed_source(&snapshot_id).await.unwrap();
        assert_eq!(signed.as_str(), GUEST_A);
        assert_eq!(meta.get("vmid").map(String::as_str), Some("101"));
        assert_eq!(meta.get("name").map(String::as_str), Some("web"));
        env.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_verify_refuses_a_snapshot_naming_another_guest() {
    for backend in backends() {
        let env = Env::new(&backend).await;
        env.upgrade().await;
        let snapshot_id = env.snapshot_claiming("guest-b", claim(GUEST_B)).await;
        env.job_of_source("j1", &snapshot_id, GUEST_A, Some(r#"{"vmid":"101"}"#))
            .await;

        env.verify("j1").await;

        env.assert_refused_as_foreign_source("j1", &snapshot_id)
            .await;
        env.ts.stop().await;
    }
}

const PG_A: &str = "postgres/7300000000000000001/16384";
const PG_B: &str = "postgres/7300000000000000002/16384";

/// One job naming `job_key`, one snapshot claiming `claimed`: the job must fail unsigned.
async fn refuses(label: &str, job_key: &str, claimed: Option<pb::Source>) {
    for backend in backends() {
        let env = Env::new(&backend).await;
        env.upgrade().await;
        let snapshot_id = env.snapshot_claiming(label, claimed.clone()).await;
        env.job_of_source("j1", &snapshot_id, job_key, None).await;

        env.verify("j1").await;

        env.assert_refused_as_foreign_source("j1", &snapshot_id)
            .await;
        env.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_verify_refuses_a_snapshot_naming_another_postgres_cluster() {
    refuses("pg", PG_A, claim(PG_B)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_verify_refuses_a_files_snapshot_for_a_guest_job() {
    refuses("files-for-guest", GUEST_A, None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_verify_refuses_a_guest_claim_in_a_files_job() {
    refuses("guest-in-files", "", claim(GUEST_A)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_verify_refuses_an_invalid_source_claim() {
    refuses("short-key", GUEST_A, claim("proxmox-vm/pve1")).await;
    let crowded = pb::Source {
        key: GUEST_A.to_string(),
        meta: (0..33)
            .map(|n| pb::SourceMeta {
                key: format!("k{n:02}"),
                value: "v".to_string(),
            })
            .collect(),
    };
    refuses("crowded", GUEST_A, Some(crowded)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attest_verify_refuses_a_claim_found_by_a_catalog_sync_first() {
    for backend in backends() {
        let env = Env::new(&backend).await;
        env.upgrade().await;
        let snapshot_id = env.snapshot_claiming("synced", claim(GUEST_B)).await;
        sync_repository(env.ts.state(), &env.repo).await.unwrap();
        env.job_of_source("j1", &snapshot_id, GUEST_A, Some(r#"{"vmid":"101"}"#))
            .await;

        env.verify("j1").await;

        env.assert_refused_as_foreign_source("j1", &snapshot_id)
            .await;
        let (_, source_ref, _, verified) = env.catalog_source(&snapshot_id).await;
        assert_eq!(source_ref, GUEST_A, "[{}]", backend.name());
        assert!(!verified);
        env.ts.stop().await;
    }
}

// Differential test of `latest`: the server's `latest_verified_id` and what
// `lode` picks from the bucket alone, on repositories with several hosts,
// shared and borrowed host names, renames and skewed clocks. Its limits: no
// retention or forget runs (policies are deleted), and the store reports
// object times (LocalFs, S3).

const PG_WAL: &str = "postgres-wal/7300000000000000001";
const KEYS: [&str; 5] = [GUEST_A, GUEST_B, PG_A, "", PG_WAL];
const FILES: usize = 3;
const WAL: usize = 4;
const HOST_IDS: [&str; 3] = [HOST, "h2", "h3"];
const NAMES: [&str; 5] = ["web", "db", "h1", "h2", "h3"];
const YEAR_MS: i64 = 365 * 86_400_000;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Fate {
    Verified,
    /// The manifest claims another source than its job.
    Refused,
    NotRun,
}

/// When the store says it wrote the snapshot. Nothing lands within a minute
/// of the future cutoff: the server and `lode` read their clocks apart.
#[derive(Debug, Clone, Copy)]
enum Stored {
    AsWritten,
    Past(i64),
    SameAsPrevious,
    NearFuture(i64),
    FarFuture(i64),
}

#[derive(Debug, Clone)]
struct Step {
    host: usize,
    key: usize,
    fate: Fate,
    stored: Stored,
    manifest_offset_ms: Option<i64>,
    rename: Option<usize>,
}

#[derive(Debug, Clone)]
struct Case {
    /// The first name of each host; its length is the number of hosts.
    names: Vec<usize>,
    steps: Vec<Step>,
}

fn plain(host: usize, key: usize, fate: Fate) -> Step {
    Step {
        host,
        key,
        fate,
        stored: Stored::AsWritten,
        manifest_offset_ms: None,
        rename: None,
    }
}

fn case_strategy() -> impl Strategy<Value = Case> {
    let step = (
        0usize..3,
        0..KEYS.len(),
        prop_oneof![
            3 => Just(Fate::Verified),
            1 => Just(Fate::Refused),
            1 => Just(Fate::NotRun),
        ],
        prop_oneof![
            Just(Stored::AsWritten),
            (60_000..10 * YEAR_MS).prop_map(Stored::Past),
            Just(Stored::SameAsPrevious),
            (60_000..300_000i64).prop_map(Stored::NearFuture),
            (3_600_000..10 * YEAR_MS).prop_map(Stored::FarFuture),
        ],
        proptest::option::of(-10 * YEAR_MS..10 * YEAR_MS),
        proptest::option::weighted(0.2, 0..NAMES.len()),
    )
        .prop_map(
            |(host, key, fate, stored, manifest_offset_ms, rename)| Step {
                host,
                key,
                fate,
                stored,
                manifest_offset_ms,
                rename,
            },
        );
    (
        proptest::collection::vec(0..NAMES.len(), 1..=3),
        proptest::collection::vec(step, 1..=8),
    )
        .prop_map(|(names, mut steps)| {
            for s in &mut steps {
                s.host %= names.len();
            }
            Case { names, steps }
        })
}

fn fixed_cases() -> Vec<Case> {
    use Fate::*;
    let (web, db, h1, h2, h3) = (0, 1, 2, 3, 4);
    let renamed = Step {
        rename: Some(db),
        ..plain(1, 2, Verified)
    };
    let tied = Step {
        stored: Stored::SameAsPrevious,
        ..plain(0, 0, Verified)
    };
    let ahead = |host| Step {
        stored: Stored::FarFuture(YEAR_MS),
        ..plain(host, 0, Verified)
    };
    vec![
        Case {
            names: vec![h1, h2],
            steps: vec![
                plain(0, 0, Verified),
                plain(1, 0, Verified),
                plain(0, FILES, Verified),
                plain(1, WAL, Verified),
            ],
        },
        Case {
            names: vec![h1, web, web],
            steps: vec![plain(1, 0, Verified), plain(2, 0, Verified)],
        },
        Case {
            names: vec![h1, h1],
            steps: vec![
                plain(0, 0, Verified),
                plain(0, FILES, Verified),
                plain(1, 0, Verified),
                plain(1, 1, Verified),
            ],
        },
        Case {
            names: vec![h1, web],
            steps: vec![plain(1, 2, Verified), renamed],
        },
        Case {
            names: vec![h1],
            steps: vec![
                plain(0, 0, Verified),
                plain(0, 0, Refused),
                plain(0, 0, NotRun),
            ],
        },
        Case {
            names: vec![h1],
            steps: vec![plain(0, 0, Verified), tied],
        },
        Case {
            names: vec![h1, h3],
            steps: vec![plain(0, 0, Verified), ahead(0), ahead(1)],
        },
    ]
}

#[derive(Debug, Default)]
struct Coverage {
    two_hosts: usize,
    shared_name: usize,
    name_is_other_id: usize,
    renamed: usize,
    newer_skipped: usize,
    tie: usize,
    future_stored: usize,
}

/// Records each read with the size of the answer.
struct Counting<S> {
    inner: S,
    reads: std::sync::Mutex<Vec<(String, usize)>>,
}

impl<S: BlobStore> Counting<S> {
    fn new(inner: S) -> Self {
        Counting {
            inner,
            reads: std::sync::Mutex::default(),
        }
    }

    fn note(&self, key: &str, got: &Result<bytes::Bytes, BlobError>) {
        let len = got.as_ref().map_or(0, bytes::Bytes::len);
        self.reads.lock().unwrap().push((key.to_string(), len));
    }

    fn take(&self) -> Vec<(String, usize)> {
        std::mem::take(&mut *self.reads.lock().unwrap())
    }
}

impl<S: BlobStore> BlobStore for Counting<S> {
    async fn put_if_absent(&self, key: &str, data: bytes::Bytes) -> Result<(), BlobError> {
        self.inner.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<bytes::Bytes, BlobError> {
        let got = self.inner.get(key).await;
        self.note(key, &got);
        got
    }

    async fn get_range(
        &self,
        key: &str,
        range: std::ops::Range<u64>,
    ) -> Result<bytes::Bytes, BlobError> {
        let got = self.inner.get_range(key, range).await;
        self.note(key, &got);
        got
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        self.inner.list(prefix).await
    }
}

#[derive(Debug, PartialEq)]
enum Got {
    Snapshot(String, Option<String>),
    Nothing,
    TwoHosts,
    TwoNamed,
}

impl Got {
    fn from_server(id: Option<String>, by_name: Option<&str>) -> Got {
        id.map_or(Got::Nothing, |id| {
            Got::Snapshot(id, by_name.map(str::to_string))
        })
    }
}

/// `resolve_latest_in` over a fresh reader, checking that it read only the
/// snapshot manifests and attestations, each once.
async fn lode_latest(
    source: &lodestore_server::catalog::OpenRepository,
    scope: &LatestScope,
) -> Result<Got, TestCaseError> {
    let counting = Counting::new(&source.store);
    let reader = Reader::open(&counting, source.keys.duplicate(), &source.config).unwrap();
    counting.take();
    let pick = reader.resolve_latest_in(false, scope).await;
    let reads = counting.take();

    let mut listed = source.store.list("snapshots/").await.unwrap();
    listed.extend(
        source
            .store
            .list("locks/")
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.key.starts_with(ATTEST_PREFIX)),
    );
    let keys: HashSet<&str> = reads.iter().map(|(k, _)| k.as_str()).collect();
    prop_assert!(
        reads
            .iter()
            .all(|(k, _)| k.starts_with("snapshots/") || k.starts_with(ATTEST_PREFIX)),
        "{scope:?} read {reads:?}"
    );
    prop_assert_eq!(keys.len(), reads.len(), "{:?} read a key twice", scope);
    prop_assert_eq!(reads.len(), listed.len(), "{:?}", scope);
    let read: usize = reads.iter().map(|(_, n)| n).sum();
    let size: u64 = listed.iter().map(|e| e.size).sum();
    prop_assert!(read as u64 <= size, "{scope:?} read {read} of {size} bytes");

    Ok(match pick {
        Ok(p) => Got::Snapshot(
            hex32(&p.entry.id),
            p.warnings.iter().find_map(|w| match w {
                LatestWarning::HostByName { host_id, .. } => Some(host_id.clone()),
                _ => None,
            }),
        ),
        Err(lodestore_format::Error::InvalidInput(m))
            if m == NO_VERIFIED_SNAPSHOT || m == ALL_SIGNED_STORED_IN_FUTURE =>
        {
            Got::Nothing
        }
        Err(lodestore_format::Error::InvalidInput(m)) if m.contains("names more than one host") => {
            Got::TwoNamed
        }
        Err(lodestore_format::Error::InvalidInput(m))
            if m.contains("come from more than one host") =>
        {
            Got::TwoHosts
        }
        Err(e) => return Err(TestCaseError::fail(format!("{scope:?}: {e}"))),
    })
}

struct Done {
    host: usize,
    key: usize,
    fate: Fate,
    id: String,
    stored_ms: i64,
    /// The host's name when the server signed it.
    name: String,
}

fn mtime_ms(path: &Path) -> i64 {
    let at = std::fs::metadata(path).unwrap().modified().unwrap();
    i64::try_from(
        at.duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

impl Env {
    async fn rename(&self, host: &str, name: &str) {
        sqlx::query("UPDATE agents SET hostname = $1 WHERE id = $2")
            .bind(name)
            .bind(host)
            .execute(self.pool())
            .await
            .unwrap();
    }

    async fn play(&self, case: &Case) -> Result<Vec<Done>, TestCaseError> {
        for (i, &name) in case.names.iter().enumerate() {
            if i > 0 {
                common::seed_agent(&self.ts, &self.repo, HOST_IDS[i], "[]").await;
            }
            self.rename(HOST_IDS[i], NAMES[name]).await;
        }
        let mut names: Vec<&str> = case.names.iter().map(|&n| NAMES[n]).collect();
        let root = self.ts.local_repo_root("main");
        let mut journal: Vec<Done> = Vec::new();
        for (n, step) in case.steps.iter().enumerate() {
            let key = KEYS[step.key];
            let claimed = match step.fate {
                Fate::Refused if key == GUEST_A => GUEST_B,
                Fate::Refused => GUEST_A,
                _ => key,
            };
            let time = step
                .manifest_offset_ms
                .map(|d| manifest_time(now_ms().saturating_add(d)));
            let host = HOST_IDS[step.host];
            let id = self
                .snapshot_on(
                    host,
                    &format!("step-{n}"),
                    Some(claimed).filter(|k| !k.is_empty()).and_then(claim),
                    time,
                )
                .await;
            let path = root.join(snapshot_object_key(&parse_hex32(&id).unwrap()));
            let now = now_ms();
            let at = match step.stored {
                Stored::AsWritten => None,
                Stored::Past(d) => Some(now - d),
                Stored::SameAsPrevious => journal.last().map(|d| d.stored_ms),
                Stored::NearFuture(d) | Stored::FarFuture(d) => Some(now + d),
            };
            if let Some(at) = at {
                File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_modified(
                        SystemTime::UNIX_EPOCH + Duration::from_millis(u64::try_from(at).unwrap()),
                    )
                    .unwrap();
            }
            if let Some(r) = step.rename {
                names[step.host] = NAMES[r];
                self.rename(host, NAMES[r]).await;
            }
            let job = format!("j{n}");
            self.job_on(host, &job, &id, key, None).await;
            if step.fate != Fate::NotRun {
                self.verify(&job).await;
            }
            let want = match step.fate {
                Fate::Verified => "verified",
                Fate::Refused => "failed",
                Fate::NotRun => "pending",
            };
            let state = self.column(&job, "verify_state").await;
            prop_assert_eq!(state.as_deref(), Some(want), "step {}", n);
            journal.push(Done {
                host: step.host,
                key: step.key,
                fate: step.fate,
                id,
                stored_ms: mtime_ms(&path),
                name: names[step.host].to_string(),
            });
        }
        Ok(journal)
    }

    async fn server_latest(&self, host: usize, key: usize) -> Option<String> {
        let source = if key == FILES {
            String::new()
        } else {
            format!("&source={}", KEYS[key].replace('/', "%2F"))
        };
        let page: Value = self
            .ts
            .api()
            .await
            .get(&format!(
                "/api/v1/restore/snapshots?host={}{source}",
                HOST_IDS[host]
            ))
            .await
            .json()
            .await
            .unwrap();
        page["latest_verified_id"].as_str().map(str::to_string)
    }

    async fn received_at(&self, id: &str) -> i64 {
        sqlx::query("SELECT received_at FROM snapshots WHERE id = $1")
            .bind(id)
            .fetch_one(self.pool())
            .await
            .unwrap()
            .try_get("received_at")
            .unwrap()
    }

    async fn compare(
        &self,
        case: &Case,
        journal: &[Done],
        coverage: &RefCell<Coverage>,
    ) -> Result<(), TestCaseError> {
        let hosts = case.names.len();
        let source = open_repository(self.ts.state(), &self.repo).await.unwrap();
        let verified = || journal.iter().filter(|d| d.fate == Fate::Verified);
        let signed_hosts: BTreeSet<usize> = verified().map(|d| d.host).collect();
        let id_of = |name: &str| {
            HOST_IDS
                .iter()
                .position(|h| *h == name)
                .filter(|h| signed_hosts.contains(h))
        };

        for key in [0, 1, 2, WAL] {
            let source_key = SourceKey::parse(KEYS[key]).unwrap();
            let of_key = || verified().filter(|d| d.key == key && key != WAL);
            for host in 0..hosts {
                let server = self.server_latest(host, key).await;
                // An id no signed record carries is looked up as a name; the name checks cover it.
                if signed_hosts.contains(&host) {
                    let scope =
                        LatestScope::InstanceOnHost(source_key.clone(), HOST_IDS[host].into());
                    let lode = lode_latest(&source, &scope).await?;
                    prop_assert_eq!(
                        &lode,
                        &Got::from_server(server.clone(), None),
                        "{:?}",
                        scope
                    );
                }
                if let Some(chosen) = &server {
                    let bar = journal.iter().find(|d| &d.id == chosen).unwrap().stored_ms;
                    if journal.iter().any(|d| {
                        d.host == host
                            && d.key == key
                            && d.fate != Fate::Verified
                            && d.stored_ms > bar
                    }) {
                        coverage.borrow_mut().newer_skipped += 1;
                    }
                }
                let pair: Vec<&Done> = of_key().filter(|d| d.host == host).collect();
                if pair.iter().map(|d| &d.name).collect::<BTreeSet<_>>().len() > 1 {
                    coverage.borrow_mut().renamed += 1;
                }
                let times: BTreeSet<i64> = pair.iter().map(|d| d.stored_ms).collect();
                if times.len() < pair.len() {
                    coverage.borrow_mut().tie += 1;
                }
            }

            let on: BTreeSet<usize> = of_key().map(|d| d.host).collect();
            let scope = LatestScope::Instance(source_key.clone());
            let lode = lode_latest(&source, &scope).await?;
            if on.len() > 1 {
                prop_assert_eq!(&lode, &Got::TwoHosts, "{:?}", scope);
                coverage.borrow_mut().two_hosts += 1;
            } else {
                let want = match on.first() {
                    Some(&host) => self.server_latest(host, key).await,
                    None => None,
                };
                prop_assert_eq!(&lode, &Got::from_server(want, None), "{:?}", scope);
            }

            for name in NAMES {
                let scope = LatestScope::InstanceOnHost(source_key.clone(), name.into());
                let lode = lode_latest(&source, &scope).await?;
                let want = if let Some(host) = id_of(name) {
                    Got::from_server(self.server_latest(host, key).await, None)
                } else {
                    let named: BTreeSet<usize> = of_key()
                        .filter(|d| d.name == name)
                        .map(|d| d.host)
                        .collect();
                    match named.len() {
                        0 => Got::Nothing,
                        1 => {
                            let host = *named.first().unwrap();
                            Got::from_server(
                                self.server_latest(host, key).await,
                                Some(HOST_IDS[host]),
                            )
                        }
                        _ => {
                            coverage.borrow_mut().shared_name += 1;
                            Got::TwoNamed
                        }
                    }
                };
                prop_assert_eq!(&lode, &want, "{:?}", scope);
            }
        }

        // `lode` keeps one files `latest` for the whole repository; the server keeps one per host.
        let mut newest: Option<(i64, String)> = None;
        for host in 0..hosts {
            if let Some(id) = self.server_latest(host, FILES).await {
                newest = newest.max(Some((self.received_at(&id).await, id)));
            }
        }
        let lode = lode_latest(&source, &LatestScope::Files).await?;
        prop_assert_eq!(
            lode,
            Got::from_server(newest.map(|(_, id)| id), None),
            "files"
        );

        let mut seen = coverage.borrow_mut();
        if verified().any(|d| id_of(&d.name).is_some_and(|h| h != d.host)) {
            seen.name_is_other_id += 1;
        }
        let cutoff = now_ms() + FUTURE_TOLERANCE_MS;
        if verified().any(|d| d.stored_ms > cutoff) {
            seen.future_stored += 1;
        }
        Ok(())
    }
}

fn manifest_time(at_ms: i64) -> pb::Timestamp {
    pb::Timestamp {
        seconds: at_ms.div_euclid(1000),
        nanos: 0,
    }
}

async fn check_case(
    backend: &common::Backend,
    case: &Case,
    coverage: &RefCell<Coverage>,
) -> Result<(), TestCaseError> {
    let env = Env::new(backend).await;
    sqlx::query("DELETE FROM policies")
        .execute(env.pool())
        .await
        .unwrap();
    env.upgrade().await;
    let result = match env.play(case).await {
        Ok(journal) => env.compare(case, &journal, coverage).await,
        Err(e) => Err(e),
    };
    env.ts.stop().await;
    result.map_err(|e| TestCaseError::fail(format!("[{}] {e}", backend.name())))
}

fn multi_thread() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn attest_verify_latest_agrees_with_lode_on_hostile_repositories() {
    let rt = multi_thread();
    for backend in backends() {
        let started = std::time::Instant::now();
        let coverage = RefCell::new(Coverage::default());
        for (n, case) in fixed_cases().iter().enumerate() {
            if let Err(e) = rt.block_on(check_case(&backend, case, &coverage)) {
                panic!("fixed case {}: {e}\n{case:#?}", n + 1);
            }
        }
        let mut runner = TestRunner::new(ProptestConfig {
            cases: 12,
            failure_persistence: None,
            max_shrink_iters: 64,
            ..ProptestConfig::default()
        });
        if let Err(e) = runner.run(&case_strategy(), |case| {
            rt.block_on(check_case(&backend, &case, &coverage))
        }) {
            panic!("{e}");
        }
        let seen = coverage.into_inner();
        eprintln!(
            "[{}] 7 fixed + 12 generated cases in {:?}: {seen:?}",
            backend.name(),
            started.elapsed()
        );
        for (class, count) in [
            ("two_hosts", seen.two_hosts),
            ("shared_name", seen.shared_name),
            ("name_is_other_id", seen.name_is_other_id),
            ("renamed", seen.renamed),
            ("newer_skipped", seen.newer_skipped),
            ("tie", seen.tie),
            ("future_stored", seen.future_stored),
        ] {
            assert!(count >= 1, "[{}] no case covered {class}", backend.name());
        }
    }
}

#[test]
fn attest_verify_latest_of_a_cloned_key_needs_a_host() {
    let rt = multi_thread();
    for backend in backends() {
        let coverage = RefCell::new(Coverage::default());
        let clone = Case {
            names: vec![2, 3],
            steps: vec![plain(0, 0, Fate::Verified), plain(1, 0, Fate::Verified)],
        };
        if let Err(e) = rt.block_on(check_case(&backend, &clone, &coverage)) {
            panic!("{e}");
        }
        assert_eq!(coverage.into_inner().two_hosts, 1, "[{}]", backend.name());
    }
}
