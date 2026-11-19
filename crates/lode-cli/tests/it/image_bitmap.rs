use crate::common::{PASSWORD, cache_dir_of, init_repo, lode, pseudo_random};
use crate::planted::{attacker_keys, plant_blobs};

use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use lodestore_format::attest::{ensure_upgraded, read_attestations};
use lodestore_format::chunker::blob_id;
use lodestore_format::crypto::{BLOB_TYPE_DATA, Entropy, RepoId};
use lodestore_format::image::{BitmapNames, ImageState};
use lodestore_format::index::parse_hex32;
use lodestore_format::pb;
use lodestore_format::provenance::ProvenanceSet;
use lodestore_format::repo::Repository;
use lodestore_format::source::SourceKey;
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_nbd::{Limits, MemExport, serve};
use serde_json::Value;

const CHUNK: usize = 4 << 20;
const GRANULE: u64 = 64 << 10;
const HOST: &str = "bitmap-host";
const EXPORT: &str = "disk";

struct Shared {
    data: Vec<u8>,
    bitmaps: Vec<(String, Vec<Range<u64>>)>,
    exported: Vec<String>,
    refuse_outside_dirty: bool,
    fail_reads_after: Option<u64>,
    watch: Option<PathBuf>,
    started: usize,
    finished: usize,
    state_files_seen: Vec<usize>,
    reads_outside_dirty: u64,
    bytes_read: u64,
}

/// A read-only NBD export on a unix socket, served from memory, whose bitmaps
/// the test rotates the way a backup script would.
struct TestExport {
    shared: Arc<Mutex<Shared>>,
    _dir: tempfile::TempDir,
    socket: PathBuf,
}

fn state_files(cache: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(repos) = fs::read_dir(cache.join("lode")) else {
        return found;
    };
    for repo in repos.flatten() {
        let Ok(images) = fs::read_dir(repo.path().join("images")) else {
            continue;
        };
        found.extend(
            images
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "json")),
        );
    }
    found
}

impl TestExport {
    fn start(data: Vec<u8>) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("lode")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = dir.path().join("s");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let shared = Arc::new(Mutex::new(Shared {
            data,
            bitmaps: Vec::new(),
            exported: Vec::new(),
            refuse_outside_dirty: false,
            fail_reads_after: None,
            watch: None,
            started: 0,
            finished: 0,
            state_files_seen: Vec::new(),
            reads_outside_dirty: 0,
            bytes_read: 0,
        }));
        let state = Arc::clone(&shared);
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::UnixListener::from_std(listener).unwrap();
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let mut export = {
                        let mut s = state.lock().unwrap();
                        s.started += 1;
                        let seen = s
                            .watch
                            .as_deref()
                            .map_or(0, |cache| state_files(cache).len());
                        s.state_files_seen.push(seen);
                        let mut export = MemExport::new(s.data.clone())
                            .with_name(EXPORT)
                            .dirty_only(s.refuse_outside_dirty);
                        for name in &s.exported {
                            let ranges = s
                                .bitmaps
                                .iter()
                                .find(|(n, _)| n == name)
                                .map(|(_, r)| r.clone())
                                .unwrap();
                            export = export.with_bitmap(name, ranges);
                        }
                        if let Some(k) = s.fail_reads_after {
                            export = export.fail_reads_after(k);
                        }
                        export
                    };
                    let _ = serve(stream, &mut export, &Limits::default()).await;
                    let mut s = state.lock().unwrap();
                    s.reads_outside_dirty = export.reads_outside_dirty();
                    s.bytes_read = export.bytes_read();
                    s.finished += 1;
                }
            });
        });
        Self {
            shared,
            _dir: dir,
            socket,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap()
    }

    fn uri(&self) -> String {
        format!("nbd+unix:///{EXPORT}?socket={}", self.socket.display())
    }

    fn write(&self, offset: u64, bytes: &[u8]) {
        let mut s = self.lock();
        let start = usize::try_from(offset).unwrap();
        s.data[start..start + bytes.len()].copy_from_slice(bytes);
        let from = offset / GRANULE * GRANULE;
        let to = (offset + bytes.len() as u64).div_ceil(GRANULE) * GRANULE;
        for (_, ranges) in &mut s.bitmaps {
            ranges.push(from..to);
        }
    }

    fn add_bitmap(&self, name: &str) {
        self.lock().bitmaps.push((name.to_owned(), Vec::new()));
    }

    fn remove_bitmap(&self, name: &str) {
        let mut s = self.lock();
        s.bitmaps.retain(|(n, _)| n != name);
        s.exported.retain(|n| n != name);
    }

    fn export_bitmaps(&self, names: &[&str]) {
        let mut s = self.lock();
        for name in names {
            assert!(
                s.bitmaps.iter().any(|(n, _)| n == name),
                "{name} was not created"
            );
        }
        s.exported = names.iter().map(|n| (*n).to_owned()).collect();
    }

    fn refuse_outside_dirty(&self, on: bool) {
        self.lock().refuse_outside_dirty = on;
    }

    fn fail_reads_after(&self, k: Option<u64>) {
        self.lock().fail_reads_after = k;
    }

    fn watch_state(&self, cache: &Path) {
        self.lock().watch = Some(cache.to_owned());
    }

    fn data(&self) -> Vec<u8> {
        self.lock().data.clone()
    }

    fn resize(&self, len: usize) {
        let mut s = self.lock();
        let old = s.data.len();
        if len > old {
            s.data.extend(pseudo_random(len - old));
        } else {
            s.data.truncate(len);
        }
    }

    fn settled(&self) -> MutexGuard<'_, Shared> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let s = self.lock();
            if s.started == s.finished || Instant::now() > deadline {
                return s;
            }
            drop(s);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn connections(&self) -> usize {
        self.settled().started
    }

    fn reads_outside_dirty(&self) -> u64 {
        self.settled().reads_outside_dirty
    }

    fn bytes_read(&self) -> u64 {
        self.settled().bytes_read
    }

    fn state_files_seen(&self) -> Vec<usize> {
        self.settled().state_files_seen.clone()
    }
}

fn disk() -> Vec<u8> {
    let mut data = vec![0u8; 10 * CHUNK];
    for chunk in [0usize, 2, 3, 5, 6, 9] {
        let part: Vec<u8> = pseudo_random(200 << 10)
            .iter()
            .map(|b| b ^ chunk as u8)
            .collect();
        let start = chunk * CHUNK + 4096;
        data[start..start + part.len()].copy_from_slice(&part);
    }
    data
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

fn repo_id_of(repo: &Path) -> RepoId {
    block_on(async {
        let store = LocalFsStore::new(repo.to_path_buf());
        let repository = Repository::open(&store, PASSWORD.as_bytes()).await.unwrap();
        *repository.read_keys().repo_id()
    })
}

fn names_of(repo: &Path, host: &str, image: &str) -> BitmapNames {
    BitmapNames::new(
        &repo_id_of(repo),
        host.as_bytes(),
        &SourceKey::image(image).unwrap(),
    )
}

struct Env {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    names: BitmapNames,
    export: TestExport,
}

fn setup() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    Env {
        names: names_of(&repo, HOST, EXPORT),
        export: TestExport::start(disk()),
        repo,
        _tmp: tmp,
    }
}

impl Env {
    fn bitmap(&self, generation: u64) -> String {
        self.names.name(generation)
    }

    fn cache(&self) -> PathBuf {
        cache_dir_of(&self.repo)
    }

    fn backup(&self, uri: &str, extra: &[&str]) -> Command {
        let mut cmd = lode(&self.repo);
        cmd.args(["backup", "--json", "--host", HOST, "--image", uri])
            .args(extra);
        cmd
    }

    fn run(&self, extra: &[&str]) -> Value {
        self.run_at(&self.export.uri(), extra)
    }

    fn run_at(&self, uri: &str, extra: &[&str]) -> Value {
        summary_of(self.backup(uri, extra))
    }

    fn run_in_cache(&self, cache: &Path, extra: &[&str]) -> Value {
        let mut cmd = self.backup(&self.export.uri(), extra);
        cmd.env("XDG_CACHE_HOME", cache);
        summary_of(cmd)
    }

    fn state_file(&self) -> PathBuf {
        state_files(&self.cache()).pop().expect("a state file")
    }

    fn rewrite_state(&self, change: impl FnOnce(&mut ImageState)) {
        let path = self.state_file();
        let key = SourceKey::image(EXPORT).unwrap();
        let repo_id = repo_id_of(&self.repo);
        let mut state =
            ImageState::parse(&fs::read(&path).unwrap(), &repo_id, HOST.as_bytes(), &key).unwrap();
        change(&mut state);
        fs::write(path, state.encode(&repo_id, HOST.as_bytes(), &key)).unwrap();
    }

    fn assert_restores(&self, summary: &Value) {
        assert!(
            self.restored(summary) == self.export.data(),
            "restore differs from the disk"
        );
    }

    fn run_bitmap(&self, generation: u64) -> Value {
        let name = self.bitmap(generation);
        self.run(&["--bitmap", &name])
    }

    fn restored(&self, summary: &Value) -> Vec<u8> {
        let tmp = tempfile::tempdir().unwrap();
        lode(&self.repo)
            .args([
                "restore",
                summary["snapshot_id"].as_str().unwrap(),
                "--target",
            ])
            .arg(tmp.path())
            .assert()
            .success();
        fs::read(tmp.path().join("disk.img")).unwrap()
    }

    fn state_bytes(&self) -> Vec<Vec<u8>> {
        state_files(&self.cache())
            .iter()
            .map(|p| fs::read(p).unwrap())
            .collect()
    }

    fn lock_file(&self) -> PathBuf {
        let state = state_files(&self.cache()).pop().expect("a state file");
        state.with_extension("lock")
    }
}

fn summary_of(mut cmd: Command) -> Value {
    let out = cmd.assert().success().get_output().stdout.clone();
    String::from_utf8(out)
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["type"] == "backup_summary")
        .expect("a backup_summary line")
}

fn read_mode(summary: &Value) -> (&str, Option<&str>) {
    (
        summary["image"]["read_mode"].as_str().unwrap(),
        summary["image"]["full_reason"].as_str(),
    )
}

#[test]
fn image_bitmap_incremental_after_a_full_read() {
    let env = setup();
    let export = &env.export;
    let first = env.run(&[]);
    assert_eq!(read_mode(&first), ("full", Some("no_bitmap")));
    assert_eq!(first["image"]["key"], "image/disk");
    assert_eq!(first["image"]["next_bitmap"], env.bitmap(1));
    assert_eq!(first["image"]["bitmap_to_create"], env.bitmap(2));
    assert!(first["image"].get("bitmap").is_none());
    assert!(env.state_bytes().is_empty());

    for g in [1, 2] {
        export.add_bitmap(&env.bitmap(g));
    }
    export.export_bitmaps(&[&env.bitmap(1), &env.bitmap(2)]);
    let second = env.run_bitmap(1);
    assert_eq!(read_mode(&second), ("full", Some("no_state")));
    assert_eq!(second["image"]["bitmap"], env.bitmap(1));
    assert_eq!(second["image"]["next_bitmap"], env.bitmap(2));
    assert_eq!(second["image"]["bitmap_to_create"], env.bitmap(3));
    assert_eq!(env.state_bytes().len(), 1);

    export.remove_bitmap(&env.bitmap(1));
    export.write(2 * CHUNK as u64 + 8192, &[0xA5; 4096]);
    let across = 6 * CHUNK as u64 - (512 << 10);
    export.write(across, &pseudo_random(1 << 20));
    export.add_bitmap(&env.bitmap(3));
    export.export_bitmaps(&[&env.bitmap(2), &env.bitmap(3)]);
    export.refuse_outside_dirty(true);
    let third = env.run_bitmap(2);
    assert_eq!(read_mode(&third), ("incremental", None));
    assert_eq!(third["parent"], second["snapshot_id"]);
    assert_eq!(third["image"]["next_bitmap"], env.bitmap(3));
    assert_eq!(third["image"]["bitmap_to_create"], env.bitmap(4));
    let dirty = GRANULE + (1 << 20);
    assert!(
        third["stats"]["bytes_read"].as_u64().unwrap() <= dirty,
        "{third}"
    );
    assert!(export.bytes_read() <= dirty);
    assert_eq!(export.reads_outside_dirty(), 0);
    assert_eq!(env.restored(&third), export.data());
}

#[test]
fn image_bitmap_wrong_name_is_refused() {
    let env = setup();
    let export = &env.export;
    for g in [1, 2] {
        export.add_bitmap(&env.bitmap(g));
    }
    export.export_bitmaps(&[&env.bitmap(1), &env.bitmap(2)]);
    env.run_bitmap(1);
    let state = env.state_bytes();
    let connections = export.connections();
    let prefix = env.names.name(0).strip_suffix('0').unwrap().to_owned();

    let other = |host: &str, image: &str| names_of(&env.repo, host, image).name(1);
    let parts: Vec<String> = env.bitmap(1).split('-').map(str::to_owned).collect();
    let foreign_repo = format!("lode-{}-{}-g1", "0".repeat(16), parts[2]);
    let cases = [
        foreign_repo,
        other("another-host", EXPORT),
        other(HOST, "another-image"),
        env.bitmap(0),
        "garbage".to_owned(),
    ];
    for name in &cases {
        let assert = env
            .backup(&export.uri(), &["--bitmap", name])
            .assert()
            .code(2);
        let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
        assert!(stderr.contains(&prefix), "{name}: {stderr}");
    }
    assert_eq!(export.connections(), connections);
    assert_eq!(env.state_bytes(), state);
}

#[test]
fn image_bitmap_flag_rules() {
    let env = setup();
    let export = &env.export;
    let file = env._tmp.path().join("file.raw");
    fs::write(&file, vec![1u8; 4096]).unwrap();
    let name = env.bitmap(1);
    lode(&env.repo)
        .args(["backup", "--image"])
        .arg(&file)
        .args(["--bitmap", &name])
        .assert()
        .code(2);
    env.backup(
        &export.uri(),
        &["--bitmap", &name, "--writer-keys", "/nonexistent/keys"],
    )
    .assert()
    .code(2);
    assert_eq!(export.connections(), 0);

    let nameless = format!("nbd+unix:///?socket={}", export.socket.display());
    env.backup(&nameless, &[]).assert().code(2);
    assert_eq!(export.connections(), 0);

    export.add_bitmap(&name);
    export.export_bitmaps(&[&name]);
    let summary = env.run(&["--bitmap", &name]);
    assert_eq!(summary["image"]["key"], "image/disk");
}

#[test]
fn image_bitmap_changed_export_needs_a_name() {
    let env = setup();
    let a = &env.export;
    let b = TestExport::start(disk());
    for g in [1, 2] {
        a.add_bitmap(&env.bitmap(g));
    }
    a.export_bitmaps(&[&env.bitmap(1), &env.bitmap(2)]);
    env.run_bitmap(1);
    let state = env.state_bytes();
    assert_eq!(state.len(), 1);

    let assert = env
        .backup(&b.uri(), &["--bitmap", &env.bitmap(2)])
        .assert()
        .code(2);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("--name"), "{stderr}");
    assert_eq!(env.state_bytes(), state);
    assert_eq!(b.connections(), 0);

    a.add_bitmap(&env.bitmap(3));
    a.export_bitmaps(&[&env.bitmap(2), &env.bitmap(3)]);
    let again = env.run_bitmap(2);
    assert_eq!(read_mode(&again), ("incremental", None));
}

#[test]
fn image_bitmap_state_is_consumed_before_the_export_is_opened() {
    let env = setup();
    let export = &env.export;
    export.watch_state(&env.cache());
    for g in [1, 2, 3] {
        export.add_bitmap(&env.bitmap(g));
    }
    export.export_bitmaps(&[&env.bitmap(1), &env.bitmap(2)]);
    env.run_bitmap(1);
    assert_eq!(env.state_bytes().len(), 1);
    export.export_bitmaps(&[&env.bitmap(2), &env.bitmap(3)]);
    let second = env.run_bitmap(2);
    assert_eq!(read_mode(&second), ("incremental", None));
    assert_eq!(env.state_bytes().len(), 1);
    assert_eq!(export.state_files_seen(), vec![0, 0]);

    let missing = format!("nbd+unix:///{EXPORT}?socket=/tmp/lode-no-such-dir/s");
    env.backup(&missing, &["--name", EXPORT, "--bitmap", &env.bitmap(3)])
        .assert()
        .failure();
    assert!(env.state_bytes().is_empty());
    assert_eq!(export.connections(), 2);
}

#[test]
fn image_bitmap_second_run_of_an_instance_is_refused() {
    let env = setup();
    let export = &env.export;
    for g in [1, 2] {
        export.add_bitmap(&env.bitmap(g));
    }
    export.export_bitmaps(&[&env.bitmap(1), &env.bitmap(2)]);
    env.run_bitmap(1);
    let state = env.state_bytes();
    let connections = export.connections();

    let held = fs::File::open(env.lock_file()).unwrap();
    held.try_lock().unwrap();
    let assert = env
        .backup(&export.uri(), &["--bitmap", &env.bitmap(2)])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("another backup"), "{stderr}");
    assert_eq!(env.state_bytes(), state);
    assert_eq!(export.connections(), connections);
}

#[test]
fn image_bitmap_next_bitmap_not_exported_records_nothing() {
    let env = setup();
    let export = &env.export;
    for g in [1, 2, 3, 4] {
        export.add_bitmap(&env.bitmap(g));
    }
    export.export_bitmaps(&[&env.bitmap(1), &env.bitmap(2)]);
    env.run_bitmap(1);

    export.export_bitmaps(&[&env.bitmap(2)]);
    let name = env.bitmap(2);
    let output = env
        .backup(&export.uri(), &["--bitmap", &name])
        .assert()
        .success();
    let stderr = String::from_utf8(output.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("note:") && stderr.contains(&env.bitmap(3)),
        "{stderr}"
    );
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let summary: Value = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["type"] == "backup_summary")
        .unwrap();
    assert_eq!(read_mode(&summary), ("incremental", None));
    assert_eq!(summary["image"]["next_bitmap"], env.bitmap(3));
    assert_eq!(summary["image"]["bitmap_to_create"], env.bitmap(4));
    assert!(env.state_bytes().is_empty());

    export.export_bitmaps(&[&env.bitmap(3), &env.bitmap(4)]);
    let next = env.run_bitmap(3);
    assert_eq!(read_mode(&next), ("full", Some("no_state")));
    assert_eq!(next["image"]["next_bitmap"], env.bitmap(4));
    assert_eq!(env.state_bytes().len(), 1);
}

#[test]
fn image_bitmap_missing_context_restarts_the_chain() {
    let env = setup();
    let export = &env.export;
    for g in [4, 5] {
        export.add_bitmap(&env.bitmap(g));
    }
    export.export_bitmaps(&[&env.bitmap(4), &env.bitmap(5)]);
    let chain = env.run_bitmap(4);
    assert_eq!(chain["image"]["next_bitmap"], env.bitmap(5));
    assert_eq!(env.state_bytes().len(), 1);

    export.export_bitmaps(&[]);
    let lost = env.run_bitmap(5);
    assert_eq!(read_mode(&lost), ("full", Some("bitmap_missing")));
    assert!(env.state_bytes().is_empty());
    assert_eq!(lost["image"]["next_bitmap"], env.bitmap(6));
    assert_eq!(lost["image"]["bitmap_to_create"], env.bitmap(7));

    for g in [6, 7, 8] {
        export.add_bitmap(&env.bitmap(g));
    }
    export.export_bitmaps(&[&env.bitmap(6), &env.bitmap(7)]);
    let restart = env.run_bitmap(6);
    assert_eq!(read_mode(&restart), ("full", Some("no_state")));
    export.export_bitmaps(&[&env.bitmap(7), &env.bitmap(8)]);
    let again = env.run_bitmap(7);
    assert_eq!(read_mode(&again), ("incremental", None));
    assert_eq!(env.restored(&again), export.data());
}

#[test]
fn image_bitmap_aborted_run_forces_a_full_read() {
    let env = setup();
    let export = &env.export;
    for g in [1, 2] {
        export.add_bitmap(&env.bitmap(g));
    }
    export.export_bitmaps(&[&env.bitmap(1), &env.bitmap(2)]);
    env.run_bitmap(1);
    assert_eq!(env.state_bytes().len(), 1);

    export.write(2 * CHUNK as u64 + 8192, &[0x5A; 4096]);
    export.write(6 * CHUNK as u64 + 8192, &[0xC3; 4096]);
    export.add_bitmap(&env.bitmap(3));
    export.export_bitmaps(&[&env.bitmap(2), &env.bitmap(3)]);
    export.fail_reads_after(Some(1));
    let name = env.bitmap(2);
    env.backup(&export.uri(), &["--bitmap", &name])
        .assert()
        .failure();
    assert!(env.state_bytes().is_empty());

    export.fail_reads_after(None);
    let third = env.run_bitmap(2);
    assert_eq!(read_mode(&third), ("full", Some("no_state")));
    assert_eq!(env.restored(&third), export.data());
}

fn signed_snapshots(repo: &Path) -> ProvenanceSet {
    block_on(async {
        let store = LocalFsStore::new(repo.to_path_buf());
        let repository = Repository::open(&store, PASSWORD.as_bytes()).await.unwrap();
        let set = read_attestations(&store, repository.read_keys())
            .await
            .unwrap();
        ProvenanceSet::from_attested(&set, repository.read_keys())
    })
}

fn plant_then_upgrade(repo: &Path, content: &[u8]) {
    block_on(async {
        let store = LocalFsStore::new(repo.to_path_buf());
        let repository = Repository::open(&store, PASSWORD.as_bytes()).await.unwrap();
        let keys = attacker_keys(&repository);
        let id = blob_id(&keys.k_id, content);
        plant_blobs(&store, &keys, &[(id, BLOB_TYPE_DATA)]).await;
        ensure_upgraded(&store, repository.read_keys(), &mut Entropy::os())
            .await
            .unwrap();
    });
}

#[test]
fn image_bitmap_rejected_snapshot_forces_a_full_read() {
    let env = setup();
    let export = &env.export;
    let lie = pseudo_random(CHUNK);
    plant_then_upgrade(&env.repo, &lie);
    for g in [1, 2, 3, 4] {
        export.add_bitmap(&env.bitmap(g));
    }
    export.export_bitmaps(&[&env.bitmap(1), &env.bitmap(2)]);
    env.run(&[]);
    env.run_bitmap(1);
    export.export_bitmaps(&[&env.bitmap(2), &env.bitmap(3)]);
    let incremental = env.run_bitmap(2);
    assert_eq!(read_mode(&incremental), ("incremental", None));
    assert_eq!(signed_snapshots(&env.repo).len(), 3);
    assert_eq!(env.state_bytes().len(), 1);

    export.write(3 * CHUNK as u64, &lie);
    export.export_bitmaps(&[&env.bitmap(3), &env.bitmap(4)]);
    let name = env.bitmap(3);
    let failed = env
        .backup(&export.uri(), &["--bitmap", &name])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&failed.get_output().stderr).to_string();
    assert!(stderr.contains("does not check out"), "{stderr}");
    assert_eq!(signed_snapshots(&env.repo).len(), 3);
    assert!(env.state_bytes().is_empty());

    let retry = env.run_bitmap(3);
    assert_eq!(read_mode(&retry), ("full", Some("no_state")));
    let signed = signed_snapshots(&env.repo);
    assert_eq!(signed.len(), 4);
    assert!(signed.contains(&parse_hex32(retry["snapshot_id"].as_str().unwrap()).unwrap()));
    env.assert_restores(&retry);
}

#[test]
fn image_bitmap_lost_cache_reads_in_full_and_keeps_dedup() {
    let env = setup();
    let export = &env.export;
    for g in [1, 2, 3, 4] {
        export.add_bitmap(&env.bitmap(g));
    }
    export.export_bitmaps(&[&env.bitmap(1), &env.bitmap(2)]);
    env.run(&[]);
    env.run_bitmap(1);
    export.export_bitmaps(&[&env.bitmap(2), &env.bitmap(3)]);
    let before = env.run_bitmap(2);
    assert_eq!(read_mode(&before), ("incremental", None));
    assert_eq!(env.state_bytes().len(), 1);

    fs::remove_dir_all(env.cache()).unwrap();
    export.export_bitmaps(&[&env.bitmap(3), &env.bitmap(4)]);
    let after = env.run_bitmap(3);
    assert_eq!(read_mode(&after), ("full", Some("no_state")));
    assert_eq!(after["stats"]["bytes_added"], 0, "{after}");
    env.assert_restores(&after);
}

#[test]
fn image_bitmap_newer_snapshot_forces_a_full_read() {
    let env = setup();
    let export = &env.export;
    for g in [1, 2, 3] {
        export.add_bitmap(&env.bitmap(g));
    }
    export.export_bitmaps(&[&env.bitmap(1), &env.bitmap(2)]);
    env.run(&[]);
    env.run_bitmap(1);

    let other_machine = tempfile::tempdir().unwrap();
    let name = env.bitmap(1);
    let foreign = env.run_in_cache(other_machine.path(), &["--bitmap", &name]);
    assert_eq!(read_mode(&foreign), ("full", Some("no_state")));

    export.export_bitmaps(&[&env.bitmap(2), &env.bitmap(3)]);
    let ours = env.run_bitmap(2);
    assert_eq!(read_mode(&ours), ("full", Some("parent_not_newest")));
    env.assert_restores(&ours);
}

fn plant_snapshot_with_broken_meta(env: &Env, claim: &str) {
    let src = env._tmp.path().join("planted");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("f"), b"planted").unwrap();
    block_on(async {
        let store = LocalFsStore::new(env.repo.clone());
        let repository = Repository::open(&store, PASSWORD.as_bytes()).await.unwrap();
        let keys = repository.read_keys().write_keys(repository.config_hash());
        let mut writer = Writer::new(&store, keys, repository.config())
            .await
            .unwrap();
        let options = BackupOptions {
            hostname: Some(HOST.as_bytes().to_vec()),
            source: Some(pb::Source {
                key: claim.to_owned(),
                meta: vec![pb::SourceMeta {
                    key: "Bad".to_owned(),
                    value: "x".to_owned(),
                }],
            }),
            ..Default::default()
        };
        writer.backup(&[src], options, &|_| {}).await.unwrap();
    });
}

#[test]
fn image_bitmap_same_host_snapshot_with_broken_meta_forces_a_full_read() {
    let env = setup();
    let export = &env.export;
    for g in [1, 2, 3, 4] {
        export.add_bitmap(&env.bitmap(g));
    }
    export.export_bitmaps(&[&env.bitmap(1), &env.bitmap(2)]);
    env.run(&[]);
    env.run_bitmap(1);

    plant_snapshot_with_broken_meta(&env, "image/other");
    export.export_bitmaps(&[&env.bitmap(2), &env.bitmap(3)]);
    let unrelated = env.run_bitmap(2);
    assert_eq!(read_mode(&unrelated), ("incremental", None));

    plant_snapshot_with_broken_meta(&env, "image/disk");
    export.export_bitmaps(&[&env.bitmap(3), &env.bitmap(4)]);
    let ours = env.run_bitmap(3);
    assert_eq!(read_mode(&ours), ("full", Some("parent_not_newest")));
    env.assert_restores(&ours);
}

#[test]
fn image_bitmap_resized_disk_reads_in_full() {
    let env = setup();
    let export = &env.export;
    for g in [1, 2, 3] {
        export.add_bitmap(&env.bitmap(g));
    }
    export.export_bitmaps(&[&env.bitmap(1), &env.bitmap(2)]);
    env.run(&[]);
    env.run_bitmap(1);
    let size = export.data().len();

    export.resize(size + (1 << 20));
    export.export_bitmaps(&[&env.bitmap(2), &env.bitmap(3)]);
    let grown = env.run_bitmap(2);
    assert_eq!(read_mode(&grown), ("full", Some("size_changed")));
    env.assert_restores(&grown);

    export.add_bitmap(&env.bitmap(4));
    export.resize(size - (1 << 20));
    export.export_bitmaps(&[&env.bitmap(3), &env.bitmap(4)]);
    let shrunk = env.run_bitmap(3);
    assert_eq!(read_mode(&shrunk), ("full", Some("size_changed")));
    env.assert_restores(&shrunk);

    export.remove_bitmap(&env.bitmap(4));
    export.resize(size - (1 << 20) - CHUNK);
    export.resize(size - (1 << 20));
    for g in [5, 6] {
        export.add_bitmap(&env.bitmap(g));
    }
    export.export_bitmaps(&[&env.bitmap(5), &env.bitmap(6)]);
    let recreated = env.run_bitmap(5);
    assert_eq!(read_mode(&recreated), ("full", Some("gen_mismatch")));
    env.assert_restores(&recreated);

    export.add_bitmap(&env.bitmap(7));
    export.write(8192, &[0x3C; 4096]);
    export.export_bitmaps(&[&env.bitmap(6), &env.bitmap(7)]);
    let again = env.run_bitmap(6);
    assert_eq!(read_mode(&again), ("incremental", None));
    env.assert_restores(&again);
}

#[test]
fn image_bitmap_tampered_state_reads_in_full() {
    let env = setup();
    let export = &env.export;
    for g in [1, 2, 3, 4] {
        export.add_bitmap(&env.bitmap(g));
    }
    export.export_bitmaps(&[&env.bitmap(1), &env.bitmap(2)]);
    env.run_bitmap(1);

    env.rewrite_state(|s| s.bitmap_gen += 1);
    export.export_bitmaps(&[&env.bitmap(2), &env.bitmap(3)]);
    let ahead = env.run_bitmap(2);
    assert_eq!(read_mode(&ahead), ("full", Some("gen_mismatch")));
    env.assert_restores(&ahead);

    let behind = env.run_bitmap(2);
    assert_eq!(read_mode(&behind), ("full", Some("gen_mismatch")));
    env.assert_restores(&behind);

    export.export_bitmaps(&[&env.bitmap(3), &env.bitmap(4)]);
    fs::write(env.state_file(), b"garbage").unwrap();
    let garbage = env.run_bitmap(3);
    assert_eq!(read_mode(&garbage), ("full", Some("state_unreadable")));
    env.assert_restores(&garbage);
}

#[test]
fn image_bitmap_two_exports_with_an_explicit_name_read_in_full() {
    let env = setup();
    let a = &env.export;
    for g in [1, 2] {
        a.add_bitmap(&env.bitmap(g));
    }
    a.export_bitmaps(&[&env.bitmap(1), &env.bitmap(2)]);
    let first = env.run_at(&a.uri(), &["--name", EXPORT]);
    assert_eq!(read_mode(&first), ("full", Some("no_bitmap")));
    env.run_at(&a.uri(), &["--name", EXPORT, "--bitmap", &env.bitmap(1)]);
    assert_eq!(env.state_bytes().len(), 1);

    let mut other = disk();
    other[CHUNK..2 * CHUNK].copy_from_slice(&pseudo_random(CHUNK));
    let b = TestExport::start(other);
    for g in [2, 3, 4] {
        b.add_bitmap(&env.bitmap(g));
    }
    b.export_bitmaps(&[&env.bitmap(2), &env.bitmap(3)]);
    let swapped = env.run_at(&b.uri(), &["--name", EXPORT, "--bitmap", &env.bitmap(2)]);
    assert_eq!(read_mode(&swapped), ("full", Some("source_changed")));
    assert_eq!(env.restored(&swapped), b.data());

    b.write(2 * CHUNK as u64 + 8192, &[0x77; 4096]);
    b.export_bitmaps(&[&env.bitmap(3), &env.bitmap(4)]);
    let next = env.run_at(&b.uri(), &["--name", EXPORT, "--bitmap", &env.bitmap(3)]);
    assert_eq!(read_mode(&next), ("incremental", None));
    assert_eq!(env.restored(&next), b.data());
}
