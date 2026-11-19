use crate::common;

use std::fs;
use std::path::Path;
use std::time::SystemTime;

use common::{AgentEnv, TestServer, backends};
use lodestore_format::attest::{put_attestation, seal_provenance};
use lodestore_format::crypto::{BlobId, Entropy};
use lodestore_format::fs::raw::FileSource;
use lodestore_format::image::{ChunkRef, ImageMeta, ReadMode, segment_nodes};
use lodestore_format::index::{hex32, parse_hex32};
use lodestore_format::pb;
use lodestore_format::provenance::owner_record;
use lodestore_format::repo::Repository;
use lodestore_format::selection::Selection;
use lodestore_format::source::{self, SourceKey};
use lodestore_format::store::LocalFsStore;
use lodestore_format::tree::encode_tree;
use lodestore_format::writer::{BackupOptions, ImageOptions, Writer};
use lodestore_proto::agent::v1::ConflictPolicy;
use lodestore_server::browse::tree::reader;
use lodestore_server::catalog::sync_repository;
use lodestore_server::download::archive::{ArchiveFormat, write_archive};
use lodestore_server::error::ApiError;
use lodestore_server::jobs::restore::{IMAGE_ENQUEUE_LIMIT, RestoreRequest, enqueue_restore};
use serde_json::{Value, json};
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

/// A snapshot of `nodes` as the segments of `/disks/disk`, which the manifest and the owner both claim as `source`.
async fn raw_image_snapshot(ts: &TestServer, nodes: Vec<pb::Node>, source: pb::Source) -> String {
    let store = LocalFsStore::new(ts.local_repo_root("main"));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    let mut writer = Writer::new(&store, keys, repo.config()).await.unwrap();
    let dir = |name: &str, tree: BlobId| pb::Node {
        name: name.as_bytes().to_vec(),
        r#type: pb::NodeType::Dir as i32,
        subtrees: vec![tree.to_vec()],
        ..Default::default()
    };
    let disk = writer.put_tree_raw(&encode_tree(nodes)).unwrap();
    let disks = writer
        .put_tree_raw(&encode_tree(vec![dir("disk", disk)]))
        .unwrap();
    let root = writer
        .put_tree_raw(&encode_tree(vec![dir("disks", disks)]))
        .unwrap();
    let id = writer
        .commit_snapshot_raw_with(root, vec![b"/".to_vec()], Some(source.clone()))
        .await
        .unwrap();
    let id = hex32(&id);
    owner_sign(ts, &id, Some(source)).await;
    id
}

fn image_claim(layout: Option<&str>, size: u64) -> pb::Source {
    let mut meta = ImageMeta {
        read_mode: ReadMode::Full,
        disk_size: size,
    }
    .to_meta("disk");
    if let Some(layout) = layout {
        meta.insert("image_layout".into(), layout.into());
    }
    source::encode(&SourceKey::image("disk").unwrap(), &meta)
        .unwrap()
        .unwrap()
}

/// Three segments of holes, `skip` of them left out of the tree.
fn hole_segments(skip: Option<usize>) -> Vec<pb::Node> {
    let size = 6 * CHUNK as u64;
    let chunks = vec![ChunkRef::Zero; 6];
    let mut nodes = segment_nodes(size, &chunks, 2).unwrap();
    if let Some(at) = skip {
        nodes.remove(at);
    }
    nodes
}

async fn jobs_queued(ts: &TestServer) -> i64 {
    sqlx::query("SELECT COUNT(*) AS n FROM jobs WHERE kind = 'restore'")
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .try_get("n")
        .unwrap()
}

async fn queue_whole(env: &AgentEnv, snapshot: &str, out: &Path) -> Result<String, ApiError> {
    enqueue_restore(
        env.ts().state(),
        request(
            &env.host,
            snapshot,
            vec![b"/".to_vec()],
            out,
            ConflictPolicy::StopIfNotEmpty,
        ),
        "admin",
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn image_restore_of_segments_works_as_files() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let ts = env.ts();
        let original = image_bytes();
        let file = env.work.path().join("source.raw");
        fs::write(&file, &original).unwrap();
        let snapshot = write_image_snapshot(ts, &file, &env.hostname).await;
        sync_repository(ts.state(), &env.repo).await.unwrap();

        let out = env.work.path().join("out");
        let id = enqueue_restore(
            ts.state(),
            request(
                &env.host,
                &snapshot,
                vec![b"/disks/disk".to_vec()],
                &out,
                ConflictPolicy::StopIfNotEmpty,
            ),
            "admin",
        )
        .await
        .expect("queue a restore of the segments");
        let job = env.wait_job(&id).await;

        assert_eq!(job["state"], "succeeded", "[{}] {job}", backend.name());
        let dir = out.join("disks/disk");
        let mut names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names.len(), 4, "{names:?}");
        let joined: Vec<u8> = names
            .iter()
            .flat_map(|name| fs::read(dir.join(name)).unwrap())
            .collect();
        assert!(joined == original, "the segments differ from the image");
        assert!(!out.join("disk.img").exists());
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn image_restore_of_a_guest_is_still_refused() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let ts = env.ts();
        let file = env.work.path().join("source.raw");
        fs::write(&file, image_bytes()).unwrap();
        let snapshot = write_image_snapshot(ts, &file, &env.hostname).await;
        sync_repository(ts.state(), &env.repo).await.unwrap();
        let guest = "proxmox-vm/pve1/11111111-1111-4111-8111-111111111111";
        sqlx::query(
            "UPDATE snapshots SET source_type = 'proxmox-vm', source_ref = $1 WHERE id = $2",
        )
        .bind(guest)
        .bind(&snapshot)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();

        for items in [vec![b"/".to_vec()], vec![b"/disks/disk".to_vec()]] {
            let err = enqueue_restore(
                ts.state(),
                request(
                    &env.host,
                    &snapshot,
                    items,
                    &env.work.path().join("out"),
                    ConflictPolicy::StopIfNotEmpty,
                ),
                "admin",
            )
            .await
            .unwrap_err();
            let ApiError::Validation(message) = err else {
                panic!("[{}] {err:?}", backend.name());
            };
            assert!(message.contains("cannot be restored as files"), "{message}");
        }
        assert_eq!(jobs_queued(ts).await, 0);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn image_restore_refuses_a_damaged_layout() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let ts = env.ts();
        let damaged = raw_image_snapshot(
            ts,
            hole_segments(Some(1)),
            image_claim(None, 6 * CHUNK as u64),
        )
        .await;
        let file = env.work.path().join("source.raw");
        fs::write(&file, image_bytes()).unwrap();
        let good = write_image_snapshot(ts, &file, &env.hostname).await;
        sync_repository(ts.state(), &env.repo).await.unwrap();
        assert_eq!(source_type(ts, &damaged).await, "image");
        let out = env.work.path().join("out");

        for _ in 0..=IMAGE_ENQUEUE_LIMIT {
            let err = queue_whole(&env, &damaged, &out).await.unwrap_err();
            assert!(
                matches!(err, ApiError::Validation(_) | ApiError::Backend(_)),
                "[{}] {err:?}",
                backend.name()
            );
        }
        assert_eq!(jobs_queued(ts).await, 0);

        let id = queue_whole(&env, &good, &out)
            .await
            .expect("the permits are released on the error path");
        let job = env.wait_job(&id).await;
        assert_eq!(job["state"], "succeeded", "{job}");
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn image_restore_refuses_a_newer_layout() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let ts = env.ts();
        let newer = raw_image_snapshot(
            ts,
            hole_segments(None),
            image_claim(Some("2"), 6 * CHUNK as u64),
        )
        .await;
        sync_repository(ts.state(), &env.repo).await.unwrap();
        assert_eq!(source_type(ts, &newer).await, "image");

        let err = queue_whole(&env, &newer, &env.work.path().join("out"))
            .await
            .unwrap_err();

        let ApiError::Validation(message) = err else {
            panic!("[{}] {err:?}", backend.name());
        };
        assert!(message.contains("newer"), "{message}");
        assert_eq!(jobs_queued(ts).await, 0);
        env.stop().await;
    }
}

/// A running agent and a signed two-chunk image snapshot, synced into the catalog.
async fn small_image(backend: &common::Backend) -> (AgentEnv, String) {
    let env = AgentEnv::start(backend).await;
    let mut data = vec![0u8; 3 * CHUNK];
    data[CHUNK..CHUNK + 4096].fill(0x5a);
    let file = env.work.path().join("small.raw");
    fs::write(&file, data).unwrap();
    let snapshot = write_image_snapshot(env.ts(), &file, &env.hostname).await;
    sync_repository(env.ts().state(), &env.repo).await.unwrap();
    (env, snapshot)
}

fn restore_body(
    env: &AgentEnv,
    snapshot: &str,
    item: &str,
    destination: &Path,
    in_place: bool,
    conflict: &str,
) -> Value {
    json!({
        "snapshot_id": snapshot,
        "items": [data_encoding::BASE64URL_NOPAD.encode(item.as_bytes())],
        "target_host_id": env.host,
        "destination": destination.to_string_lossy(),
        "in_place": in_place,
        "conflict": conflict,
        "restore_owners": false,
    })
}

async fn post_as(env: &AgentEnv, scopes: &[&str], body: &Value) -> (u16, Value) {
    let client = common::bearer_client(&env.ts().issue_token(scopes).await);
    let response = client
        .post(format!("{}/api/v1/restores", env.ts().rest_url()))
        .json(body)
        .send()
        .await
        .unwrap();
    (response.status().as_u16(), response.json().await.unwrap())
}

async fn finish(env: &AgentEnv, queued: &[Value]) {
    for body in queued {
        let id = body["job_id"].as_str().unwrap();
        let job = env.wait_job(id).await;
        assert_eq!(job["state"], "succeeded", "{job}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn image_restore_in_place_is_refused() {
    for backend in backends() {
        let (env, snapshot) = small_image(&backend).await;
        let root = Path::new("/");
        for item in ["/", "/disks/disk"] {
            let body = restore_body(&env, &snapshot, item, root, true, "skip_existing");
            let (status, answer) =
                post_as(&env, &["restore:host", "restore:in-place"], &body).await;
            assert_eq!(status, 422, "[{}] {answer}", backend.name());
            let message = answer["message"].as_str().unwrap();
            assert!(
                message.contains("A disk image cannot be restored in place"),
                "{message}"
            );
            let (status, _) = post_as(&env, &["restore:host"], &body).await;
            assert_eq!(status, 403);
        }
        assert_eq!(jobs_queued(env.ts()).await, 0);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn image_restore_overwrite_needs_the_in_place_scope() {
    for backend in backends() {
        let (env, snapshot) = small_image(&backend).await;
        let out = |name: &str| env.work.path().join(name);
        let host = ["restore:host"];
        let whole = |name: &str, conflict: &str| {
            restore_body(&env, &snapshot, "/", &out(name), false, conflict)
        };

        let (status, answer) = post_as(&env, &host, &whole("a", "overwrite_changed")).await;
        assert_eq!(status, 403, "[{}] {answer}", backend.name());
        assert_eq!(
            answer["message"],
            "The token needs the restore:in-place scope to replace an existing disk image file."
        );
        assert_eq!(jobs_queued(env.ts()).await, 0);

        let mut queued = Vec::new();
        for (name, conflict) in [("b", "stop_if_not_empty"), ("c", "skip_existing")] {
            let (status, answer) = post_as(&env, &host, &whole(name, conflict)).await;
            assert_eq!(status, 201, "{conflict}: {answer}");
            queued.push(answer);
        }
        let both = ["restore:host", "restore:in-place"];
        let (status, answer) = post_as(&env, &both, &whole("d", "overwrite_changed")).await;
        assert_eq!(status, 201, "{answer}");
        queued.push(answer);
        let segments = restore_body(
            &env,
            &snapshot,
            "/disks/disk",
            &out("e"),
            false,
            "overwrite_changed",
        );
        let (status, answer) = post_as(&env, &host, &segments).await;
        assert_eq!(status, 201, "{answer}");
        queued.push(answer);

        finish(&env, &queued).await;
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn image_restore_session_overwrite_needs_a_recent_password() {
    for backend in backends() {
        let (env, snapshot) = small_image(&backend).await;
        sqlx::query("UPDATE sessions SET authenticated_at = $1")
            .bind(lodestore_server::db::now_ms() - 16 * 60_000)
            .execute(&env.ts().state().db.pool)
            .await
            .unwrap();
        let out = env.work.path().join("out");

        let body = restore_body(&env, &snapshot, "/", &out, false, "overwrite_changed");
        let response = env.api.post("/api/v1/restores", &body).await;
        assert_eq!(response.status(), 403, "[{}]", backend.name());
        let answer: Value = response.json().await.unwrap();
        assert_eq!(answer["code"], "step_up_required");
        assert_eq!(jobs_queued(env.ts()).await, 0);

        let body = restore_body(&env, &snapshot, "/", &out, false, "stop_if_not_empty");
        let response = env.api.post("/api/v1/restores", &body).await;
        assert_eq!(response.status(), 201);
        let answer: Value = response.json().await.unwrap();
        finish(&env, &[answer]).await;
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn image_download_lists_segments() {
    for backend in backends() {
        let (env, snapshot) = small_image(&backend).await;
        let ts = env.ts();
        let mut reader = reader(ts.state(), &env.repo).await.unwrap();
        let id = parse_hex32(&snapshot).unwrap();
        let selection = Selection::from_paths([b"/".to_vec()]).unwrap();

        let handle = tokio::runtime::Handle::current();
        let names = tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            write_archive(
                &handle,
                &mut reader,
                &id,
                &selection,
                ArchiveFormat::Zip,
                &mut out,
            )
            .unwrap();
            let archive = zip::ZipArchive::new(std::io::Cursor::new(out)).unwrap();
            archive
                .file_names()
                .map(|name| name.unwrap().into_owned())
                .collect::<Vec<_>>()
        })
        .await
        .unwrap();

        for segment in ["000000", "000001"] {
            let wanted = format!("disks/disk/{segment}");
            assert!(
                names.iter().any(|n| n.ends_with(&wanted)),
                "[{}] {names:?}",
                backend.name()
            );
        }
        assert!(!names.iter().any(|n| n.ends_with(".img")), "{names:?}");
        env.stop().await;
    }
}
