mod common;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use common::{AgentEnv, Backend, TestServer, backends};
use lodestore_format::index::parse_hex32;
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::repo::Repository;
use lodestore_format::snapshot::snapshot_object_key;
use lodestore_format::store::LocalFsStore;
use lodestore_server::maintenance::gc::cycle::{StepOutcome, step};
use lodestore_server::maintenance::gc::{GcOrigin, GcSettings, GcState, db, start_cycle};
use sqlx::Row;

const PASSWORD: &[u8] = b"test-password";
const REPO: &str = "main";
const SHARED_SIZE: usize = 150 * 1024;
const OWN_SIZE: usize = 200 * 1024;
const BACK_SIZE: usize = 120 * 1024;
const THREE_DAYS: Duration = Duration::from_secs(3 * 86_400);

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

struct Snap {
    id: String,
    dir: PathBuf,
    files: Vec<(&'static str, Vec<u8>)>,
}

/// What a host holds in round `round`: a file shared by everyone, one that
/// changes every round, and one that is gone in rounds 2 and 3 and comes back
/// with its first content, so new backups find its blobs in old packs.
fn write_round(env: &AgentEnv, host: u64, round: u64) -> (PathBuf, Vec<(&'static str, Vec<u8>)>) {
    let dir = env.work.path().join("src");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut files = vec![
        ("shared.bin", noise(1, SHARED_SIZE)),
        ("own.bin", noise(100 * host + round, OWN_SIZE)),
    ];
    if !(2..=3).contains(&round) {
        files.push(("back.bin", noise(500 + host, BACK_SIZE)));
    }
    let base = SystemTime::now() - Duration::from_secs(3600);
    for (name, body) in &files {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(base - Duration::from_secs(60 * round))
            .unwrap();
    }
    (dir.canonicalize().unwrap(), files)
}

async fn round(env: &AgentEnv, host: u64, round: u64) -> Snap {
    let (dir, files) = write_round(env, host, round);
    let id = env.backup(&[dir.as_path()]).await;
    Snap { id, dir, files }
}

fn age_packs(ts: &TestServer) {
    let old = SystemTime::now() - THREE_DAYS;
    for path in files_under(&ts.local_repo_root(REPO).join("packs")) {
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

fn forget(ts: &TestServer, snap: &Snap) {
    let id = parse_hex32(&snap.id).unwrap();
    std::fs::remove_file(ts.local_repo_root(REPO).join(snapshot_object_key(&id))).unwrap();
}

async fn assert_restores(ts: &TestServer, snap: &Snap) {
    let store = LocalFsStore::new(ts.local_repo_root(REPO));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let mut reader = Reader::open(&store, repo.read_keys().duplicate(), repo.config()).unwrap();
    let target = tempfile::tempdir().unwrap();
    let report = reader
        .restore(
            &parse_hex32(&snap.id).unwrap(),
            target.path(),
            &RestoreOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert!(
        report.errors.is_empty(),
        "snapshot {}: {:?}",
        snap.id,
        report.errors
    );
    let root = target.path().join(snap.dir.strip_prefix("/").unwrap());
    for (name, body) in &snap.files {
        let restored = std::fs::read(root.join(name))
            .unwrap_or_else(|e| panic!("{name} of snapshot {} is missing: {e}", snap.id));
        assert!(
            restored == *body,
            "{name} of snapshot {} was not restored byte for byte",
            snap.id
        );
    }
}

/// Steps the cycle on the real clock until it publishes its marker.
async fn mark(ts: &TestServer, id: &str) -> i64 {
    loop {
        match step(ts.state(), id, lodestore_server::db::now_ms())
            .await
            .unwrap()
        {
            StepOutcome::Advanced(_) => {}
            StepOutcome::Waiting { until_ms } => return until_ms,
            other => panic!("the cycle was expected to wait out the pause: {other:?}"),
        }
    }
}

/// Steps the cycle on a clock past the pause until it ends.
async fn finish(ts: &TestServer, id: &str, now: i64) -> GcState {
    loop {
        match step(ts.state(), id, now).await.unwrap() {
            StepOutcome::Advanced(_) => {}
            StepOutcome::Finished(state) => return state,
            other => panic!("the cycle stopped on {other:?}"),
        }
    }
}

async fn reconnect(ts: &mut TestServer, agents: [&mut AgentEnv; 2]) {
    ts.restart().await;
    for agent in agents {
        agent.api = ts.api().await;
    }
}

async fn chaos(backend: &Backend, restarts: bool) {
    let mut ts = TestServer::start_with(backend, |c| c.schedule_tick_secs = 0).await;
    let repo = ts.create_local_repository(REPO).await;
    let mut a = AgentEnv::start_on(&ts, &repo, REPO, "host-a").await;
    let mut b = AgentEnv::start_on(&ts, &repo, REPO, "host-b").await;

    let mut kept = Vec::new();
    let mut forgotten = Vec::new();
    for n in 1..=3 {
        let (x, y) = tokio::join!(round(&a, 0, n), round(&b, 1, n));
        for snap in [x, y] {
            if n == 1 {
                forgotten.push(snap);
            } else {
                kept.push(snap);
            }
        }
    }
    for snap in &forgotten {
        forget(&ts, snap);
    }
    age_packs(&ts);

    let settings = GcSettings {
        enabled: true,
        ..GcSettings::default()
    };
    db::save_settings(ts.state(), &repo, &settings)
        .await
        .unwrap();
    let id = start_cycle(ts.state(), &repo, false, GcOrigin::Manual, "admin")
        .await
        .unwrap();

    let (x, y, until_ms) = tokio::join!(round(&a, 0, 4), round(&b, 1, 4), mark(&ts, &id));
    kept.extend([x, y]);
    if restarts {
        reconnect(&mut ts, [&mut a, &mut b]).await;
    }

    let (x, y) = tokio::join!(round(&a, 0, 5), round(&b, 1, 5));
    kept.extend([x, y]);

    let later = until_ms + 1_000;
    if restarts {
        assert_eq!(
            step(ts.state(), &id, later).await.unwrap(),
            StepOutcome::Advanced(GcState::Verifying)
        );
        reconnect(&mut ts, [&mut a, &mut b]).await;
    }
    let (x, y, state) = tokio::join!(round(&a, 0, 6), round(&b, 1, 6), finish(&ts, &id, later));
    kept.extend([x, y]);
    assert_eq!(state, GcState::Done);
    let summary: String = sqlx::query("SELECT summary_json FROM gc_cycles WHERE id = $1")
        .bind(&id)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .get("summary_json");
    let summary: serde_json::Value = serde_json::from_str(&summary).unwrap();
    assert!(
        summary["bytes_freed"].as_u64().unwrap() > 0,
        "[{}] the cycle freed nothing: {summary}",
        backend.name()
    );

    let (x, y) = tokio::join!(round(&a, 0, 7), round(&b, 1, 7));
    kept.extend([x, y]);

    for snap in &kept {
        assert_restores(&ts, snap).await;
    }
    let failed = sqlx::query("SELECT COUNT(*) AS n FROM jobs WHERE state = 'failed'")
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .get::<i64, _>("n");
    assert_eq!(failed, 0, "[{}] a backup job failed", backend.name());

    a.stop().await;
    b.stop().await;
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_chaos_backups_during_cycle() {
    for backend in backends() {
        chaos(&backend, false).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_chaos_restart_mid_cycle() {
    for backend in backends() {
        chaos(&backend, true).await;
    }
}
