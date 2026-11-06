#[path = "../../lodestore-format/tests/adv_support/mod.rs"]
mod adv_support;
mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use adv_support::{attacker_keys, plant_attestation_hint, plant_blobs};
use common::{AgentEnv, Backend, TestServer, WAIT, wait_until};
use lodestore_format::chunker::blob_id;
use lodestore_format::crypto::{BLOB_TYPE_DATA, BlobId};
use lodestore_format::index::{hex32, index_object_key, open_shard, parse_hex32};
use lodestore_format::keys::WriteKeys;
use lodestore_format::pack::pack_object_key;
use lodestore_format::repo::Repository;
use lodestore_format::store::{BlobStore, LocalFsStore};
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_server::catalog::open_repository;
use lodestore_server::db::now_ms;
use lodestore_server::maintenance::attest::verify::verify_then_retain;
use lodestore_server::maintenance::attest::{ensure_upgraded, load_distrusted, record_distrusted};
use serde_json::{Value, json};
use sqlx::Row;

const PASSWORD: &[u8] = b"test-password";
const HOST: &str = "h1";

struct Env {
    ts: TestServer,
    repo: String,
    store: LocalFsStore,
}

impl Env {
    async fn new() -> Env {
        let ts = TestServer::start_with(&Backend::Sqlite, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        let store = LocalFsStore::new(ts.local_repo_root("main"));
        sqlx::query(
            "INSERT INTO agents (id, hostname, repository_id, labels_json, os, arch, version, \
             status, enrolled_at, cert_serial, cert_issued_at, cert_not_after, last_seen_at) \
             VALUES ($1, $1, $2, '[]', 'linux', 'x86_64', '0.1.0', 'active', $3, 'serial', $3, $4, $3)",
        )
        .bind(HOST)
        .bind(&repo)
        .bind(now_ms() - 3_600_000)
        .bind(now_ms() + 86_400_000_000)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
        Env { ts, repo, store }
    }

    async fn upgrade(&self) {
        ensure_upgraded(self.ts.state(), &self.repo).await.unwrap();
    }

    async fn snapshot(&self, label: &str, plant: bool) -> (String, Option<BlobId>) {
        let dir: PathBuf = self.ts.dir.path().join(format!("src-{label}"));
        std::fs::create_dir_all(&dir).unwrap();
        let content = format!("content of {label}, unique to this test").into_bytes();
        std::fs::write(dir.join("file.txt"), &content).unwrap();
        let repo = Repository::open(&self.store, PASSWORD).await.unwrap();
        let mut planted = None;
        if plant {
            let id = blob_id(repo.read_keys().k_id(), &content);
            planted = Some(
                plant_blobs(&self.store, &attacker_keys(&repo), &[(id, BLOB_TYPE_DATA)]).await,
            );
        }
        let keys = repo.read_keys().write_keys(repo.config_hash());
        let mut writer = Writer::connect(&self.store, keys).await.unwrap();
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
            .unwrap();
        (hex32(&out.snapshot_id), planted)
    }

    async fn job(&self, id: &str, snapshot_id: &str) {
        sqlx::query(
            "INSERT INTO jobs (id, host_id, policy_name, source, state, spec, queued_at, \
             finished_at, snapshot_id, verify_state) \
             VALUES ($1, $2, 'p', 'schedule', 'succeeded', $3, $4, $4, $5, 'pending')",
        )
        .bind(id)
        .bind(HOST)
        .bind(Vec::<u8>::new())
        .bind(now_ms())
        .bind(snapshot_id)
        .execute(&self.ts.state().db.pool)
        .await
        .unwrap();
    }

    async fn verify_state(&self, job: &str) -> String {
        sqlx::query("SELECT verify_state FROM jobs WHERE id = $1")
            .bind(job)
            .fetch_one(&self.ts.state().db.pool)
            .await
            .unwrap()
            .try_get("verify_state")
            .unwrap()
    }

    async fn distrusted(&self) -> Vec<BlobId> {
        load_distrusted(self.ts.state(), &self.repo).await.unwrap()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn distrust_recorded_only_for_data_errors() {
    let env = Env::new().await;
    env.upgrade().await;
    let (snapshot, planted) = env.snapshot("planted", true).await;
    let planted = planted.unwrap();
    env.job("j1", &snapshot).await;
    verify_then_retain(env.ts.state().clone(), "j1".into()).await;
    assert_eq!(env.verify_state("j1").await, "failed");
    assert_eq!(env.distrusted().await, vec![planted]);
    let reason: String = sqlx::query("SELECT reason FROM distrusted_shards WHERE shard_id = $1")
        .bind(hex32(&planted))
        .fetch_one(&env.ts.state().db.pool)
        .await
        .unwrap()
        .try_get("reason")
        .unwrap();
    assert!(!reason.is_empty());
    env.ts.stop().await;

    let env = Env::new().await;
    let (snapshot, _) = env.snapshot("honest", false).await;
    env.upgrade().await;
    let keys = open_repository(env.ts.state(), &env.repo)
        .await
        .unwrap()
        .keys;
    for entry in env.store.list("index/").await.unwrap() {
        let id = parse_hex32(entry.key.strip_prefix("index/").unwrap()).unwrap();
        let bytes = BlobStore::get(&env.store, &index_object_key(&id))
            .await
            .unwrap();
        let shard = open_shard(keys.k_idx(), keys.repo_id(), &id, &bytes).unwrap();
        for record in &shard.packs {
            let pack: BlobId = record.pack_id.as_slice().try_into().unwrap();
            let path = env.ts.local_repo_root("main").join(pack_object_key(&pack));
            std::fs::remove_file(&path).unwrap();
            std::fs::create_dir(&path).unwrap();
        }
    }
    env.job("j2", &snapshot).await;
    verify_then_retain(env.ts.state().clone(), "j2".into()).await;
    assert_eq!(env.verify_state("j2").await, "pending");
    assert!(env.distrusted().await.is_empty());
    env.ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn distrust_redetection_returns_to_the_sent_window() {
    let env = Env::new().await;
    env.upgrade().await;
    let (snapshot1, planted) = env.snapshot("planted", true).await;
    let planted = planted.unwrap();
    env.job("j1", &snapshot1).await;
    verify_then_retain(env.ts.state().clone(), "j1".into()).await;
    assert_eq!(env.verify_state("j1").await, "failed");
    assert_eq!(env.distrusted().await, vec![planted]);

    sqlx::query("UPDATE distrusted_shards SET at = 1 WHERE repository_id = $1 AND shard_id = $2")
        .bind(&env.repo)
        .bind(hex32(&planted))
        .execute(&env.ts.state().db.pool)
        .await
        .unwrap();
    let junk: BlobId = [0; 32];
    record_distrusted(env.ts.state(), &env.repo, &junk, "test")
        .await
        .unwrap();
    assert_eq!(env.distrusted().await, vec![junk, planted]);

    tokio::time::sleep(Duration::from_millis(5)).await;
    let (snapshot2, _) = env.snapshot("planted", false).await;
    env.job("j2", &snapshot2).await;
    verify_then_retain(env.ts.state().clone(), "j2".into()).await;
    assert_eq!(env.verify_state("j2").await, "failed");
    assert_eq!(env.distrusted().await, vec![planted, junk]);

    let rows: i64 = sqlx::query("SELECT COUNT(*) AS n FROM distrusted_shards WHERE shard_id = $1")
        .bind(hex32(&planted))
        .fetch_one(&env.ts.state().db.pool)
        .await
        .unwrap()
        .try_get("n")
        .unwrap();
    assert_eq!(rows, 1);
    env.ts.stop().await;
}

fn frame(stream: &mut UnixStream, data: &[u8]) {
    stream
        .write_all(&u32::try_from(data.len()).unwrap().to_be_bytes())
        .unwrap();
    stream.write_all(data).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn distrust_reaches_stdin_jobs_through_hello() {
    let mut env = AgentEnv::enrolled(&Backend::Sqlite).await;
    let socket_dir = tempfile::Builder::new()
        .prefix("ls")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = socket_dir.path().join("agent.sock");
    let yaml = env.paths.agent_yaml();
    std::fs::write(&yaml, format!("socket: {}\n", socket.display())).unwrap();
    std::fs::set_permissions(&yaml, std::fs::Permissions::from_mode(0o600)).unwrap();

    let keys = WriteKeys::from_file_text(
        &std::fs::read_to_string(env.paths.writer_keys()).expect("the agent's writer keys"),
    )
    .unwrap();
    let content = b"a dump the attacker names through an attestation hint".to_vec();
    let store = LocalFsStore::new(env.ts().local_repo_root("main"));
    let shard = plant_blobs(
        &store,
        &keys,
        &[(blob_id(&keys.k_id, &content), BLOB_TYPE_DATA)],
    )
    .await;
    plant_attestation_hint(&store, &[shard]).await;
    record_distrusted(env.ts().state(), &env.repo, &shard, "test")
        .await
        .unwrap();

    env.start_agent().await;
    let file = env.paths.distrusted_shards();
    let received = wait_until(WAIT, || async {
        std::fs::read_to_string(&file).is_ok_and(|t| t.lines().any(|l| l == hex32(&shard)))
    })
    .await;
    assert!(received, "the agent never received the distrusted shard");
    assert!(wait_until(Duration::from_secs(10), || async { socket.exists() }).await);

    let mut stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let line = json!({"v": 1, "op": "backup_stdin", "name": "app.dump", "policy": null});
    writeln!(stream, "{line}").unwrap();
    frame(&mut stream, &content);
    stream.write_all(&0u32.to_be_bytes()).unwrap();
    let answers: Vec<Value> = BufReader::new(stream)
        .lines()
        .map_while(Result::ok)
        .map(|l| serde_json::from_str(&l).unwrap())
        .collect();
    let summary = answers.last().unwrap();
    assert_eq!(summary["type"], "backup_summary", "{answers:?}");

    let pool = &env.ts().state().db.pool;
    let verified = wait_until(WAIT, || async {
        sqlx::query("SELECT verify_state FROM jobs WHERE source = 'cli'")
            .fetch_all(pool)
            .await
            .unwrap()
            .iter()
            .any(|r| {
                r.try_get::<Option<String>, _>("verify_state")
                    .unwrap()
                    .as_deref()
                    == Some("verified")
            })
    })
    .await;
    assert!(verified, "the stdin job was not verified");
    env.stop().await;
}
