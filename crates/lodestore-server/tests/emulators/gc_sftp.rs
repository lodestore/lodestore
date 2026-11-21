use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::agent_material::{PASSWORD, create, make_tree, sftp_draft};
use crate::common::{Backend, TestServer};
use lodestore_format::index::{hex32, parse_hex32};
use lodestore_format::keys::WriteKeys;
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::repo::Repository;
use lodestore_format::snapshot::snapshot_object_key;
use lodestore_format::store::{BlobDelete, RepoLocation, Store};
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_server::maintenance::gc::cycle::{StepOutcome, step};
use lodestore_server::maintenance::gc::{GcOrigin, GcState, db, run_dry_run_at, start_cycle};
use lodestore_server::repos::db::{server_access, writer_material};
use lodestore_testkit::sshd::SshdServer;

const REPO: &str = "/srv/repo/data";
const HOUR_S: i64 = 3_600;
const DAY_MS: i64 = 86_400_000;
const STALE: &str = "00000000000000000000000000000001";
const HOUR_OLD: &str = "00000000000000000000000000000002";
const FRESH: &str = "00000000000000000000000000000003";

fn wall_clock_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn noise(seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..600 * 1024)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn exec(sshd: &SshdServer, script: &str) -> String {
    let out = Command::new("docker")
        .args(["exec", sshd.container(), "sh", "-c", script])
        .output()
        .expect("docker exec");
    assert!(
        out.status.success(),
        "{script}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn plant_tmp(sshd: &SshdServer) {
    let now = wall_clock_ms() / 1000;
    for (name, age_s) in [(STALE, 25 * HOUR_S), (HOUR_OLD, HOUR_S), (FRESH, 0)] {
        exec(
            sshd,
            &format!(
                "echo x > {REPO}/tmp/{name} && touch -d @{} {REPO}/tmp/{name}",
                now - age_s
            ),
        );
    }
}

fn tmp_names(sshd: &SshdServer) -> Vec<String> {
    let mut names: Vec<String> = exec(sshd, &format!("ls {REPO}/tmp"))
        .lines()
        .map(str::to_owned)
        .collect();
    names.sort();
    names
}

async fn finish(ts: &TestServer, id: &str, mut now: i64) -> GcState {
    loop {
        match step(ts.state(), id, now).await.unwrap() {
            StepOutcome::Advanced(_) => {}
            StepOutcome::Waiting { until_ms } => now = until_ms + 1_000,
            StepOutcome::Postponed => panic!("the cycle is postponed"),
            StepOutcome::Finished(state) => return state,
        }
    }
}

async fn failure(ts: &TestServer, id: &str) -> Option<String> {
    db::load_cycle(ts.state(), id).await.unwrap().unwrap().error
}

async fn backup(ts: &TestServer, repo: &str, src: &Path) -> lodestore_format::crypto::BlobId {
    let material = writer_material(ts.state(), repo).await.unwrap();
    let store = RepoLocation {
        backend: material.location.backend.clone(),
        options: material.location.options.clone(),
    }
    .open(None)
    .unwrap();
    let opened = Repository::open(&store, PASSWORD.as_bytes()).await.unwrap();
    let keys = WriteKeys::from_file_text(&material.writer_keys_text).unwrap();
    let mut writer = Writer::new(&store, keys, opened.config()).await.unwrap();
    let outcome = writer
        .backup(&[src.to_path_buf()], BackupOptions::default(), &|_| {})
        .await
        .unwrap();
    assert!(!outcome.incomplete);
    outcome.snapshot_id
}

async fn server_store(ts: &TestServer, repo: &str) -> Store {
    server_access(ts.state(), repo)
        .await
        .unwrap()
        .location
        .open(None)
        .unwrap()
}

async fn assert_restores(store: &Store, snapshot: &lodestore_format::crypto::BlobId, src: &Path) {
    let opened = Repository::open(store, PASSWORD.as_bytes()).await.unwrap();
    let mut reader = Reader::open(store, opened.read_keys().duplicate(), opened.config()).unwrap();
    let out = tempfile::tempdir().unwrap();
    let report = reader
        .restore(snapshot, out.path(), &RestoreOptions::default(), &|_| {})
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let restored = out
        .path()
        .join(src.canonicalize().unwrap().strip_prefix("/").unwrap());
    for file in ["big.bin", "sub/small.txt"] {
        assert!(
            std::fs::read(restored.join(file)).unwrap() == std::fs::read(src.join(file)).unwrap(),
            "{file} was not restored byte for byte"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn gc_sftp_removes_only_stale_tmp() {
    let sshd = SshdServer::start().await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;
    let repo = create(&api, &sftp_draft(&sshd, "sftp-repo")).await;
    let src = tempfile::tempdir().unwrap();
    make_tree(src.path());
    let snapshot = backup(&ts, &repo, src.path()).await;
    plant_tmp(&sshd);
    let all = [FRESH.to_string(), HOUR_OLD.to_string(), STALE.to_string()];
    assert!(tmp_names(&sshd).len() >= 3);

    let dry = start_cycle(ts.state(), &repo, true, GcOrigin::Manual, "test")
        .await
        .unwrap();
    run_dry_run_at(ts.state(), &dry, wall_clock_ms()).await;
    assert!(
        failure(&ts, &dry).await.is_none(),
        "{:?}",
        failure(&ts, &dry).await
    );
    let names = tmp_names(&sshd);
    assert!(
        all.iter().all(|n| names.contains(n)),
        "dry run removed a tmp entry: {names:?}"
    );

    let id = start_cycle(ts.state(), &repo, false, GcOrigin::Manual, "test")
        .await
        .unwrap();
    assert_eq!(
        finish(&ts, &id, wall_clock_ms()).await,
        GcState::Done,
        "{:?}",
        failure(&ts, &id).await
    );
    let names = tmp_names(&sshd);
    assert!(!names.contains(&STALE.to_string()), "{names:?}");
    assert!(names.contains(&HOUR_OLD.to_string()), "{names:?}");
    assert!(names.contains(&FRESH.to_string()), "{names:?}");

    assert_restores(&server_store(&ts, &repo).await, &snapshot, src.path()).await;
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn gc_sftp_backup_during_sweep_restores() {
    let sshd = SshdServer::start().await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;
    let repo = create(&api, &sftp_draft(&sshd, "sftp-repo")).await;
    let server = server_store(&ts, &repo).await;

    let src_a = tempfile::tempdir().unwrap();
    make_tree(src_a.path());
    std::fs::write(src_a.path().join("only-a.bin"), noise(11)).unwrap();
    let src_b = tempfile::tempdir().unwrap();
    make_tree(src_b.path());
    std::fs::write(src_b.path().join("only-b.bin"), noise(12)).unwrap();
    let a = backup(&ts, &repo, src_a.path()).await;
    let b = backup(&ts, &repo, src_b.path()).await;
    server
        .delete(&snapshot_object_key(&parse_hex32(&hex32(&a)).unwrap()))
        .await
        .unwrap();
    plant_tmp(&sshd);

    let t0 = wall_clock_ms() + 3 * DAY_MS;
    let id = start_cycle(ts.state(), &repo, false, GcOrigin::Manual, "test")
        .await
        .unwrap();
    loop {
        match step(ts.state(), &id, t0).await.unwrap() {
            StepOutcome::Advanced(_) => {}
            StepOutcome::Waiting { .. } => break,
            other => panic!("the cycle must wait for its pause, not {other:?}"),
        }
    }

    let src_c = tempfile::tempdir().unwrap();
    make_tree(src_c.path());
    std::fs::write(src_c.path().join("only-c.bin"), noise(13)).unwrap();
    let c = backup(&ts, &repo, src_c.path()).await;

    assert_eq!(
        finish(&ts, &id, t0).await,
        GcState::Done,
        "{:?}",
        failure(&ts, &id).await
    );
    assert!(!tmp_names(&sshd).contains(&STALE.to_string()));
    assert_restores(&server, &b, src_b.path()).await;
    assert_restores(&server, &c, src_c.path()).await;
    ts.stop().await;
}
