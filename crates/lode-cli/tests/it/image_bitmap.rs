use crate::common::{PASSWORD, cache_dir_of, init_repo, lode, pseudo_random};

use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use lodestore_format::image::BitmapNames;
use lodestore_format::repo::Repository;
use lodestore_format::source::SourceKey;
use lodestore_format::store::LocalFsStore;
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

fn names_of(repo: &Path, host: &str, image: &str) -> BitmapNames {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let repo_id = rt.block_on(async {
        let store = LocalFsStore::new(repo.to_path_buf());
        let repository = Repository::open(&store, PASSWORD.as_bytes()).await.unwrap();
        *repository.read_keys().repo_id()
    });
    BitmapNames::new(&repo_id, host.as_bytes(), &SourceKey::image(image).unwrap())
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
        let out = self
            .backup(uri, extra)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|v| v["type"] == "backup_summary")
            .expect("a backup_summary line")
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
