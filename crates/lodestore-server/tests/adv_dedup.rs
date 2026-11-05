//! A host holding writer credentials plants a shard that names a blob the
//! next backup contains. Whatever the agent does with it, the job must not end
//! up as a success whose snapshot cannot be restored. Only the public path of
//! a job is used, with the real agent, so the scenarios run unchanged against
//! the first release.

#[path = "../../lodestore-format/tests/adv_support/mod.rs"]
mod adv_support;
mod common;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use adv_support::{attacker_keys, plant_blobs};
use common::{AgentEnv, Backend, TestServer, WAIT, wait_until};
use lodestore_format::chunker::blob_id;
use lodestore_format::config::config_hash;
use lodestore_format::crypto::{BLOB_TYPE_DATA, Entropy};
use lodestore_format::index::parse_hex32;
use lodestore_format::keys::{KdfCost, WriteKeys};
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::store::LocalFsStore;
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

/// Either the job fails with `verification_failed`, or its snapshot restores
/// byte for byte. A third outcome never settles, and the job is shown.
async fn assert_never_unrestorable_success(env: &AgentEnv, job: &str, dir: &Path, root: PathBuf) {
    let deadline = Instant::now() + WAIT;
    let mut last: String;
    loop {
        let body = env.json(&format!("/api/v1/jobs/{job}")).await;
        match body["state"].as_str().unwrap() {
            "failed" if body["error_code"] == "verification_failed" => return,
            "succeeded" | "warnings" => match body["snapshot_id"].as_str() {
                Some(snapshot) => match restores(root.clone(), snapshot, dir).await {
                    Ok(()) => return,
                    Err(e) => last = format!("{e}; job {body}"),
                },
                None => last = format!("no snapshot; job {body}"),
            },
            "failed" | "cancelled" | "interrupted" | "not_started" => {
                panic!("the job ended in an unexpected state: {body}")
            }
            _ => last = format!("job {body}"),
        }
        assert!(
            Instant::now() < deadline,
            "the job is a success whose snapshot does not restore: {last}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn source(env: &AgentEnv, label: &str, content: &[u8]) -> PathBuf {
    let dir = env.work.path().join(label);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("file.txt"), content).unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_planted_shard_never_yields_successful_unrestorable_backup() {
    let env = AgentEnv::start(&Backend::Sqlite).await;
    let first = source(&env, "first", b"the first backup, nothing planted");
    let job = run_backup(&env, 0, &first).await;
    let job = env.wait_job(&job).await;
    assert!(
        job["state"] == "succeeded" || job["state"] == "warnings",
        "{job}"
    );

    let pool = &env.ts().state().db.pool;
    let seen = RefCell::new(Ok(false));
    wait_until(WAIT, || async {
        let row = sqlx::query("SELECT attest_upgraded_at FROM repositories WHERE id = $1")
            .bind(&env.repo)
            .fetch_one(pool)
            .await
            .and_then(|row| row.try_get::<Option<i64>, _>("attest_upgraded_at"))
            .map(|marker| marker.is_some())
            .map_err(|e| e.to_string());
        let settled = !matches!(row, Ok(false));
        *seen.borrow_mut() = row;
        settled
    })
    .await;
    match seen.into_inner() {
        Ok(true) => {}
        Ok(false) => panic!(
            "precondition failed: the server did not record the repository upgrade marker \
             within WAIT; the shard must be planted after the marker"
        ),
        Err(e) => panic!("precondition failed: reading the upgrade marker: {e}"),
    }

    let keys = WriteKeys::from_file_text(
        &std::fs::read_to_string(env.paths.writer_keys()).expect("the agent's writer keys"),
    )
    .unwrap();
    let content = b"a file the attacker names before the agent ever reads it".to_vec();
    let root = env.ts().local_repo_root("main");
    let store = LocalFsStore::new(root.clone());
    plant_blobs(
        &store,
        &keys,
        &[(blob_id(&keys.k_id, &content), BLOB_TYPE_DATA)],
    )
    .await;

    let dir = source(&env, "planted", &content);
    let job = run_backup(&env, 1, &dir).await;
    assert_never_unrestorable_success(&env, &job, &dir, root).await;
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_shard_planted_before_upgrade_never_yields_successful_unrestorable_backup() {
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

    let env = AgentEnv::start_on(&ts, &id, "main", "host-d23").await;
    let dir = source(&env, "planted", &content);
    let job = run_backup(&env, 0, &dir).await;
    assert_never_unrestorable_success(&env, &job, &dir, root).await;
    env.stop().await;
    ts.stop().await;
}
