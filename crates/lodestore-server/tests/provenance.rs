mod common;

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{AgentEnv, TestServer, WAIT, backends, wait_until};
use lodestore_format::attest::{
    ATTEST_PREFIX, put_attestation, read_attestations, seal_provenance,
};
use lodestore_format::crypto::{BlobId, Entropy};
use lodestore_format::index::{hex32, parse_hex32};
use lodestore_format::keys::ReadKeys;
use lodestore_format::pb;
use lodestore_format::provenance::{ProvenanceSet, Signer};
use lodestore_format::repo::Repository;
use lodestore_format::store::{BlobStore, LocalFsStore};
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_server::catalog::{JobLink, ingest_snapshot, open_repository, sync_repository};
use lodestore_server::db::now_ms;
use lodestore_server::maintenance::attest::ensure_provenance_backfilled;
use serde_json::{Value, json};
use sqlx::Row;

const PASSWORD: &[u8] = b"test-password";

async fn keys(ts: &TestServer, repo: &str) -> ReadKeys {
    open_repository(ts.state(), repo).await.unwrap().keys
}

fn store(ts: &TestServer) -> LocalFsStore {
    LocalFsStore::new(ts.local_repo_root("main"))
}

async fn provenance(ts: &TestServer, repo: &str) -> ProvenanceSet {
    let keys = keys(ts, repo).await;
    let set = read_attestations(&store(ts), &keys).await.unwrap();
    ProvenanceSet::from_attested(&set, &keys)
}

async fn attest_objects(ts: &TestServer) -> usize {
    store(ts).list(ATTEST_PREFIX).await.unwrap().len()
}

async fn verified_columns(ts: &TestServer, snapshot: &str) -> (Option<i64>, Option<String>) {
    let row = sqlx::query("SELECT verified_at, verified_by FROM snapshots WHERE id = $1")
        .bind(snapshot)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap();
    (
        row.try_get("verified_at").unwrap(),
        row.try_get("verified_by").unwrap(),
    )
}

async fn wait_verified(ts: &TestServer, snapshot: &str) {
    let pool = &ts.state().db.pool;
    let done = wait_until(WAIT, || async {
        sqlx::query("SELECT j.verify_state, s.verified_at FROM jobs j JOIN snapshots s ON s.job_id = j.id WHERE j.snapshot_id = $1")
            .bind(snapshot)
            .fetch_optional(pool)
            .await
            .unwrap()
            .is_some_and(|row| {
                row.try_get::<Option<String>, _>("verify_state").unwrap().as_deref()
                    == Some("verified")
                    && row.try_get::<Option<i64>, _>("verified_at").unwrap().is_some()
            })
    })
    .await;
    assert!(done, "snapshot {snapshot} was not verified");
}

async fn job_of(ts: &TestServer, snapshot: &str) -> String {
    sqlx::query("SELECT id FROM jobs WHERE snapshot_id = $1")
        .bind(snapshot)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .try_get("id")
        .unwrap()
}

fn source(env: &AgentEnv, label: &str) -> std::path::PathBuf {
    let dir = env.work.path().join(label);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), b"alpha alpha alpha").unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provenance_signed_after_verification() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let ts = env.ts();
        let dir = source(&env, "data");
        let first = env.backup(&[&dir]).await;
        wait_verified(ts, &first).await;

        let set = provenance(ts, &env.repo).await;
        let id: BlobId = parse_hex32(&first).unwrap();
        let record = set.get(&id).expect("the snapshot is signed");
        assert_eq!(set.signer(&id), Some(Signer::Server));
        assert_eq!(record.host_id, env.host);
        assert_eq!(record.hostname, env.hostname);
        assert_eq!(record.job_id, job_of(ts, &first).await);
        let (at, by) = verified_columns(ts, &first).await;
        assert!(at.is_some(), "[{}]", backend.name());
        assert_eq!(by.as_deref(), Some("server"));

        let before = attest_objects(ts).await;
        let second = env.backup(&[&dir]).await;
        wait_verified(ts, &second).await;
        assert_eq!(attest_objects(ts).await, before + 1, "[{}]", backend.name());
        let set = provenance(ts, &env.repo).await;
        assert!(set.contains(&parse_hex32(&second).unwrap()));
        env.stop().await;
    }
}

fn frame(stream: &mut UnixStream, data: &[u8]) {
    stream
        .write_all(&u32::try_from(data.len()).unwrap().to_be_bytes())
        .unwrap();
    stream.write_all(data).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provenance_stdin_job_signed_for_reporting_host() {
    for backend in backends() {
        let mut env = AgentEnv::enrolled(&backend).await;
        let socket_dir = tempfile::Builder::new()
            .prefix("ls")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = socket_dir.path().join("agent.sock");
        let yaml = env.paths.agent_yaml();
        std::fs::write(&yaml, format!("socket: {}\n", socket.display())).unwrap();
        std::fs::set_permissions(&yaml, std::fs::Permissions::from_mode(0o600)).unwrap();
        env.start_agent().await;
        assert!(wait_until(Duration::from_secs(10), || async { socket.exists() }).await);

        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let line = json!({"v": 1, "op": "backup_stdin", "name": "app.dump", "policy": null});
        writeln!(stream, "{line}").unwrap();
        frame(&mut stream, b"a dump, in two parts");
        stream.write_all(&0u32.to_be_bytes()).unwrap();
        let answers: Vec<Value> = BufReader::new(stream)
            .lines()
            .map_while(Result::ok)
            .map(|l| serde_json::from_str(&l).unwrap())
            .collect();
        let summary = answers.last().unwrap();
        assert_eq!(summary["type"], "backup_summary", "{answers:?}");
        let snapshot = summary["snapshot_id"].as_str().unwrap().to_string();

        let ts = env.ts();
        wait_verified(ts, &snapshot).await;
        let job = job_of(ts, &snapshot).await;
        let source: String = sqlx::query("SELECT source FROM jobs WHERE id = $1")
            .bind(&job)
            .fetch_one(&ts.state().db.pool)
            .await
            .unwrap()
            .try_get("source")
            .unwrap();
        assert_eq!(source, "cli");
        let set = provenance(ts, &env.repo).await;
        let record = set.get(&parse_hex32(&snapshot).unwrap()).unwrap();
        assert_eq!(record.host_id, env.host, "[{}]", backend.name());
        assert_eq!(record.job_id, job);
        assert_eq!(
            verified_columns(ts, &snapshot).await.1.as_deref(),
            Some("server")
        );
        env.stop().await;
    }
}

struct Catalog {
    ts: TestServer,
    repo: String,
}

impl Catalog {
    async fn new(backend: &common::Backend) -> Catalog {
        let ts = TestServer::start_with(backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        let catalog = Catalog { ts, repo };
        catalog.host("host-a").await;
        catalog
    }

    fn pool(&self) -> &sqlx::AnyPool {
        &self.ts.state().db.pool
    }

    async fn host(&self, id: &str) {
        sqlx::query(
            "INSERT INTO agents (id, hostname, repository_id, labels_json, os, arch, version, \
             status, enrolled_at, cert_serial, cert_issued_at, cert_not_after, last_seen_at) \
             VALUES ($1, $1, $2, '[]', 'linux', 'x86_64', '0.1.0', 'active', $3, 'serial', $3, $4, $3)",
        )
        .bind(id)
        .bind(&self.repo)
        .bind(now_ms() - 3_600_000)
        .bind(now_ms() + 86_400_000_000)
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn write(&self, label: &str, agent: Option<&str>) -> String {
        let store = store(&self.ts);
        let repo = Repository::open(&store, PASSWORD).await.unwrap();
        let keys = repo.read_keys().write_keys(repo.config_hash());
        let mut writer = Writer::new(&store, keys, repo.config()).await.unwrap();
        let data = self.ts.dir.path().join(format!("data-{label}"));
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("f.txt"), format!("content of {label}")).unwrap();
        let options = BackupOptions {
            agent_id: agent.map(|a| a.as_bytes().to_vec()),
            ..BackupOptions::default()
        };
        let out = writer.backup(&[data], options, &|_| {}).await.unwrap();
        hex32(&out.snapshot_id)
    }

    async fn job(&self, id: &str, snapshot: &str, verify_state: Option<&str>) {
        sqlx::query(
            "INSERT INTO jobs (id, host_id, source, state, spec, queued_at, finished_at, \
             snapshot_id, verify_state) VALUES ($1, 'host-a', 'schedule', 'succeeded', $2, $3, $3, $4, $5)",
        )
        .bind(id)
        .bind(Vec::<u8>::new())
        .bind(now_ms())
        .bind(snapshot)
        .bind(verify_state)
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn linked(&self, label: &str, verify_state: Option<&str>) -> String {
        let snapshot = self.write(label, Some("host-a")).await;
        let job = format!("job-{label}");
        self.job(&job, &snapshot, verify_state).await;
        let link = JobLink {
            job_id: &job,
            host_id: "host-a",
        };
        ingest_snapshot(self.ts.state(), &self.repo, &snapshot, Some(link))
            .await
            .unwrap();
        snapshot
    }

    async fn like_a_zero_one_catalog(&self) -> [String; 5] {
        let a = self.linked("a", None).await;
        let b = self.linked("b", Some("verified")).await;
        let c = self.linked("c", Some("failed")).await;
        let d = self.write("d", Some("host-a")).await;
        ingest_snapshot(self.ts.state(), &self.repo, &d, None)
            .await
            .unwrap();
        let e = self.linked("e", None).await;
        sqlx::query("UPDATE snapshots SET unverified_reason = 'bad' WHERE id = $1")
            .bind(&e)
            .execute(self.pool())
            .await
            .unwrap();
        [a, b, c, d, e]
    }

    async fn flag(&self) -> Option<i64> {
        sqlx::query("SELECT provenance_backfilled_at FROM repositories WHERE id = $1")
            .bind(&self.repo)
            .fetch_one(self.pool())
            .await
            .unwrap()
            .try_get("provenance_backfilled_at")
            .unwrap()
    }

    async fn forget_marks(&self) {
        sqlx::query("UPDATE snapshots SET verified_at = NULL, verified_by = NULL")
            .execute(self.pool())
            .await
            .unwrap();
    }
}

fn ids(set: &ProvenanceSet, all: &[String]) -> BTreeSet<String> {
    all.iter()
        .filter(|s| set.contains(&parse_hex32(s).unwrap()))
        .cloned()
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provenance_backfill_signs_linked_snapshots_only() {
    for backend in backends() {
        let c = Catalog::new(&backend).await;
        let all = c.like_a_zero_one_catalog().await;
        assert_eq!(c.flag().await, None);

        let signed = ensure_provenance_backfilled(c.ts.state(), &c.repo)
            .await
            .unwrap();

        assert_eq!(signed, 2, "[{}]", backend.name());
        let set = provenance(&c.ts, &c.repo).await;
        assert_eq!(
            ids(&set, &all),
            BTreeSet::from([all[0].clone(), all[1].clone()])
        );
        let record = set.get(&parse_hex32(&all[0]).unwrap()).unwrap();
        assert_eq!(record.host_id, "host-a");
        assert_eq!(record.job_id, "job-a");
        for (n, snapshot) in all.iter().enumerate() {
            let (at, by) = verified_columns(&c.ts, snapshot).await;
            assert_eq!(at.is_some(), n < 2, "snapshot {n}");
            assert_eq!(by.as_deref(), (n < 2).then_some("server"), "snapshot {n}");
        }
        assert!(c.flag().await.is_some());
        c.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provenance_backfill_is_idempotent_and_resumable() {
    for backend in backends() {
        let c = Catalog::new(&backend).await;
        let all = c.like_a_zero_one_catalog().await;
        ensure_provenance_backfilled(c.ts.state(), &c.repo)
            .await
            .unwrap();
        let objects = attest_objects(&c.ts).await;
        let first = ids(&provenance(&c.ts, &c.repo).await, &all);

        let again = ensure_provenance_backfilled(c.ts.state(), &c.repo)
            .await
            .unwrap();
        assert_eq!(again, 0, "[{}]", backend.name());
        assert_eq!(attest_objects(&c.ts).await, objects);

        sqlx::query("UPDATE repositories SET provenance_backfilled_at = NULL")
            .execute(c.pool())
            .await
            .unwrap();
        c.forget_marks().await;
        let redone = ensure_provenance_backfilled(c.ts.state(), &c.repo)
            .await
            .unwrap();
        assert_eq!(redone, 2);
        assert_eq!(attest_objects(&c.ts).await, objects + 1);
        let set = provenance(&c.ts, &c.repo).await;
        assert_eq!(ids(&set, &all), first);
        assert_eq!(set.len(), 2);
        c.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provenance_reconcile_restores_marks_after_db_loss() {
    for backend in backends() {
        let c = Catalog::new(&backend).await;
        let all = c.like_a_zero_one_catalog().await;
        ensure_provenance_backfilled(c.ts.state(), &c.repo)
            .await
            .unwrap();
        c.forget_marks().await;
        assert_eq!(verified_columns(&c.ts, &all[0]).await, (None, None));

        sync_repository(c.ts.state(), &c.repo).await.unwrap();

        for snapshot in &all[..2] {
            let (at, by) = verified_columns(&c.ts, snapshot).await;
            assert!(at.is_some(), "[{}]", backend.name());
            assert_eq!(by.as_deref(), Some("server"));
        }
        assert_eq!(verified_columns(&c.ts, &all[2]).await, (None, None));
        c.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provenance_reconcile_marks_owner_signed() {
    for backend in backends() {
        let c = Catalog::new(&backend).await;
        let snapshot = c.write("direct", None).await;
        let store = store(&c.ts);
        let repo = Repository::open(&store, PASSWORD).await.unwrap();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let record = pb::ProvenanceRecord {
            snapshot_id: parse_hex32(&snapshot).unwrap().to_vec(),
            verified_at: Some(pb::Timestamp {
                seconds: i64::try_from(now.as_secs()).unwrap(),
                nanos: now.subsec_nanos(),
            }),
            ..Default::default()
        };
        let mut entropy = Entropy::os();
        let sealed = seal_provenance(repo.read_keys(), &record, &mut entropy).unwrap();
        put_attestation(
            &store,
            repo.read_keys(),
            &[],
            &[],
            vec![sealed],
            &mut entropy,
        )
        .await
        .unwrap();

        sync_repository(c.ts.state(), &c.repo).await.unwrap();

        let (at, by) = verified_columns(&c.ts, &snapshot).await;
        assert!(at.is_some(), "[{}]", backend.name());
        assert_eq!(by.as_deref(), Some("owner"));
        c.ts.stop().await;
    }
}
