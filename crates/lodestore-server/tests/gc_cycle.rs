mod common;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use common::{Api, TestServer, WAIT, backends, bearer_client, wait_until};
use lodestore_format::crypto::{BLOB_TYPE_DATA, BlobId, Entropy, HPKE_ENC_LEN, SEALED_KEY_LEN};
use lodestore_format::index::{hex32, index_object_key, parse_hex32, seal_shard_superseding};
use lodestore_format::locks::{new_lock_id, publish_gc_manifest};
use lodestore_format::pack::PACK_PREFIX_LEN;
use lodestore_format::pb;
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::repo::Repository;
use lodestore_format::snapshot::snapshot_object_key;
use lodestore_format::store::{LocalFsStore, put_named};
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_server::maintenance::gc::cycle::{StepOutcome, step};
use serde_json::{Value, json};
use sqlx::Row;

const PASSWORD: &[u8] = b"test-password";
const SHARED_SIZE: usize = 64 * 1024;
const UNIQUE_SIZE: usize = 600 * 1024;

fn noise(seed: u64, size: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..size)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

struct Backed {
    id: String,
    dir: PathBuf,
}

async fn backup_files(ts: &TestServer, name: &str, tag: &str, unique_seed: u64) -> Backed {
    let store = LocalFsStore::new(ts.local_repo_root(name));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    let mut writer = Writer::new(&store, keys, repo.config()).await.unwrap();
    let dir = ts.dir.path().join(format!("data-{tag}"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("shared.bin"), noise(1, SHARED_SIZE)).unwrap();
    std::fs::write(dir.join("unique.bin"), noise(unique_seed, UNIQUE_SIZE)).unwrap();
    let outcome = writer
        .backup(
            std::slice::from_ref(&dir),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .expect("backup");
    Backed {
        id: hex32(&outcome.snapshot_id),
        dir,
    }
}

fn forget_snapshot(ts: &TestServer, name: &str, id: &str) {
    let id = parse_hex32(id).unwrap();
    std::fs::remove_file(ts.local_repo_root(name).join(snapshot_object_key(&id))).unwrap();
}

fn age_packs(ts: &TestServer, name: &str) {
    let old = SystemTime::now() - Duration::from_secs(3 * 86_400);
    for path in files_under(&ts.local_repo_root(name).join("packs")) {
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(read) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in read {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(files_under(&path));
        } else {
            out.push(path);
        }
    }
    out.sort();
    out
}

fn packs_size(ts: &TestServer, name: &str) -> u64 {
    files_under(&ts.local_repo_root(name).join("packs"))
        .iter()
        .map(|p| std::fs::metadata(p).unwrap().len())
        .sum()
}

fn lock_files(ts: &TestServer, name: &str) -> Vec<PathBuf> {
    files_under(&ts.local_repo_root(name).join("locks"))
}

async fn assert_restores(ts: &TestServer, name: &str, snapshot: &Backed) {
    let store = LocalFsStore::new(ts.local_repo_root(name));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let mut reader = Reader::open(&store, repo.read_keys().duplicate(), repo.config()).unwrap();
    let target = tempfile::tempdir().unwrap();
    let report = reader
        .restore(
            &parse_hex32(&snapshot.id).unwrap(),
            target.path(),
            &RestoreOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let root = target.path().join(
        snapshot
            .dir
            .canonicalize()
            .unwrap()
            .strip_prefix("/")
            .unwrap(),
    );
    for file in ["shared.bin", "unique.bin"] {
        assert!(
            std::fs::read(root.join(file)).unwrap()
                == std::fs::read(snapshot.dir.join(file)).unwrap(),
            "{file} of snapshot {} was not restored byte for byte",
            snapshot.id
        );
    }
}

fn enabled_input() -> Value {
    json!({
        "enabled": true,
        "cron": "0 3 * * 0",
        "timezone": "UTC",
        "pause": "24h",
        "repack_below_pct": 50,
        "max_repack": "10GiB",
    })
}

async fn put_settings(ts: &TestServer, api: &Api, repo: &str, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .put(format!(
            "{}/api/v1/repositories/{repo}/gc/settings",
            ts.rest_url()
        ))
        .header("cookie", api.cookie())
        .header("x-csrf-token", &api.csrf)
        .json(body)
        .send()
        .await
        .unwrap()
}

async fn enable(ts: &TestServer, api: &Api, repo: &str) {
    let response = put_settings(ts, api, repo, &enabled_input()).await;
    assert_eq!(response.status(), 200, "{:?}", response.text().await);
}

async fn start_run(api: &Api, repo: &str) -> reqwest::Response {
    api.post(&format!("/api/v1/repositories/{repo}/gc/run"), &json!({}))
        .await
}

struct Cycle {
    state: String,
    delete_after: Option<i64>,
    error: Option<String>,
    error_after_delete: bool,
    sweep_started: bool,
}

async fn cycle(ts: &TestServer, id: &str) -> Cycle {
    let row = sqlx::query(
        "SELECT state, delete_after, error, error_after_delete, sweep_started \
         FROM gc_cycles WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&ts.state().db.pool)
    .await
    .unwrap();
    Cycle {
        state: row.get("state"),
        delete_after: row.get("delete_after"),
        error: row.get("error"),
        error_after_delete: row.get::<i64, _>("error_after_delete") != 0,
        sweep_started: row.get::<i64, _>("sweep_started") != 0,
    }
}

async fn wait_state(ts: &TestServer, id: &str, state: &str) {
    let reached = wait_until(WAIT, || async { cycle(ts, id).await.state == state }).await;
    if !reached {
        let now = cycle(ts, id).await;
        panic!(
            "the cycle never reached {state}: it is {} ({:?})",
            now.state, now.error
        );
    }
}

async fn run_cycle(api: &Api, repo: &str) -> String {
    let response = start_run(api, repo).await;
    assert_eq!(response.status(), 202, "{:?}", response.text().await);
    let body: Value = response.json().await.unwrap();
    body["cycle_id"].as_str().unwrap().to_string()
}

/// Steps until the cycle stops advancing and returns how it stopped.
async fn steps(ts: &TestServer, id: &str, now_ms: i64) -> StepOutcome {
    loop {
        match step(ts.state(), id, now_ms).await.unwrap() {
            StepOutcome::Advanced(_) => {}
            other => return other,
        }
    }
}

async fn after_pause(ts: &TestServer, id: &str) -> i64 {
    wait_state(ts, id, "waiting").await;
    cycle(ts, id).await.delete_after.unwrap() + 1_000
}

async fn report(api: &Api, repo: &str, id: &str) -> Value {
    let response = api
        .get(&format!(
            "/api/v1/repositories/{repo}/gc/cycles/{id}/report"
        ))
        .await;
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

/// A repository whose snapshot A was forgotten; B and C remain.
async fn forgotten_a(ts: &TestServer, name: &str) -> (Backed, Backed) {
    let a = backup_files(ts, name, "a", 11).await;
    let b = backup_files(ts, name, "b", 12).await;
    let c = backup_files(ts, name, "c", 13).await;
    forget_snapshot(ts, name, &a.id);
    age_packs(ts, name);
    (b, c)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_frees_space_and_restores() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let (b, c) = forgotten_a(&ts, "main").await;
        enable(&ts, &api, &repo).await;
        let before = packs_size(&ts, "main");

        let id = run_cycle(&api, &repo).await;
        let later = after_pause(&ts, &id).await;
        assert!(
            !lock_files(&ts, "main").is_empty(),
            "the marker must be published"
        );
        assert!(
            packs_size(&ts, "main") > before,
            "the repack adds packs before anything goes"
        );

        assert_eq!(
            steps(&ts, &id, later).await,
            StepOutcome::Finished(lodestore_server::maintenance::gc::GcState::Done),
            "[{}]",
            backend.name()
        );
        assert!(
            packs_size(&ts, "main") < before,
            "[{}] packs/ did not shrink",
            backend.name()
        );
        assert!(lock_files(&ts, "main").is_empty(), "locks/ must be empty");
        assert_restores(&ts, "main", &b).await;
        assert_restores(&ts, "main", &c).await;

        let done = report(&api, &repo, &id).await;
        assert!(done["bytes_freed"].as_u64().unwrap() > 0, "{done}");
        assert!(done["packs_repacked"].as_u64().unwrap() >= 1, "{done}");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_nothing_to_free() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        backup_files(&ts, "main", "a", 11).await;
        backup_files(&ts, "main", "b", 12).await;
        age_packs(&ts, "main");
        let mut input = enabled_input();
        input["repack_below_pct"] = json!(1);
        let saved = put_settings(&ts, &api, &repo, &input).await;
        assert_eq!(saved.status(), 200);
        let before = packs_size(&ts, "main");

        let id = run_cycle(&api, &repo).await;
        wait_state(&ts, &id, "done").await;
        assert!(
            lock_files(&ts, "main").is_empty(),
            "no marker for an empty plan"
        );
        assert_eq!(packs_size(&ts, "main"), before);
        let done = report(&api, &repo, &id).await;
        assert_eq!(done["bytes_freed"], 0);
        assert_eq!(done["packs_deleted"], 0);
        let events = api
            .get(&format!("/api/v1/repositories/{repo}/gc"))
            .await
            .json::<Value>()
            .await
            .unwrap();
        let shown = events["events"].to_string();
        assert!(shown.contains("Nothing to free."), "{shown}");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_run_requires_enabled() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let response = start_run(&api, &repo).await;
        assert_eq!(response.status(), 409, "[{}]", backend.name());
        let body: Value = response.json().await.unwrap();
        assert_eq!(
            body["message"],
            "Turn on cleanup for this repository first."
        );
        let unknown = start_run(&api, "unknown").await;
        assert_eq!(unknown.status(), 404);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_second_run_frees_nothing() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let (b, c) = forgotten_a(&ts, "main").await;
        enable(&ts, &api, &repo).await;

        let first = run_cycle(&api, &repo).await;
        let later = after_pause(&ts, &first).await;
        steps(&ts, &first, later).await;
        assert_eq!(cycle(&ts, &first).await.state, "done");
        age_packs(&ts, "main");
        let after_first = packs_size(&ts, "main");

        let second = run_cycle(&api, &repo).await;
        wait_state(&ts, &second, "done").await;
        let done = report(&api, &repo, &second).await;
        assert_eq!(done["bytes_freed"], 0, "[{}] {done}", backend.name());
        assert_eq!(packs_size(&ts, "main"), after_first);
        assert_restores(&ts, "main", &b).await;
        assert_restores(&ts, "main", &c).await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_d15_failure_deletes_nothing() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        forgotten_a(&ts, "main").await;
        enable(&ts, &api, &repo).await;

        let id = run_cycle(&api, &repo).await;
        let later = after_pause(&ts, &id).await;
        let packs = files_under(&ts.local_repo_root("main").join("packs"));
        for shard in files_under(&ts.local_repo_root("main").join("index")) {
            std::fs::remove_file(shard).unwrap();
        }

        assert_eq!(
            steps(&ts, &id, later).await,
            StepOutcome::Finished(lodestore_server::maintenance::gc::GcState::Failed),
            "[{}]",
            backend.name()
        );
        let failed = cycle(&ts, &id).await;
        assert_eq!(failed.state, "failed");
        assert!(!failed.error_after_delete);
        assert!(!failed.sweep_started);
        assert!(failed.error.is_some());
        assert_eq!(
            files_under(&ts.local_repo_root("main").join("packs")),
            packs
        );
        assert!(
            lock_files(&ts, "main").is_empty(),
            "the marker must be taken down"
        );
        ts.stop().await;
    }
}

fn fake_pack_record(n: u32) -> pb::PackRecord {
    let id = |tag: u8| {
        let mut id = [tag; 32];
        id[..4].copy_from_slice(&n.to_le_bytes());
        id
    };
    let mut entries = id(0xB1).to_vec();
    entries.push(BLOB_TYPE_DATA);
    entries.extend_from_slice(&(PACK_PREFIX_LEN as u32).to_le_bytes());
    entries.extend_from_slice(&100u32.to_le_bytes());
    entries.extend_from_slice(&50u32.to_le_bytes());
    pb::PackRecord {
        pack_id: id(0xFA).to_vec(),
        hpke_enc: vec![0; HPKE_ENC_LEN],
        sealed_pack_key: vec![0; SEALED_KEY_LEN],
        pack_size: 5000,
        entries,
    }
}

async fn put_shard(
    ts: &TestServer,
    name: &str,
    packs: Vec<pb::PackRecord>,
    supersedes: Vec<BlobId>,
) -> BlobId {
    let store = LocalFsStore::new(ts.local_repo_root(name));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let keys = repo.read_keys();
    let (id, bytes) = seal_shard_superseding(
        keys.k_idx(),
        keys.repo_id(),
        packs,
        supersedes,
        &mut Entropy::os(),
    )
    .unwrap();
    put_named(&store, &index_object_key(&id), bytes)
        .await
        .unwrap();
    id
}

/// A shard any writer key can plant names a large shard the compaction keeps
/// in `supersedes`; the sweep deletes only what the compaction replaced.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_sweep_deletes_only_its_own_shards() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let (b, c) = forgotten_a(&ts, "main").await;
        let big = put_shard(
            &ts,
            "main",
            (0..600).map(fake_pack_record).collect(),
            vec![],
        )
        .await;
        put_shard(&ts, "main", vec![], vec![big]).await;
        enable(&ts, &api, &repo).await;

        let id = run_cycle(&api, &repo).await;
        let later = after_pause(&ts, &id).await;
        assert_eq!(
            steps(&ts, &id, later).await,
            StepOutcome::Finished(lodestore_server::maintenance::gc::GcState::Done),
            "[{}] {:?}",
            backend.name(),
            cycle(&ts, &id).await.error
        );
        assert!(
            ts.local_repo_root("main")
                .join(index_object_key(&big))
                .exists(),
            "[{}] the sweep deleted a shard the compaction did not replace",
            backend.name()
        );
        assert_restores(&ts, "main", &b).await;
        assert_restores(&ts, "main", &c).await;
        ts.stop().await;
    }
}

fn pack_path(ts: &TestServer, name: &str, hex: &str) -> PathBuf {
    ts.local_repo_root(name)
        .join(lodestore_format::pack::pack_object_key(
            &parse_hex32(hex).unwrap(),
        ))
}

async fn marked_packs(api: &Api, repo: &str, id: &str) -> Vec<String> {
    let full = report(api, repo, id).await;
    ["delete", "repack"]
        .iter()
        .flat_map(|list| full[list].as_array().unwrap().clone())
        .map(|p| p["id"].as_str().unwrap().to_string())
        .collect()
}

async fn finish_ok(ts: &TestServer, id: &str, later: i64, before: u64, b: &Backed, c: &Backed) {
    assert_eq!(
        steps(ts, id, later).await,
        StepOutcome::Finished(lodestore_server::maintenance::gc::GcState::Done)
    );
    assert!(packs_size(ts, "main") < before, "packs/ did not shrink");
    assert!(lock_files(ts, "main").is_empty(), "locks/ must be empty");
    assert_restores(ts, "main", b).await;
    assert_restores(ts, "main", c).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_restart_each_step() {
    for backend in backends() {
        for stop_at in ["repacking", "waiting", "verifying", "sweeping"] {
            let mut ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
            let api = ts.api().await;
            let repo = ts.create_local_repository("main").await;
            let (b, c) = forgotten_a(&ts, "main").await;
            enable(&ts, &api, &repo).await;
            let before = packs_size(&ts, "main");

            let id = run_cycle(&api, &repo).await;
            let later = after_pause(&ts, &id).await;
            match stop_at {
                "repacking" => {
                    sqlx::query("UPDATE gc_cycles SET state = 'repacking' WHERE id = $1")
                        .bind(&id)
                        .execute(&ts.state().db.pool)
                        .await
                        .unwrap();
                }
                "verifying" => {
                    step(ts.state(), &id, later).await.unwrap();
                }
                "sweeping" => {
                    step(ts.state(), &id, later).await.unwrap();
                    step(ts.state(), &id, later).await.unwrap();
                }
                _ => {}
            }
            assert_eq!(cycle(&ts, &id).await.state, stop_at, "[{}]", backend.name());

            ts.restart().await;
            let later = if stop_at == "repacking" {
                steps(&ts, &id, later - 2_000).await;
                cycle(&ts, &id).await.delete_after.unwrap() + 1_000
            } else {
                later
            };
            finish_ok(&ts, &id, later, before, &b, &c).await;
            ts.stop().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_resume_partial_sweep() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let (b, c) = forgotten_a(&ts, "main").await;
        enable(&ts, &api, &repo).await;
        let before = packs_size(&ts, "main");

        let id = run_cycle(&api, &repo).await;
        let later = after_pause(&ts, &id).await;
        step(ts.state(), &id, later).await.unwrap();
        step(ts.state(), &id, later).await.unwrap();
        assert_eq!(cycle(&ts, &id).await.state, "sweeping");
        let marked = marked_packs(&api, &repo, &id).await;
        assert!(!marked.is_empty());
        std::fs::remove_file(pack_path(&ts, "main", &marked[0])).unwrap();

        finish_ok(&ts, &id, later, before, &b, &c).await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_failed_sweep_resumed_by_next_run() {
    use std::os::unix::fs::PermissionsExt;
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let (b, c) = forgotten_a(&ts, "main").await;
        enable(&ts, &api, &repo).await;
        let before = packs_size(&ts, "main");

        let id = run_cycle(&api, &repo).await;
        let later = after_pause(&ts, &id).await;
        let marked = marked_packs(&api, &repo, &id).await;
        let locked_dir = pack_path(&ts, "main", &marked[0])
            .parent()
            .unwrap()
            .to_path_buf();
        std::fs::set_permissions(&locked_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::File::create(locked_dir.join("probe")).is_ok() {
            std::fs::remove_file(locked_dir.join("probe")).unwrap();
            eprintln!("skipped: this user can write to a read-only folder");
            ts.stop().await;
            return;
        }

        assert_eq!(
            steps(&ts, &id, later).await,
            StepOutcome::Finished(lodestore_server::maintenance::gc::GcState::Failed),
            "[{}]",
            backend.name()
        );
        let failed = cycle(&ts, &id).await;
        assert!(
            failed.error_after_delete && failed.sweep_started,
            "{:?}",
            failed.error
        );
        assert!(!lock_files(&ts, "main").is_empty(), "the marker must stay");

        std::fs::set_permissions(&locked_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let again = run_cycle(&api, &repo).await;
        assert_eq!(again, id, "the next run continues the same cycle");
        wait_state(&ts, &id, "done").await;
        assert!(packs_size(&ts, "main") < before);
        assert!(lock_files(&ts, "main").is_empty());
        assert_restores(&ts, "main", &b).await;
        assert_restores(&ts, "main", &c).await;
        ts.stop().await;
    }
}

/// A cycle that stopped while deleting is retried by the server itself,
/// after a rest, even when cleanup has been turned off since.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_stopped_sweep_retried_when_off() {
    use std::os::unix::fs::PermissionsExt;
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let (b, c) = forgotten_a(&ts, "main").await;
        enable(&ts, &api, &repo).await;
        let before = packs_size(&ts, "main");

        let id = run_cycle(&api, &repo).await;
        let later = after_pause(&ts, &id).await;
        let marked = marked_packs(&api, &repo, &id).await;
        let locked_dir = pack_path(&ts, "main", &marked[0])
            .parent()
            .unwrap()
            .to_path_buf();
        std::fs::set_permissions(&locked_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::File::create(locked_dir.join("probe")).is_ok() {
            std::fs::remove_file(locked_dir.join("probe")).unwrap();
            eprintln!("skipped: this user can write to a read-only folder");
            ts.stop().await;
            return;
        }
        steps(&ts, &id, later).await;
        assert!(cycle(&ts, &id).await.error_after_delete);
        std::fs::set_permissions(&locked_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut off = enabled_input();
        off["enabled"] = json!(false);
        assert_eq!(put_settings(&ts, &api, &repo, &off).await.status(), 200);

        let tick = lodestore_server::maintenance::gc::tick_once;
        tick(ts.state(), wall_ms()).await.unwrap();
        assert_eq!(cycle(&ts, &id).await.state, "failed", "it rests first");
        tick(ts.state(), wall_ms() + 10 * 60_000).await.unwrap();
        wait_state(&ts, &id, "done").await;
        assert!(packs_size(&ts, "main") < before, "[{}]", backend.name());
        assert!(lock_files(&ts, "main").is_empty());
        assert_restores(&ts, "main", &b).await;
        assert_restores(&ts, "main", &c).await;
        ts.stop().await;
    }
}

async fn abort(api: &Api, repo: &str) -> reqwest::Response {
    api.post(&format!("/api/v1/repositories/{repo}/gc/abort"), &json!({}))
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_abort_before_sweep() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        forgotten_a(&ts, "main").await;
        enable(&ts, &api, &repo).await;

        let id = run_cycle(&api, &repo).await;
        let later = after_pause(&ts, &id).await;
        let packs = files_under(&ts.local_repo_root("main").join("packs"));
        assert!(!lock_files(&ts, "main").is_empty());

        let response = abort(&api, &repo).await;
        assert_eq!(response.status(), 200, "[{}]", backend.name());
        assert_eq!(cycle(&ts, &id).await.state, "aborted");
        assert!(
            lock_files(&ts, "main").is_empty(),
            "the marker must be gone"
        );
        assert_eq!(
            files_under(&ts.local_repo_root("main").join("packs")),
            packs
        );
        assert_eq!(
            steps(&ts, &id, later).await,
            StepOutcome::Finished(lodestore_server::maintenance::gc::GcState::Aborted)
        );
        assert_eq!(abort(&api, &repo).await.status(), 409);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_abort_while_sweeping_refused() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        forgotten_a(&ts, "main").await;
        enable(&ts, &api, &repo).await;

        let id = run_cycle(&api, &repo).await;
        let later = after_pause(&ts, &id).await;
        step(ts.state(), &id, later).await.unwrap();
        step(ts.state(), &id, later).await.unwrap();
        assert_eq!(cycle(&ts, &id).await.state, "sweeping");

        let response = abort(&api, &repo).await;
        assert_eq!(response.status(), 409, "[{}]", backend.name());
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["message"], "This step can't be aborted.");
        assert_eq!(cycle(&ts, &id).await.state, "sweeping");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_abort_during_marking() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        forgotten_a(&ts, "main").await;
        enable(&ts, &api, &repo).await;

        let id = lodestore_server::maintenance::gc::start_cycle(
            ts.state(),
            &repo,
            false,
            lodestore_server::maintenance::gc::GcOrigin::Manual,
            "test",
        )
        .await
        .unwrap();
        let state = ts.state().clone();
        let running = {
            let id = id.clone();
            tokio::spawn(async move { step(&state, &id, lodestore_server::db::now_ms()).await })
        };
        assert_eq!(abort(&api, &repo).await.status(), 200);
        running.await.unwrap().unwrap();

        assert_eq!(
            cycle(&ts, &id).await.state,
            "aborted",
            "[{}]",
            backend.name()
        );
        assert!(
            lock_files(&ts, "main").is_empty(),
            "no marker may survive an abort"
        );
        ts.stop().await;
    }
}

async fn seed_restore_job(ts: &TestServer, repo: &str, state: &str) {
    sqlx::query(
        "INSERT INTO agents (id, hostname, repository_id, labels_json, os, arch, version, status, \
         enrolled_at, cert_serial, cert_issued_at, cert_not_after, capabilities_json) \
         VALUES ('host-1', 'host-1', $1, '{}', 'linux', 'x86_64', 'test', 'active', 0, 'host-1', 0, 0, '[\"gc_v1\"]')",
    )
    .bind(repo)
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
    let record = json!({
        "snapshot_id": "00",
        "source_repository_id": repo,
        "source_host_id": null,
        "source_hostname": "host-1",
        "items_b64": [],
        "destination": "/tmp/x",
        "in_place": false,
        "conflict": "skip_existing",
        "restore_owners": false,
        "started_by": "test",
    });
    sqlx::query(
        "INSERT INTO jobs (id, host_id, source, state, spec, queued_at, kind, restore_json) \
         VALUES ('restore-1', 'host-1', 'manual', $1, $2, 0, 'restore', $3)",
    )
    .bind(state)
    .bind(Vec::<u8>::new())
    .bind(record.to_string())
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_waits_for_restore() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let (b, c) = forgotten_a(&ts, "main").await;
        enable(&ts, &api, &repo).await;
        let before = packs_size(&ts, "main");
        seed_restore_job(&ts, &repo, "running").await;

        let id = run_cycle(&api, &repo).await;
        let later = after_pause(&ts, &id).await;
        let packs = files_under(&ts.local_repo_root("main").join("packs"));
        assert_eq!(
            steps(&ts, &id, later).await,
            StepOutcome::Postponed,
            "[{}]",
            backend.name()
        );
        let status: Value = api
            .get(&format!("/api/v1/repositories/{repo}/gc"))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(status["cycle"]["waiting_for_restores"], true);
        assert_eq!(status["cycle"]["state"], "verifying");
        assert_eq!(
            files_under(&ts.local_repo_root("main").join("packs")),
            packs
        );

        sqlx::query("UPDATE jobs SET state = 'succeeded' WHERE id = 'restore-1'")
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        finish_ok(&ts, &id, later, before, &b, &c).await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_restore_after_gc() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let (b, c) = forgotten_a(&ts, "main").await;
        enable(&ts, &api, &repo).await;
        let synced = api
            .post(
                &format!("/api/v1/repositories/{repo}/catalog/sync"),
                &json!({}),
            )
            .await;
        assert_eq!(synced.status(), 200);

        let listing = |backed: &Backed| {
            let path = data_encoding::BASE64URL_NOPAD.encode(
                backed
                    .dir
                    .canonicalize()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .as_bytes(),
            );
            format!(
                "/api/v1/snapshots/{}/entries?path={path}&offset=0",
                backed.id
            )
        };
        let warm = api.get(&listing(&b)).await;
        assert_eq!(warm.status(), 200);
        let names_before: Value = warm.json().await.unwrap();

        let before = packs_size(&ts, "main");
        let id = run_cycle(&api, &repo).await;
        let later = after_pause(&ts, &id).await;
        finish_ok(&ts, &id, later, before, &b, &c).await;

        for snapshot in [&b, &c] {
            let page = api.get(&listing(snapshot)).await;
            assert_eq!(page.status(), 200, "[{}]", backend.name());
        }
        let again: Value = api.get(&listing(&b)).await.json().await.unwrap();
        assert_eq!(again["entries"], names_before["entries"]);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_tick_resumes_once() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let (b, c) = forgotten_a(&ts, "main").await;
        enable(&ts, &api, &repo).await;
        let before = packs_size(&ts, "main");

        let id = run_cycle(&api, &repo).await;
        after_pause(&ts, &id).await;
        sqlx::query("UPDATE gc_cycles SET delete_after = 1 WHERE id = $1")
            .bind(&id)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();

        let now = lodestore_server::db::now_ms();
        let guard = ts.state().gc.claim(&id).expect("not driven yet");
        assert!(ts.state().gc.claim(&id).is_none(), "one driver per cycle");
        lodestore_server::maintenance::gc::tick_once(ts.state(), now)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            cycle(&ts, &id).await.state,
            "waiting",
            "[{}]",
            backend.name()
        );

        drop(guard);
        lodestore_server::maintenance::gc::tick_once(ts.state(), now)
            .await
            .unwrap();
        wait_state(&ts, &id, "done").await;
        assert!(packs_size(&ts, "main") < before);
        assert_restores(&ts, "main", &b).await;
        assert_restores(&ts, "main", &c).await;
        ts.stop().await;
    }
}

async fn put_marker(ts: &TestServer, host: &[u8]) -> lodestore_format::locks::LockId {
    let store = LocalFsStore::new(ts.local_repo_root("main"));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let keys = repo.read_keys();
    lodestore_format::locks::put_writer_marker(
        &store,
        keys.k_idx(),
        keys.repo_id(),
        host,
        &mut lodestore_format::crypto::Entropy::os(),
    )
    .await
    .unwrap()
}

async fn close_marker(ts: &TestServer, id: &lodestore_format::locks::LockId) {
    let store = LocalFsStore::new(ts.local_repo_root("main"));
    lodestore_format::locks::close_writer_marker(&store, id)
        .await
        .unwrap();
}

async fn blocked(api: &Api, repo: &str) -> Vec<Value> {
    let status: Value = api
        .get(&format!("/api/v1/repositories/{repo}/gc"))
        .await
        .json()
        .await
        .unwrap();
    status["cycle"]["blocked_by_writers"]
        .as_array()
        .unwrap()
        .clone()
}

async fn events_of(ts: &TestServer, id: &str, kind: &str) -> i64 {
    sqlx::query("SELECT COUNT(*) AS n FROM gc_events WHERE cycle_id = $1 AND kind = $2")
        .bind(id)
        .bind(kind)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .get("n")
}

async fn unlock(api: &Api, repo: &str) -> reqwest::Response {
    api.post(
        &format!("/api/v1/repositories/{repo}/gc/unlock"),
        &json!({}),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_writer_marker_postpones() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        forgotten_a(&ts, "main").await;
        enable(&ts, &api, &repo).await;
        let marker = put_marker(&ts, b"host-x").await;

        let id = run_cycle(&api, &repo).await;
        let later = after_pause(&ts, &id).await;
        let packs = files_under(&ts.local_repo_root("main").join("packs"));
        assert_eq!(
            steps(&ts, &id, later).await,
            StepOutcome::Postponed,
            "[{}]",
            backend.name()
        );
        assert_eq!(steps(&ts, &id, later).await, StepOutcome::Postponed);

        let shown = blocked(&api, &repo).await;
        assert_eq!(shown.len(), 1, "{shown:?}");
        assert_eq!(
            shown[0]["key"],
            lodestore_format::locks::writer_marker_key(&marker)
        );
        assert_eq!(shown[0]["host"], "host-x");
        assert!(shown[0]["started_at"].as_i64().unwrap() > 0);
        assert_eq!(cycle(&ts, &id).await.state, "waiting");
        assert_eq!(
            files_under(&ts.local_repo_root("main").join("packs")),
            packs
        );
        assert_eq!(events_of(&ts, &id, "postponed").await, 1);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_closed_marker_continues() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let (b, c) = forgotten_a(&ts, "main").await;
        enable(&ts, &api, &repo).await;
        let before = packs_size(&ts, "main");
        let marker = put_marker(&ts, b"host-x").await;

        let id = run_cycle(&api, &repo).await;
        let later = after_pause(&ts, &id).await;
        assert_eq!(steps(&ts, &id, later).await, StepOutcome::Postponed);

        close_marker(&ts, &marker).await;
        finish_ok(&ts, &id, later, before, &b, &c).await;
        assert!(
            lock_files(&ts, "main").is_empty(),
            "the closed pair must be removed"
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_unreadable_marker_postpones() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        forgotten_a(&ts, "main").await;
        enable(&ts, &api, &repo).await;
        let locks = ts.local_repo_root("main").join("locks");
        std::fs::create_dir_all(&locks).unwrap();
        std::fs::write(
            locks.join(format!("writer-{}", "ab".repeat(16))),
            b"garbage",
        )
        .unwrap();

        let id = run_cycle(&api, &repo).await;
        let later = after_pause(&ts, &id).await;
        assert_eq!(
            steps(&ts, &id, later).await,
            StepOutcome::Postponed,
            "[{}]",
            backend.name()
        );
        let shown = blocked(&api, &repo).await;
        assert_eq!(shown.len(), 1);
        assert!(shown[0]["host"].is_null(), "{shown:?}");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_marker_after_publish_ignored() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let (b, c) = forgotten_a(&ts, "main").await;
        enable(&ts, &api, &repo).await;
        let before = packs_size(&ts, "main");

        let id = run_cycle(&api, &repo).await;
        let later = after_pause(&ts, &id).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let marker = put_marker(&ts, b"late-host").await;

        finish_ok_keeping_locks(&ts, &id, later, before, &b, &c).await;
        let left = lock_files(&ts, "main");
        assert_eq!(left.len(), 1, "[{}] {left:?}", backend.name());
        assert!(
            left[0]
                .to_str()
                .unwrap()
                .ends_with(&lodestore_format::locks::lock_id_hex(&marker))
        );
        ts.stop().await;
    }
}

async fn finish_ok_keeping_locks(
    ts: &TestServer,
    id: &str,
    later: i64,
    before: u64,
    b: &Backed,
    c: &Backed,
) {
    assert_eq!(
        steps(ts, id, later).await,
        StepOutcome::Finished(lodestore_server::maintenance::gc::GcState::Done)
    );
    assert!(packs_size(ts, "main") < before, "packs/ did not shrink");
    assert_restores(ts, "main", b).await;
    assert_restores(ts, "main", c).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_unlock_markers() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let (b, c) = forgotten_a(&ts, "main").await;
        enable(&ts, &api, &repo).await;
        let before = packs_size(&ts, "main");
        put_marker(&ts, b"host-1").await;
        put_marker(&ts, b"host-2").await;

        let id = run_cycle(&api, &repo).await;
        let later = after_pause(&ts, &id).await;
        assert_eq!(steps(&ts, &id, later).await, StepOutcome::Postponed);
        assert_eq!(blocked(&api, &repo).await.len(), 2);

        let response = unlock(&api, &repo).await;
        assert_eq!(response.status(), 200, "[{}]", backend.name());
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["unlocked"], 2);
        assert_eq!(events_of(&ts, &id, "unlocked").await, 1);

        finish_ok(&ts, &id, later, before, &b, &c).await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_unlock_scope_and_conflict() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        forgotten_a(&ts, "main").await;
        enable(&ts, &api, &repo).await;
        let url = format!("{}/api/v1/repositories/{repo}/gc/unlock", ts.rest_url());

        let weak = bearer_client(&ts.issue_token(&["jobs:run"]).await);
        let response = weak.post(&url).json(&json!({})).send().await.unwrap();
        assert_eq!(response.status(), 403, "[{}]", backend.name());

        let response = unlock(&api, &repo).await;
        assert_eq!(response.status(), 409);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["message"], "There are no markers to unlock.");

        let id = run_cycle(&api, &repo).await;
        after_pause(&ts, &id).await;
        assert_eq!(unlock(&api, &repo).await.status(), 409);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_marker_never_expires() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        forgotten_a(&ts, "main").await;
        enable(&ts, &api, &repo).await;
        put_marker(&ts, b"host-x").await;

        let id = run_cycle(&api, &repo).await;
        let later = after_pause(&ts, &id).await;
        for days in [1, 10, 365] {
            assert_eq!(
                steps(&ts, &id, later + days * 86_400_000).await,
                StepOutcome::Postponed,
                "[{}] after {days} days",
                backend.name()
            );
        }
        assert_eq!(cycle(&ts, &id).await.state, "waiting");
        ts.stop().await;
    }
}

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;

fn wall_ms() -> i64 {
    lodestore_server::db::now_ms()
}

/// A cycle that is stepped by hand, with no drive in the background.
async fn manual_cycle(ts: &TestServer, repo: &str) -> String {
    lodestore_server::maintenance::gc::start_cycle(
        ts.state(),
        repo,
        false,
        lodestore_server::maintenance::gc::GcOrigin::Manual,
        "test",
    )
    .await
    .unwrap()
}

async fn published_at(ts: &TestServer, id: &str) -> i64 {
    sqlx::query("SELECT published_at FROM gc_cycles WHERE id = $1")
        .bind(id)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .get("published_at")
}

/// The walk and the repack take time before the marker exists; the pause
/// counts from the marker, not from when the step began.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_pause_counts_from_publication() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        forgotten_a(&ts, "main").await;
        let id = manual_cycle(&ts, &repo).await;

        let before = wall_ms();
        let began = before - 2 * HOUR_MS;
        assert!(matches!(
            steps(&ts, &id, began).await,
            StepOutcome::Waiting { .. }
        ));
        let published = published_at(&ts, &id).await;
        assert!(
            published >= before,
            "[{}] T0 is before the PUT",
            backend.name()
        );
        assert!(cycle(&ts, &id).await.delete_after.unwrap() >= before + DAY_MS);
        ts.stop().await;
    }
}

async fn seed_agent(ts: &TestServer, repo: &str, id: &str, capabilities: &str) {
    sqlx::query(
        "INSERT INTO agents (id, hostname, repository_id, labels_json, os, arch, version, status, \
         enrolled_at, cert_serial, cert_issued_at, cert_not_after, capabilities_json) \
         VALUES ($1, $1, $2, '{}', 'linux', 'x86_64', 'test', 'active', 0, $1, 0, 0, $3)",
    )
    .bind(id)
    .bind(repo)
    .bind(capabilities)
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
}

async fn seed_backup_job(ts: &TestServer, host: &str, began_ms: i64, max_runtime_s: u64) {
    use prost::Message;
    let spec = lodestore_proto::agent::v1::JobSpec {
        max_runtime_s,
        ..Default::default()
    };
    sqlx::query(
        "INSERT INTO jobs (id, host_id, source, state, spec, queued_at, assigned_at, started_at, kind) \
         VALUES ($1, $1, 'manual', 'running', $2, $3, $3, $3, 'backup')",
    )
    .bind(host)
    .bind(spec.encode_to_vec())
    .bind(began_ms)
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
}

/// A running backup keeps the limit it was queued with, even when its
/// policy is shortened or deleted later.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_pause_covers_running_backups() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        forgotten_a(&ts, "main").await;
        seed_agent(&ts, &repo, "host-1", r#"["gc_v1"]"#).await;
        let began = wall_ms() - HOUR_MS;
        seed_backup_job(&ts, "host-1", began, 3 * 86_400).await;
        let id = manual_cycle(&ts, &repo).await;

        let StepOutcome::Waiting { until_ms } = steps(&ts, &id, wall_ms()).await else {
            panic!("the cycle must wait");
        };
        assert!(
            until_ms >= began + 3 * DAY_MS + HOUR_MS,
            "[{}] the pause ends before the running backup's limit",
            backend.name()
        );

        let published = published_at(&ts, &id).await;
        seed_agent(&ts, &repo, "host-2", r#"["gc_v1"]"#).await;
        seed_backup_job(&ts, "host-2", published - 1, 5 * 86_400).await;
        let StepOutcome::Waiting { until_ms } = steps(&ts, &id, until_ms + 1).await else {
            panic!("a backup that began before the marker must hold the cycle back");
        };
        assert!(until_ms >= published - 1 + 5 * DAY_MS + HOUR_MS);
        assert_eq!(cycle(&ts, &id).await.delete_after, Some(until_ms));
        ts.stop().await;
    }
}

/// Listings give the bucket's time to the second, so a writer marker in the
/// same second as the cleanup marker may have read `locks/` before it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_marker_same_second_postpones() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        forgotten_a(&ts, "main").await;
        enable(&ts, &api, &repo).await;

        let id = run_cycle(&api, &repo).await;
        let later = after_pause(&ts, &id).await;
        let writer = put_marker(&ts, b"host-x").await;
        let locks = ts.local_repo_root("main").join("locks");
        let manifest = lock_files(&ts, "main")
            .into_iter()
            .find(|p| p.file_name().unwrap().to_str().unwrap().starts_with("gc-"))
            .unwrap();
        let published = std::fs::metadata(&manifest).unwrap().modified().unwrap();
        File::options()
            .write(true)
            .open(locks.join(format!(
                "writer-{}",
                lodestore_format::locks::lock_id_hex(&writer)
            )))
            .unwrap()
            .set_modified(published)
            .unwrap();

        assert_eq!(
            steps(&ts, &id, later).await,
            StepOutcome::Postponed,
            "[{}]",
            backend.name()
        );
        ts.stop().await;
    }
}

/// An agent that neither reads the marker nor keeps to a time limit joins
/// during the pause: the cycle stops before it deletes anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_old_agent_joining_stops_before_delete() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        forgotten_a(&ts, "main").await;
        enable(&ts, &api, &repo).await;

        let id = run_cycle(&api, &repo).await;
        let later = after_pause(&ts, &id).await;
        let packs = files_under(&ts.local_repo_root("main").join("packs"));
        seed_agent(&ts, &repo, "old-host", "[]").await;

        assert_eq!(
            steps(&ts, &id, later).await,
            StepOutcome::Finished(lodestore_server::maintenance::gc::GcState::Failed),
            "[{}]",
            backend.name()
        );
        let failed = cycle(&ts, &id).await;
        assert!(!failed.sweep_started && !failed.error_after_delete);
        assert!(failed.error.unwrap().contains("old-host"));
        assert_eq!(
            files_under(&ts.local_repo_root("main").join("packs")),
            packs
        );
        assert!(lock_files(&ts, "main").is_empty(), "the marker must go");
        let refused = start_run(&api, &repo).await;
        assert_eq!(refused.status(), 409);
        ts.stop().await;
    }
}

/// A marker a failed cycle could not take down is this server's own: it does
/// not block the next run, which removes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_left_marker_removed_by_next_run() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let (b, c) = forgotten_a(&ts, "main").await;
        enable(&ts, &api, &repo).await;

        let first = run_cycle(&api, &repo).await;
        after_pause(&ts, &first).await;
        let left = lock_files(&ts, "main");
        assert_eq!(left.len(), 1);
        sqlx::query("UPDATE gc_cycles SET state = 'failed', error = 'lost' WHERE id = $1")
            .bind(&first)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();

        let second = run_cycle(&api, &repo).await;
        let later = after_pause(&ts, &second).await;
        assert!(
            !lock_files(&ts, "main").contains(&left[0]),
            "[{}] the old marker is still there",
            backend.name()
        );
        assert_eq!(
            steps(&ts, &second, later).await,
            StepOutcome::Finished(lodestore_server::maintenance::gc::GcState::Done)
        );
        assert!(lock_files(&ts, "main").is_empty());
        assert_restores(&ts, "main", &b).await;
        assert_restores(&ts, "main", &c).await;
        ts.stop().await;
    }
}

/// Another server's marker, as if its cycle had passed the preflight at the
/// same moment as ours.
async fn plant_foreign_marker(ts: &TestServer, name: &str) {
    let store = LocalFsStore::new(ts.local_repo_root(name));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let keys = repo.read_keys();
    let mut entropy = Entropy::os();
    let gc_id = new_lock_id(&mut entropy);
    publish_gc_manifest(
        &store,
        keys.k_idx(),
        keys.repo_id(),
        &gc_id,
        &[[7u8; 32]],
        &mut entropy,
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_foreign_marker_stops_marking() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let (b, _) = forgotten_a(&ts, "main").await;
        enable(&ts, &api, &repo).await;
        let before = packs_size(&ts, "main");

        let id = lodestore_server::maintenance::gc::start_cycle(
            ts.state(),
            &repo,
            false,
            lodestore_server::maintenance::gc::GcOrigin::Manual,
            "test",
        )
        .await
        .unwrap();
        plant_foreign_marker(&ts, "main").await;

        assert_eq!(
            steps(&ts, &id, lodestore_server::db::now_ms()).await,
            StepOutcome::Finished(lodestore_server::maintenance::gc::GcState::Failed),
            "[{}]",
            backend.name()
        );
        let stopped = cycle(&ts, &id).await;
        assert!(
            stopped
                .error
                .unwrap()
                .contains("Another cleanup marker (locks/gc-"),
            "[{}]",
            backend.name()
        );
        assert_eq!(packs_size(&ts, "main"), before, "nothing may be repacked");
        assert_eq!(
            lock_files(&ts, "main").len(),
            1,
            "only the foreign marker stays"
        );
        assert_restores(&ts, "main", &b).await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_cycle_foreign_marker_stops_before_sweep() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let (b, c) = forgotten_a(&ts, "main").await;
        enable(&ts, &api, &repo).await;

        let id = run_cycle(&api, &repo).await;
        let later = after_pause(&ts, &id).await;
        let marked = packs_size(&ts, "main");
        plant_foreign_marker(&ts, "main").await;

        assert_eq!(
            steps(&ts, &id, later).await,
            StepOutcome::Finished(lodestore_server::maintenance::gc::GcState::Failed),
            "[{}]",
            backend.name()
        );
        let stopped = cycle(&ts, &id).await;
        assert!(!stopped.sweep_started, "[{}]", backend.name());
        assert!(
            stopped
                .error
                .unwrap()
                .contains("Another cleanup marker (locks/gc-")
        );
        assert_eq!(packs_size(&ts, "main"), marked, "nothing may be deleted");
        assert_eq!(
            lock_files(&ts, "main").len(),
            1,
            "only the foreign marker stays"
        );
        assert_restores(&ts, "main", &b).await;
        assert_restores(&ts, "main", &c).await;
        ts.stop().await;
    }
}
