//! A shard that lies about its blobs (signed by the upgrade, or named by a
//! planted hint) may cost a host one backup, not every night. Only the public
//! path of a job is used, with the real agent, and only fields the first
//! release has.

#[path = "../../lodestore-format/tests/adv_support/mod.rs"]
mod adv_support;
mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use adv_support::{attacker_keys, plant_attestation_hint, plant_blobs};
use common::{AgentEnv, Backend, TestServer, WAIT, wait_until};
use lodestore_format::chunker::blob_id;
use lodestore_format::config::config_hash;
use lodestore_format::crypto::{BLOB_TYPE_DATA, Entropy};
use lodestore_format::index::parse_hex32;
use lodestore_format::keys::{KdfCost, WriteKeys};
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::store::LocalFsStore;
use lodestore_server::maintenance::attest::load_distrusted;
use lodestore_server::repos::db::{NewRepository, RepoLocation, create_repository};
use serde_json::{Value, json};
use sqlx::Row;

const PASSWORD: &[u8] = b"test-password";
const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

async fn run_backup(env: &AgentEnv, n: usize, dir: &Path) -> String {
    let name = format!("adv-{}-{n}", env.hostname);
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
    assert_eq!(body["jobs"][0]["created"], true, "{body}");
    body["jobs"][0]["id"].as_str().unwrap().to_string()
}

async fn restores(root: PathBuf, snapshot: &str, dir: &Path) -> Result<(), String> {
    let store = LocalFsStore::new(root);
    let repo = Repository::open(&store, PASSWORD)
        .await
        .map_err(|e| e.to_string())?;
    let mut reader = Reader::open(&store, repo.read_keys().duplicate(), repo.config())
        .map_err(|e| e.to_string())?;
    let target = tempfile::tempdir().unwrap();
    let id = parse_hex32(snapshot).ok_or("a malformed snapshot id")?;
    let report = reader
        .restore(&id, target.path(), &RestoreOptions::default(), &|_| {})
        .await
        .map_err(|e| e.to_string())?;
    if !report.errors.is_empty() {
        return Err(format!("{:?}", report.errors));
    }
    let restored = target
        .path()
        .join(dir.canonicalize().unwrap().strip_prefix("/").unwrap());
    for entry in std::fs::read_dir(dir).unwrap() {
        let name = entry.unwrap().file_name();
        let got = std::fs::read(restored.join(&name)).map_err(|e| format!("{name:?}: {e}"))?;
        if got != std::fs::read(dir.join(&name)).unwrap() {
            return Err(format!("{name:?} differs"));
        }
    }
    Ok(())
}

fn source(env: &AgentEnv, label: &str, content: &[u8]) -> PathBuf {
    let dir = env.work.path().join(label);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("file.txt"), content).unwrap();
    dir
}

/// Job 1 may fail verification or restore; job 2, with the same content,
/// must succeed and restore.
async fn assert_costs_one_backup(env: &AgentEnv, dir: &Path, root: PathBuf) {
    let first = run_backup(env, 1, dir).await;
    let first = env.wait_job_settled(&first).await;
    match first["state"].as_str().unwrap() {
        "succeeded" | "warnings" => {
            let snapshot = first["snapshot_id"].as_str().expect("a snapshot");
            restores(root.clone(), snapshot, dir)
                .await
                .unwrap_or_else(|e| panic!("job 1 succeeded but does not restore: {e}; {first}"));
        }
        _ => assert_eq!(first["error_code"], "verification_failed", "{first}"),
    }
    let second = run_backup(env, 2, dir).await;
    let second = env.wait_job_settled(&second).await;
    assert!(
        second["state"] == "succeeded" || second["state"] == "warnings",
        "the second backup of the same content did not succeed: {second}"
    );
    let snapshot = second["snapshot_id"].as_str().expect("a snapshot");
    restores(root, snapshot, dir)
        .await
        .unwrap_or_else(|e| panic!("job 2 does not restore: {e}; {second}"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_lying_attested_shard_costs_one_backup() {
    let ts = TestServer::start(&Backend::Sqlite).await;
    let root = ts.local_repo_root("main");
    let store = LocalFsStore::new(root.clone());
    let mut entropy = Entropy::os();
    let pending = PendingInit::generate(
        &mut entropy,
        InitOptions {
            kdf: LOW_KDF,
            ..InitOptions::default()
        },
    )
    .unwrap();
    let keys = pending.read_keys();
    let master = keys.master().clone();
    let format_repo_id = *keys.repo_id();
    let hash = config_hash(pending.config_bytes());
    pending
        .commit(&store, PASSWORD, &mut entropy)
        .await
        .unwrap();

    let content = b"a file the attacker names before the server opens the repository".to_vec();
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let attacker = attacker_keys(&repo);
    plant_blobs(
        &store,
        &attacker,
        &[(blob_id(&attacker.k_id, &content), BLOB_TYPE_DATA)],
    )
    .await;

    let id = create_repository(
        ts.state(),
        NewRepository {
            name: "main".to_string(),
            location: RepoLocation {
                backend: "local".to_string(),
                options: BTreeMap::from([("path".to_string(), root.display().to_string())]),
            },
            experimental: false,
            format_repo_id,
            config_hash: hash,
            master,
            server_slot_secret: None,
            server_creds: None,
            agent_creds: None,
        },
    )
    .await
    .unwrap();

    let env = AgentEnv::start_on(&ts, &id, "main", "host-d29").await;
    let dir = source(&env, "planted", &content);
    assert_costs_one_backup(&env, &dir, root).await;
    env.stop().await;
    ts.stop().await;
}

/// One hint names a shard per file; every file of the backup dedups against
/// a lying shard, and the host still loses only one backup.
async fn planted_hint_costs_one_backup(files: &[&[u8]]) {
    let env = AgentEnv::start(&Backend::Sqlite).await;
    let first = source(&env, "first", b"the first backup, nothing planted");
    let job = run_backup(&env, 0, &first).await;
    let job = env.wait_job_settled(&job).await;
    assert!(
        job["state"] == "succeeded" || job["state"] == "warnings",
        "{job}"
    );

    let pool = &env.ts().state().db.pool;
    let upgraded = wait_until(WAIT, || async {
        sqlx::query("SELECT attest_upgraded_at FROM repositories WHERE id = $1")
            .bind(&env.repo)
            .fetch_one(pool)
            .await
            .and_then(|row| row.try_get::<Option<i64>, _>("attest_upgraded_at"))
            .is_ok_and(|marker| marker.is_some())
    })
    .await;
    assert!(upgraded, "the server did not record the upgrade marker");

    let keys = WriteKeys::from_file_text(
        &std::fs::read_to_string(env.paths.writer_keys()).expect("the agent's writer keys"),
    )
    .unwrap();
    let root = env.ts().local_repo_root("main");
    let store = LocalFsStore::new(root.clone());
    let dir = env.work.path().join("planted");
    std::fs::create_dir_all(&dir).unwrap();
    let mut shards = Vec::new();
    for (n, content) in files.iter().enumerate() {
        std::fs::write(dir.join(format!("file-{n}.txt")), content).unwrap();
        let id = blob_id(&keys.k_id, content);
        shards.push(plant_blobs(&store, &keys, &[(id, BLOB_TYPE_DATA)]).await);
    }
    plant_attestation_hint(&store, &shards).await;

    assert_costs_one_backup(&env, &dir, root).await;
    let distrusted = load_distrusted(env.ts().state(), &env.repo).await.unwrap();
    for shard in &shards {
        assert!(
            distrusted.contains(shard),
            "a planted shard was not distrusted"
        );
    }
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_planted_attestation_hint_costs_one_backup() {
    planted_hint_costs_one_backup(&[b"a file the attacker names through an attestation hint"])
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_two_lying_shards_cost_one_backup() {
    planted_hint_costs_one_backup(&[
        b"the first file the attacker names through an attestation hint",
        b"the second file the attacker names through the same hint",
    ])
    .await;
}
