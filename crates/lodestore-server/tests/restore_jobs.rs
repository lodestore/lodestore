mod common;

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use common::{AgentEnv, TestServer, WAIT, backends, wait_until};
use lodestore_proto::agent::v1::ConflictPolicy;
use lodestore_server::jobs::restore::{RestoreRequest, enqueue_restore};
use serde_json::{Value, json};
use sqlx::Row;

const MIB: usize = 1 << 20;

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed ^ 0x9E37_79B9_7F4A_7C15;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        out.extend_from_slice(&(z ^ (z >> 31)).to_le_bytes());
    }
    out.truncate(len);
    out
}

fn stamp(path: &Path, secs: u64) {
    File::open(path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::new(secs, 123_456_789))
        .unwrap();
}

fn write_file(path: &Path, data: &[u8], mode: u32, mtime: u64) {
    fs::write(path, data).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    stamp(path, mtime);
}

#[cfg(target_os = "linux")]
fn set_xattr(path: &Path) -> bool {
    rustix::fs::lsetxattr(path, "user.k", b"v", rustix::fs::XattrFlags::empty()).is_ok()
}

#[cfg(target_os = "linux")]
fn get_xattr(path: &Path) -> Option<Vec<u8>> {
    let mut buf = [0u8; 64];
    let n = rustix::fs::lgetxattr(path, "user.k", &mut buf[..]).ok()?;
    Some(buf[..n].to_vec())
}

struct Tree {
    dir: PathBuf,
    created: u64,
}

impl Tree {
    fn item(&self, rel: &str) -> Vec<u8> {
        self.dir
            .join(rel)
            .to_string_lossy()
            .into_owned()
            .into_bytes()
    }

    fn whole(&self) -> Vec<Vec<u8>> {
        vec![self.dir.to_string_lossy().into_owned().into_bytes()]
    }
}

fn build_tree(base: &Path, name: &str) -> Tree {
    let dir = base.join(name);
    fs::create_dir_all(dir.join("a/b")).unwrap();
    let dir = dir.canonicalize().unwrap();
    write_file(&dir.join("small.txt"), b"hello", 0o640, 1_600_000_000);
    write_file(
        &dir.join("a/big.bin"),
        &noise(5 * MIB, 1),
        0o600,
        1_600_000_100,
    );
    write_file(&dir.join("a/b/inner.txt"), b"inner", 0o644, 1_600_000_200);
    write_file(&dir.join("h1"), b"linked", 0o644, 1_600_000_300);
    fs::hard_link(dir.join("h1"), dir.join("a/h2")).unwrap();
    symlink("small.txt", dir.join("link")).unwrap();

    let sparse = dir.join("sparse.bin");
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = File::create(&sparse).unwrap();
        file.write_all(&noise(4096, 2)).unwrap();
        file.seek(SeekFrom::Start(3 * MIB as u64)).unwrap();
        file.write_all(&noise(4096, 3)).unwrap();
        file.set_len(4 * MIB as u64).unwrap();
    }
    stamp(&sparse, 1_600_000_400);
    #[cfg(target_os = "linux")]
    set_xattr(&dir.join("small.txt"));

    for (rel, t) in [
        ("a/b", 1_600_001_000),
        ("a", 1_600_001_100),
        ("", 1_600_001_200),
    ] {
        stamp(&dir.join(rel), t);
    }
    Tree { dir, created: 6 }
}

fn names(dir: &Path) -> Vec<std::ffi::OsString> {
    let mut all: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    all.sort();
    all
}

fn assert_trees_equal(a: &Path, b: &Path) {
    let (ma, mb) = (
        fs::symlink_metadata(a).unwrap(),
        fs::symlink_metadata(b).unwrap(),
    );
    assert_eq!(ma.file_type(), mb.file_type(), "type of {}", a.display());
    if ma.is_symlink() {
        assert_eq!(
            fs::read_link(a).unwrap(),
            fs::read_link(b).unwrap(),
            "{}",
            a.display()
        );
        return;
    }
    assert_eq!(
        ma.permissions().mode() & 0o7777,
        mb.permissions().mode() & 0o7777,
        "mode of {}",
        a.display()
    );
    assert_eq!(
        ma.modified().unwrap(),
        mb.modified().unwrap(),
        "mtime of {}",
        a.display()
    );
    if ma.is_dir() {
        let listed = names(a);
        assert_eq!(listed, names(b), "entries of {}", a.display());
        for name in listed {
            assert_trees_equal(&a.join(&name), &b.join(&name));
        }
        return;
    }
    assert_eq!(ma.len(), mb.len(), "size of {}", a.display());
    assert!(
        fs::read(a).unwrap() == fs::read(b).unwrap(),
        "content of {}",
        a.display()
    );
    #[cfg(target_os = "linux")]
    assert_eq!(get_xattr(a), get_xattr(b), "xattr of {}", a.display());
}

fn assert_restored(src: &Tree, out: &Path) {
    let restored = out.join(src.dir.strip_prefix("/").unwrap());
    assert_trees_equal(&src.dir, &restored);
    let (h1, h2) = (
        fs::metadata(restored.join("h1")).unwrap(),
        fs::metadata(restored.join("a/h2")).unwrap(),
    );
    assert_eq!(h1.ino(), h2.ino(), "hardlinks share one inode");
    assert_eq!(h1.nlink(), 2);
    #[cfg(target_os = "linux")]
    {
        let blocks = |p: &Path| fs::metadata(p).unwrap().blocks() * 512;
        let original = blocks(&src.dir.join("sparse.bin"));
        let copy = blocks(&restored.join("sparse.bin"));
        assert!(
            copy <= original + 64 * 1024,
            "sparse file got dense: {copy} > {original}"
        );
    }
}

fn restored_at(out: &Path, src: &Path) -> PathBuf {
    out.join(src.strip_prefix("/").unwrap())
}

fn request(
    host: &str,
    snapshot: &str,
    items: Vec<Vec<u8>>,
    destination: &Path,
    conflict: ConflictPolicy,
    owners: bool,
) -> RestoreRequest {
    RestoreRequest {
        snapshot_id: snapshot.to_string(),
        items,
        target_host_id: host.to_string(),
        destination: destination.to_string_lossy().into_owned(),
        in_place: false,
        conflict,
        restore_owners: owners,
    }
}

async fn queue(ts: &TestServer, req: RestoreRequest) -> String {
    enqueue_restore(ts.state(), req, "admin")
        .await
        .expect("queue a restore")
}

async fn column(ts: &TestServer, job: &str, name: &'static str) -> Value {
    let sql = format!("SELECT {name} FROM jobs WHERE id = $1");
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(job)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap();
    let text: Option<String> = row.try_get(name).unwrap();
    text.map(|t| serde_json::from_str(&t).unwrap())
        .unwrap_or(Value::Null)
}

async fn kinds(env: &AgentEnv, job: &str) -> Vec<String> {
    env.events(job, "")
        .await
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_string())
        .collect()
}

fn leftovers(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(".lode-restore-")
        {
            found.push(entry.path());
        }
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            found.extend(leftovers(&entry.path()));
        }
    }
    found
}

async fn assert_only_ancestor_warnings(env: &AgentEnv, id: &str, job: &Value, tree: &Tree) {
    let below = tree
        .dir
        .strip_prefix("/")
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let bad: Vec<_> = env
        .events(id, "?level=warning")
        .await
        .into_iter()
        .filter(|e| {
            !["owner_not_applied", "time_not_applied"].contains(&e["kind"].as_str().unwrap())
                || e["path"].as_str().unwrap_or_default().starts_with(&below)
        })
        .collect();
    assert!(bad.is_empty(), "{job} {bad:#?}");
    assert!(
        ["succeeded", "warnings"].contains(&job["state"].as_str().unwrap()),
        "{job}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_e2e_same_host_new_folder() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let tree = build_tree(env.work.path(), "src");
        let snapshot = env.backup(&[&tree.dir]).await;
        let out = env.work.path().join("out");

        let id = queue(
            env.ts(),
            request(
                &env.host,
                &snapshot,
                tree.whole(),
                &out,
                ConflictPolicy::OverwriteChanged,
                true,
            ),
        )
        .await;
        let job = env.wait_job(&id).await;
        assert_only_ancestor_warnings(&env, &id, &job, &tree).await;
        assert_restored(&tree, &out);

        let stats = column(env.ts(), &id, "stats_json").await;
        assert_eq!(stats["created"], tree.created, "{stats}");
        let kinds = kinds(&env, &id).await;
        assert!(
            !kinds.contains(&"restore_integrity".to_string()),
            "{kinds:?}"
        );
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_e2e_in_place_restores_changes() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let tree = build_tree(env.work.path(), "data");
        let snapshot = env.backup(&[&tree.dir]).await;

        fs::write(tree.dir.join("small.txt"), b"edited!").unwrap();
        fs::remove_file(tree.dir.join("a/b/inner.txt")).unwrap();
        fs::write(tree.dir.join("extra.txt"), b"extra").unwrap();
        let before = |rel: &str| {
            let meta = fs::metadata(tree.dir.join(rel)).unwrap();
            (meta.permissions().mode(), meta.modified().unwrap())
        };
        let ancestors: BTreeMap<&str, _> = ["", "a", "a/b"]
            .into_iter()
            .map(|r| (r, before(r)))
            .collect();

        let mut req = request(
            &env.host,
            &snapshot,
            vec![
                tree.item("small.txt"),
                tree.item("a/b/inner.txt"),
                tree.item("a/big.bin"),
            ],
            Path::new("/"),
            ConflictPolicy::OverwriteChanged,
            true,
        );
        req.in_place = true;
        let id = queue(env.ts(), req).await;
        let job = env.wait_job(&id).await;
        assert_only_ancestor_warnings(&env, &id, &job, &tree).await;

        assert_eq!(fs::read(tree.dir.join("small.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(tree.dir.join("a/b/inner.txt")).unwrap(), b"inner");
        assert_eq!(fs::read(tree.dir.join("extra.txt")).unwrap(), b"extra");
        for (rel, was) in ancestors {
            assert_eq!(before(rel), was, "metadata of '{rel}' changed");
        }
        let stats = column(env.ts(), &id, "stats_json").await;
        assert_eq!(stats["overwritten"], 1, "{stats}");
        assert_eq!(stats["created"], 1, "{stats}");
        assert_eq!(stats["skipped"], 1, "{stats}");
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_e2e_skip_existing() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let tree = build_tree(env.work.path(), "src");
        let snapshot = env.backup(&[&tree.dir]).await;
        let out = env.work.path().join("out");
        let run = |conflict| request(&env.host, &snapshot, tree.whole(), &out, conflict, false);

        let first = queue(env.ts(), run(ConflictPolicy::OverwriteChanged)).await;
        assert_eq!(env.wait_job(&first).await["state"], "succeeded");
        let small = restored_at(&out, &tree.dir).join("small.txt");
        fs::write(&small, b"mine").unwrap();

        let second = queue(env.ts(), run(ConflictPolicy::SkipExisting)).await;
        let job = env.wait_job(&second).await;
        assert_eq!(job["state"], "succeeded", "[{}] {job}", backend.name());
        assert_eq!(fs::read(&small).unwrap(), b"mine");
        let stats = column(env.ts(), &second, "stats_json").await;
        assert!(stats["skipped"].as_u64().unwrap() >= 1, "{stats}");
        assert_eq!(stats["created"], 0, "{stats}");
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_e2e_stop_if_not_empty() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let tree = build_tree(env.work.path(), "src");
        let snapshot = env.backup(&[&tree.dir]).await;

        let busy = env.work.path().join("busy");
        fs::create_dir_all(&busy).unwrap();
        fs::write(busy.join("keep.txt"), b"keep").unwrap();
        let id = queue(
            env.ts(),
            request(
                &env.host,
                &snapshot,
                tree.whole(),
                &busy,
                ConflictPolicy::StopIfNotEmpty,
                false,
            ),
        )
        .await;
        let job = env.wait_job(&id).await;
        assert_eq!(job["state"], "failed", "[{}] {job}", backend.name());
        assert_eq!(job["error_code"], "restore_target_not_empty", "{job}");
        assert_eq!(names(&busy), vec![std::ffi::OsString::from("keep.txt")]);

        let empty = env.work.path().join("empty");
        fs::create_dir_all(&empty).unwrap();
        let id = queue(
            env.ts(),
            request(
                &env.host,
                &snapshot,
                tree.whole(),
                &empty,
                ConflictPolicy::StopIfNotEmpty,
                false,
            ),
        )
        .await;
        let job = env.wait_job(&id).await;
        assert_eq!(job["state"], "succeeded", "{job}");
        assert_restored(&tree, &empty);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_e2e_owners_off_no_owner_warnings() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let tree = build_tree(env.work.path(), "src");
        let snapshot = env.backup(&[&tree.dir]).await;
        let out = env.work.path().join("out");
        let id = queue(
            env.ts(),
            request(
                &env.host,
                &snapshot,
                tree.whole(),
                &out,
                ConflictPolicy::OverwriteChanged,
                false,
            ),
        )
        .await;
        let job = env.wait_job(&id).await;
        assert_eq!(job["state"], "succeeded", "[{}] {job}", backend.name());
        let kinds = kinds(&env, &id).await;
        assert!(
            !kinds.contains(&"owner_not_applied".to_string()),
            "{kinds:?}"
        );
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_e2e_other_host_same_repo() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let other = AgentEnv::start_on(env.ts(), &env.repo, "main", "host-b").await;
        let tree = build_tree(env.work.path(), "src");
        let snapshot = env.backup(&[&tree.dir]).await;
        let out = other.work.path().join("out");

        let id = queue(
            env.ts(),
            request(
                &other.host,
                &snapshot,
                tree.whole(),
                &out,
                ConflictPolicy::OverwriteChanged,
                false,
            ),
        )
        .await;
        let job = env.wait_job(&id).await;
        assert_eq!(job["state"], "succeeded", "[{}] {job}", backend.name());
        assert_eq!(job["host_id"], other.host.as_str(), "{job}");
        assert_restored(&tree, &out);
        other.stop().await;
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_e2e_cross_repository() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let second = env.ts().create_local_repository("second").await;
        let other = AgentEnv::start_on(env.ts(), &second, "second", "host-c").await;
        let tree = build_tree(env.work.path(), "src");
        let snapshot = env.backup(&[&tree.dir]).await;
        let out = other.work.path().join("out");

        let id = queue(
            env.ts(),
            request(
                &other.host,
                &snapshot,
                tree.whole(),
                &out,
                ConflictPolicy::OverwriteChanged,
                false,
            ),
        )
        .await;
        let job = env.wait_job(&id).await;
        assert_eq!(job["state"], "succeeded", "[{}] {job}", backend.name());
        assert_restored(&tree, &out);
        let kinds = kinds(&env, &id).await;
        assert!(
            !kinds.contains(&"restore_integrity".to_string()),
            "{kinds:?}"
        );
        other.stop().await;
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_e2e_offline_target_waits() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let mut other = AgentEnv::start_on(env.ts(), &env.repo, "main", "host-b").await;
        let tree = build_tree(env.work.path(), "src");
        let snapshot = env.backup(&[&tree.dir]).await;
        other.stop_agent().await;
        let out = other.work.path().join("out");

        let id = queue(
            env.ts(),
            request(
                &other.host,
                &snapshot,
                tree.whole(),
                &out,
                ConflictPolicy::OverwriteChanged,
                false,
            ),
        )
        .await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let job = env.json(&format!("/api/v1/jobs/{id}")).await;
        assert_eq!(job["state"], "queued", "[{}] {job}", backend.name());
        assert!(!out.exists());

        other.start_agent().await;
        let job = env.wait_job(&id).await;
        assert_eq!(job["state"], "succeeded", "{job}");
        assert_restored(&tree, &out);
        other.stop().await;
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_e2e_waits_for_backup_on_host() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let tree = build_tree(env.work.path(), "src");
        let snapshot = env.backup(&[&tree.dir]).await;
        let out = env.work.path().join("out");

        let hooks = env.paths.hooks_dir();
        fs::create_dir_all(&hooks).unwrap();
        fs::set_permissions(&hooks, fs::Permissions::from_mode(0o700)).unwrap();
        let marker = env.work.path().join("hook-started");
        let script = hooks.join("slow");
        fs::write(
            &script,
            format!("#!/bin/sh\ntouch {}\nsleep 4\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let yaml = format!(
            "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: slow\n\
             spec:\n  repository: main\n  selector:\n    matchLabels:\n      env: e2e\n  \
             paths:\n    - {}\n  hooks:\n    before:\n      - script: slow\n",
            tree.dir.display()
        );
        let created = env
            .api
            .post("/api/v1/policies", &json!({ "yaml": yaml }))
            .await;
        assert_eq!(created.status(), 201);
        let started: Value = env
            .api
            .post("/api/v1/policies/slow/run", &json!({}))
            .await
            .json()
            .await
            .unwrap();
        let backup_id = started["jobs"][0]["id"].as_str().unwrap().to_string();
        assert!(
            wait_until(WAIT, || async { marker.exists() }).await,
            "the backup never started"
        );

        let id = queue(
            env.ts(),
            request(
                &env.host,
                &snapshot,
                tree.whole(),
                &out,
                ConflictPolicy::OverwriteChanged,
                false,
            ),
        )
        .await;
        loop {
            let backup = env.json(&format!("/api/v1/jobs/{backup_id}")).await;
            let restore = env.json(&format!("/api/v1/jobs/{id}")).await;
            let backup_done =
                !["queued", "assigned", "running"].contains(&backup["state"].as_str().unwrap());
            if backup_done {
                break;
            }
            assert_eq!(restore["state"], "queued", "{restore} while {backup}");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let job = env.wait_job(&id).await;
        assert_eq!(job["state"], "succeeded", "[{}] {job}", backend.name());
        assert_restored(&tree, &out);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_e2e_cancel_leaves_no_temp_files() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let big = env.work.path().join("big");
        fs::create_dir_all(&big).unwrap();
        let big = big.canonicalize().unwrap();
        let block = noise(8 * MIB, 7);
        let mut data = Vec::with_capacity(24 * 8 * MIB);
        for _ in 0..24 {
            data.extend_from_slice(&block);
        }
        fs::write(big.join("huge.bin"), &data).unwrap();
        drop(data);
        let snapshot = env.backup(&[&big]).await;
        let out = env.work.path().join("out");

        let id = queue(
            env.ts(),
            request(
                &env.host,
                &snapshot,
                vec![big.to_string_lossy().into_owned().into_bytes()],
                &out,
                ConflictPolicy::OverwriteChanged,
                false,
            ),
        )
        .await;
        let probe = out.clone();
        let seen = tokio::task::spawn_blocking(move || {
            let deadline = std::time::Instant::now() + WAIT;
            while std::time::Instant::now() < deadline {
                if !leftovers(&probe).is_empty() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            false
        })
        .await
        .unwrap();
        assert!(seen, "the restore never started writing");

        let response = env
            .api
            .post(&format!("/api/v1/jobs/{id}/cancel"), &json!({}))
            .await;
        assert_eq!(response.status(), 200);
        let job = env.wait_job(&id).await;
        assert_eq!(job["state"], "cancelled", "[{}] {job}", backend.name());
        assert!(leftovers(&out).is_empty(), "{:?}", leftovers(&out));
        assert!(!restored_at(&out, &big).join("huge.bin").exists());
        env.stop().await;
    }
}
