//! A writer past its deadline stops, uploads what it has and writes no snapshot.

use std::cell::Cell;
use std::fs;
use std::io::{Cursor, Read};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use lodestore_format::Error;
use lodestore_format::crypto::Entropy;
use lodestore_format::keys::KdfCost;
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::store::{BlobStore, LocalFsStore};
use lodestore_format::writer::{BackupOptions, Deadline, Writer};
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{Rng, SeedableRng};

const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};
const HOUR: Duration = Duration::from_secs(3600);

async fn init_repo(dir: &Path) -> LocalFsStore {
    let store = LocalFsStore::new(dir.to_path_buf());
    let mut entropy = Entropy::os();
    let options = InitOptions {
        kdf: LOW_KDF,
        ..Default::default()
    };
    PendingInit::generate(&mut entropy, options)
        .unwrap()
        .commit(&store, b"pw", &mut entropy)
        .await
        .unwrap();
    store
}

async fn writer(store: &LocalFsStore) -> Writer<&LocalFsStore> {
    let repo = Repository::open(store, b"pw").await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    Writer::new(store, keys, repo.config()).await.unwrap()
}

fn random_bytes(len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    ChaCha20Rng::from_seed([9; 32]).fill_bytes(&mut out);
    out
}

fn expired() -> Option<Deadline> {
    Some(Deadline::starting_now(Duration::ZERO))
}

fn after(limit: Duration) -> Option<Deadline> {
    Some(Deadline::starting_now(limit))
}

async fn count(store: &LocalFsStore, prefix: &str) -> usize {
    store.list(prefix).await.unwrap().len()
}

#[test]
fn writer_deadline_wall_clock_jump() {
    let wall = SystemTime::now() - 2 * HOUR;
    assert!(Deadline::started_at(Instant::now(), wall, HOUR).expired());
}

#[test]
fn writer_deadline_clock_back() {
    let wall = SystemTime::now() + 2 * HOUR;
    assert!(!Deadline::started_at(Instant::now(), wall, HOUR).expired());
}

#[tokio::test]
async fn writer_deadline_no_snapshot_after_expiry() {
    let tmp = tempfile::tempdir().unwrap();
    let store = init_repo(&tmp.path().join("repo")).await;
    let src = tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("random.bin"), random_bytes(2 << 20)).unwrap();

    let err = writer(&store)
        .await
        .backup(
            std::slice::from_ref(&src),
            BackupOptions {
                deadline: expired(),
                ..Default::default()
            },
            &|_| {},
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::DeadlineExceeded { limit_secs: 0 }));
    assert_eq!(count(&store, "snapshots/").await, 0);
}

#[tokio::test]
async fn writer_deadline_interrupts_walk() {
    let tmp = tempfile::tempdir().unwrap();
    let store = init_repo(&tmp.path().join("repo")).await;
    let src = tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    let total = 2usize << 20;
    fs::write(src.join("a.bin"), random_bytes(total)).unwrap();
    fs::write(src.join("b.bin"), random_bytes(total + 1)).unwrap();

    let slept = Cell::new(false);
    let started = Cell::new(0u32);
    let err = writer(&store)
        .await
        .backup(
            std::slice::from_ref(&src),
            BackupOptions {
                deadline: after(Duration::from_millis(200)),
                ..Default::default()
            },
            &|p| {
                if p.starting {
                    started.set(started.get() + 1);
                } else if p.bytes_added > 0 && !slept.replace(true) {
                    std::thread::sleep(Duration::from_millis(300));
                }
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::DeadlineExceeded { .. }), "{err:?}");
    assert_eq!(started.get(), 1, "the second file was not started");
    assert_eq!(count(&store, "snapshots/").await, 0);
    assert!(
        count(&store, "packs/").await > 0,
        "what was read is uploaded"
    );

    let again = writer(&store)
        .await
        .backup(
            std::slice::from_ref(&src),
            BackupOptions {
                deadline: after(HOUR),
                ..Default::default()
            },
            &|_| {},
        )
        .await
        .unwrap();
    assert!(
        again.stats.bytes_added < total as u64 + (1 << 20),
        "the first file was already indexed"
    );
    assert_eq!(count(&store, "snapshots/").await, 1);
}

struct Slow {
    reads: u32,
}

impl Read for Slow {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reads += 1;
        std::thread::sleep(Duration::from_millis(30));
        buf.fill(self.reads as u8);
        Ok(buf.len().min(1 << 20))
    }
}

#[tokio::test]
async fn writer_deadline_interrupts_stream() {
    let tmp = tempfile::tempdir().unwrap();
    let store = init_repo(&tmp.path().join("repo")).await;
    let mut source = Slow { reads: 0 };
    let err = writer(&store)
        .await
        .backup_stream(
            b"dump",
            &mut source,
            BackupOptions {
                deadline: after(Duration::from_millis(100)),
                ..Default::default()
            },
            &|_| {},
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::DeadlineExceeded { .. }), "{err:?}");
    assert!(
        source.reads < 50,
        "the stream was read {} times",
        source.reads
    );
    assert_eq!(count(&store, "snapshots/").await, 0);
}

#[tokio::test]
async fn writer_deadline_stream() {
    let tmp = tempfile::tempdir().unwrap();
    let store = init_repo(&tmp.path().join("repo")).await;
    let err = writer(&store)
        .await
        .backup_stream(
            b"dump",
            &mut Cursor::new(random_bytes(1 << 20)),
            BackupOptions {
                deadline: expired(),
                ..Default::default()
            },
            &|_| {},
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::DeadlineExceeded { .. }));
    assert_eq!(count(&store, "snapshots/").await, 0);
}
