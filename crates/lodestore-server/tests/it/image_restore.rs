use crate::common;

use std::fs;
use std::path::Path;
use std::time::SystemTime;

use common::{AgentEnv, TestServer, backends};
use lodestore_format::attest::{put_attestation, seal_provenance};
use lodestore_format::crypto::Entropy;
use lodestore_format::fs::raw::FileSource;
use lodestore_format::index::{hex32, parse_hex32};
use lodestore_format::provenance::owner_record;
use lodestore_format::repo::Repository;
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, ImageOptions, Writer};
use lodestore_proto::agent::v1::ConflictPolicy;
use lodestore_server::catalog::sync_repository;
use lodestore_server::jobs::restore::{RestoreRequest, enqueue_restore};
use sqlx::Row;

const PASSWORD: &[u8] = b"test-password";
const CHUNK: usize = 4 << 20;
const MIB: usize = 1 << 20;

/// Chunks 0, 2, 3 and the short last one are holes; the rest is data.
fn image_bytes() -> Vec<u8> {
    let mut data = vec![0u8; 6 * CHUNK + MIB];
    for chunk in [1usize, 4, 5] {
        let mut state = chunk as u64 + 1;
        for byte in &mut data[chunk * CHUNK..(chunk + 1) * CHUNK] {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *byte = (state >> 33) as u8;
        }
    }
    data
}

/// Signs `snapshot` as the repository owner does, with `source` as the claim.
async fn owner_sign(ts: &TestServer, snapshot: &str, source: Option<lodestore_format::pb::Source>) {
    let store = LocalFsStore::new(ts.local_repo_root("main"));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let record = owner_record(
        &parse_hex32(snapshot).unwrap(),
        b"owner",
        source,
        SystemTime::now(),
    );
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
}

/// Writes a disk image snapshot the way `lode backup --image` does and signs it as the owner.
async fn write_image_snapshot(ts: &TestServer, file: &Path, hostname: &str) -> String {
    let store = LocalFsStore::new(ts.local_repo_root("main"));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    let mut writer = Writer::new(&store, keys, repo.config()).await.unwrap();
    let mut source = FileSource::open(file).unwrap();
    let options = BackupOptions {
        hostname: Some(hostname.as_bytes().to_vec()),
        ..BackupOptions::default()
    };
    let done = writer
        .backup_image(
            "disk",
            &mut source,
            options,
            ImageOptions { segment_chunks: 2 },
            &|_| {},
        )
        .await
        .unwrap();
    let id = hex32(&done.outcome.snapshot_id);
    owner_sign(ts, &id, Some(done.source)).await;
    id
}

fn request(
    host: &str,
    snapshot: &str,
    items: Vec<Vec<u8>>,
    destination: &Path,
    conflict: ConflictPolicy,
) -> RestoreRequest {
    RestoreRequest {
        snapshot_id: snapshot.to_string(),
        items,
        target_host_id: host.to_string(),
        destination: destination.to_string_lossy().into_owned(),
        in_place: false,
        conflict,
        restore_owners: false,
        accept_unverified: false,
    }
}

async fn source_type(ts: &TestServer, snapshot: &str) -> String {
    sqlx::query("SELECT source_type FROM snapshots WHERE id = $1")
        .bind(snapshot)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .try_get("source_type")
        .unwrap()
}

#[cfg(target_os = "linux")]
fn assert_holes_stay_holes(path: &Path) {
    use lodestore_format::fs::walk::data_extents;
    let file = fs::File::open(path).unwrap();
    let size = file.metadata().unwrap().len();
    let extents = data_extents(&file.into(), size).expect("a sparse restore");
    let in_hole = |chunk: usize| {
        let (from, to) = (chunk * CHUNK, ((chunk + 1) * CHUNK).min(size as usize));
        extents
            .iter()
            .any(|&(o, l)| (o as usize) < to && (o + l) as usize > from)
    };
    for chunk in [0, 2, 3, 6] {
        assert!(!in_hole(chunk), "chunk {chunk} holds data");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn image_restore_e2e_through_the_server() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let ts = env.ts();
        let original = image_bytes();
        let file = env.work.path().join("source.raw");
        fs::write(&file, &original).unwrap();
        let snapshot = write_image_snapshot(ts, &file, &env.hostname).await;
        sync_repository(ts.state(), &env.repo).await.unwrap();
        assert_eq!(source_type(ts, &snapshot).await, "image");

        let out = env.work.path().join("out");
        let id = enqueue_restore(
            ts.state(),
            request(
                &env.host,
                &snapshot,
                vec![b"/".to_vec()],
                &out,
                ConflictPolicy::StopIfNotEmpty,
            ),
            "admin",
        )
        .await
        .expect("queue an image restore");
        let job = env.wait_job(&id).await;

        assert_eq!(job["state"], "succeeded", "[{}] {job}", backend.name());
        let restored = out.join("disk.img");
        assert!(
            fs::read(&restored).unwrap() == original,
            "the restored image differs"
        );
        #[cfg(target_os = "linux")]
        assert_holes_stay_holes(&restored);
        env.stop().await;
    }
}
