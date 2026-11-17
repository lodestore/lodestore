use std::fs;
use std::io::{Cursor, Read};
use std::path::Path;

use lodestore_format::Error;
use lodestore_format::crypto::Entropy;
use lodestore_format::keys::KdfCost;
use lodestore_format::reader::Reader;
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::store::{BlobStore, LocalFsStore};
use lodestore_format::tree::MAX_NAME_LEN;
use lodestore_format::writer::{BackupOptions, Writer};
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{Rng, SeedableRng};

const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

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

async fn reader(store: &LocalFsStore) -> Reader<&LocalFsStore> {
    let repo = Repository::open(store, b"pw").await.unwrap();
    Reader::open(store, repo.read_keys().duplicate(), repo.config()).unwrap()
}

fn random_bytes(len: usize, seed: u8) -> Vec<u8> {
    let mut rng = ChaCha20Rng::from_seed([seed; 32]);
    let mut out = vec![0u8; len];
    rng.fill_bytes(&mut out);
    out
}

struct FailingReader {
    data: Cursor<Vec<u8>>,
    fail_after: usize,
    read: usize,
}

impl Read for FailingReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.read >= self.fail_after {
            return Err(std::io::Error::other("pipe broke"));
        }
        let room = (self.fail_after - self.read).min(buf.len());
        let n = self.data.read(&mut buf[..room])?;
        self.read += n;
        Ok(n)
    }
}

#[tokio::test]
async fn stream_roundtrip_restores_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir(&repo_dir).unwrap();
    let store = init_repo(&repo_dir).await;
    let data = random_bytes(3 << 20, 1);

    let mut w = writer(&store).await;
    let options = BackupOptions {
        hostname: Some(b"db1".to_vec()),
        tags: vec!["policy=nightly".into()],
        agent_id: Some(b"agent-1".to_vec()),
        ..Default::default()
    };
    let outcome = w
        .backup_stream(b"app.dump", &mut Cursor::new(&data), options, &|_| {})
        .await
        .unwrap();
    assert!(!outcome.incomplete);
    assert_eq!(outcome.stats.files_new, 1);
    assert_eq!(outcome.stats.bytes_total, data.len() as u64);

    let mut r = reader(&store).await;
    let entry = r.list_snapshots().await.unwrap().entries.remove(0);
    assert_eq!(entry.id, outcome.snapshot_id);
    let snapshot = entry.snapshot;
    assert_eq!(snapshot.paths, vec![b"/app.dump".to_vec()]);
    assert_eq!(snapshot.hostname, b"db1");
    assert_eq!(snapshot.agent_id, b"agent-1");
    assert!(snapshot.tags.contains(&"stdin=app.dump".to_owned()));
    assert!(snapshot.tags.contains(&"policy=nightly".to_owned()));

    let mut restored = Vec::new();
    r.dump_file(&outcome.snapshot_id, b"/app.dump", &mut restored)
        .await
        .unwrap();
    assert_eq!(restored, data);
}

#[tokio::test]
async fn stream_dedups_with_file_of_same_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir(&src).unwrap();
    let data = random_bytes(5 << 20, 2);
    fs::write(src.join("app.dump"), &data).unwrap();
    let store = init_repo(&repo_dir).await;

    let mut w = writer(&store).await;
    let first = w
        .backup(
            std::slice::from_ref(&src),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert!(first.stats.bytes_added >= data.len() as u64);

    let second = w
        .backup_stream(
            b"app.dump",
            &mut Cursor::new(&data),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert!(
        second.stats.bytes_added < 4096,
        "stream added {} bytes",
        second.stats.bytes_added
    );
}

#[tokio::test]
async fn stream_empty_input_is_empty_file() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir(&repo_dir).unwrap();
    let store = init_repo(&repo_dir).await;

    let mut w = writer(&store).await;
    let outcome = w
        .backup_stream(
            b"empty.dump",
            &mut std::io::empty(),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert_eq!(outcome.stats.bytes_total, 0);

    let mut r = reader(&store).await;
    let mut restored = vec![1u8];
    r.dump_file(&outcome.snapshot_id, b"/empty.dump", &mut restored)
        .await
        .unwrap();
    assert_eq!(restored, [1u8]);
}

#[tokio::test]
async fn stream_rejects_bad_names() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir(&repo_dir).unwrap();
    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;

    let too_long = vec![b'a'; MAX_NAME_LEN + 1];
    for name in [&b""[..], b"a/b", b".", b"..", b"a\0b", &too_long] {
        let err = w
            .backup_stream(
                name,
                &mut Cursor::new(b"x"),
                BackupOptions::default(),
                &|_| {},
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)), "{name:?}: {err}");
    }
    assert!(store.list("snapshots/").await.unwrap().is_empty());
}

#[tokio::test]
async fn stream_read_error_writes_no_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir(&repo_dir).unwrap();
    let store = init_repo(&repo_dir).await;
    let data = random_bytes(12 << 20, 3);

    let mut w = writer(&store).await;
    let mut broken = FailingReader {
        data: Cursor::new(data.clone()),
        fail_after: 10 << 20,
        read: 0,
    };
    let err = w
        .backup_stream(b"app.dump", &mut broken, BackupOptions::default(), &|_| {})
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Io(_)), "{err}");
    assert!(store.list("snapshots/").await.unwrap().is_empty());
    assert!(!store.list("packs/").await.unwrap().is_empty());

    let mut w = writer(&store).await;
    let retry = w
        .backup_stream(
            b"app.dump",
            &mut Cursor::new(&data),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    // The chunk still being cut when the stream broke is lost, everything
    // before it was uploaded and is not uploaded again.
    assert!(
        retry.stats.bytes_added < 6 << 20,
        "retry added {} bytes",
        retry.stats.bytes_added
    );
}
