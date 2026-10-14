mod common;

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{assert_trees_equal, count_files, init_repo, lode, restored_path, stdout_of};
use serde_json::Value;

const OLD: u64 = 1_600_000_000;

/// Deterministic incompressible bytes (splitmix64).
fn random_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
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

fn set_mtime(path: &Path, secs: u64) {
    fs::File::open(path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::new(secs, 5))
        .unwrap();
}

/// A file whose mtime is far in the past, so it is outside the racy window.
fn write_old(path: &Path, data: &[u8], mtime: u64) {
    fs::write(path, data).unwrap();
    set_mtime(path, mtime);
}

fn json_summary(cmd: &mut assert_cmd::Command) -> Value {
    stdout_of(cmd)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["type"] == "backup_summary")
        .expect("a backup_summary line")
}

fn backup_json(repo: &Path, src: &Path, extra: &[&str]) -> Value {
    json_summary(lode(repo).args(["backup", "--json"]).args(extra).arg(src))
}

fn snapshots_json(repo: &Path) -> Vec<Value> {
    let out = stdout_of(lode(repo).args(["snapshots", "--json"]));
    let v: Value = out
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["type"] == "snapshots")
        .expect("a snapshots line");
    v["snapshots"].as_array().unwrap().clone()
}

fn setup() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    fs::create_dir(&src).unwrap();
    init_repo(&repo);
    (tmp, repo, src)
}

/// Two small files and a 20 MiB one, all with old mtimes.
fn three_files(src: &Path) {
    write_old(&src.join("small1"), b"first", OLD);
    write_old(&src.join("small2"), b"second", OLD + 1);
    write_old(&src.join("big.bin"), &random_bytes(20 << 20, 1), OLD + 2);
    set_mtime(src, OLD + 3);
}

#[test]
fn backup_parent_reuse_skips_unchanged() {
    let (tmp, repo, src) = setup();
    three_files(&src);
    let first = backup_json(&repo, &src, &[]);
    assert!(first["parent"].is_null());
    assert_eq!(first["stats"]["files_new"], 3);

    let changed = b"first, but longer now";
    write_old(&src.join("small1"), changed, OLD + 10);
    let second = backup_json(&repo, &src, &[]);
    assert_eq!(second["parent"], first["snapshot_id"]);
    assert_eq!(second["stats"]["files_unchanged"], 2);
    assert_eq!(second["stats"]["files_changed"], 1);
    assert_eq!(second["stats"]["files_new"], 0);
    assert_eq!(second["stats"]["bytes_read"], changed.len());

    let out = tmp.path().join("out");
    let id = second["snapshot_id"].as_str().unwrap();
    lode(&repo)
        .args(["restore", &id[..8], "--target"])
        .arg(&out)
        .assert()
        .success();
    assert_trees_equal(&src, &restored_path(&out, &src));
}

#[test]
fn backup_force_rescan_reads_all() {
    let (_tmp, repo, src) = setup();
    three_files(&src);
    let first = backup_json(&repo, &src, &[]);
    let total = first["stats"]["bytes_total"].as_u64().unwrap();

    let second = backup_json(&repo, &src, &["--force-rescan"]);
    assert_eq!(second["stats"]["files_unchanged"], 0);
    assert_eq!(second["stats"]["bytes_read"].as_u64().unwrap(), total);
    assert_eq!(second["stats"]["bytes_added"], 0);
}

#[test]
fn backup_parent_explicit() {
    let (_tmp, repo, src) = setup();
    three_files(&src);
    let first = backup_json(&repo, &src, &[]);
    let id = first["snapshot_id"].as_str().unwrap();

    let other_host = backup_json(&repo, &src, &["--host", "other"]);
    assert!(other_host["parent"].is_null());
    assert_eq!(other_host["stats"]["files_unchanged"], 0);

    let explicit = backup_json(&repo, &src, &["--host", "other", "--parent", &id[..12]]);
    assert_eq!(explicit["parent"], first["snapshot_id"]);
    assert_eq!(explicit["stats"]["files_unchanged"], 3);
    assert_eq!(explicit["stats"]["bytes_read"], 0);
}

#[test]
fn backup_racy_mtime_reread() {
    let (_tmp, repo, src) = setup();
    fs::write(src.join("fresh"), b"just written").unwrap();
    fs::write(src.join("fresh2"), random_bytes(3 << 20, 2)).unwrap();
    // A future mtime stays inside the racy window however slow the run is.
    let future = SystemTime::now() + Duration::from_secs(3600);
    for name in ["fresh", "fresh2"] {
        let file = fs::File::options()
            .write(true)
            .open(src.join(name))
            .unwrap();
        file.set_modified(future).unwrap();
    }
    let first = backup_json(&repo, &src, &[]);

    let second = backup_json(&repo, &src, &[]);
    assert_eq!(second["parent"], first["snapshot_id"]);
    assert_eq!(second["stats"]["files_unchanged"], 0);
    assert_eq!(
        second["stats"]["bytes_read"].as_u64().unwrap(),
        first["stats"]["bytes_total"].as_u64().unwrap()
    );
}

#[test]
fn backup_dedup_across_hosts() {
    let (_tmp, repo, src) = setup();
    three_files(&src);
    backup_json(&repo, &src, &["--host", "a"]);
    let packs = count_files(&repo.join("packs"));
    let shards = count_files(&repo.join("index"));
    assert!(packs > 0);

    let second = backup_json(&repo, &src, &["--host", "b"]);
    assert!(second["parent"].is_null());
    assert_eq!(second["stats"]["bytes_added"], 0);
    assert_eq!(count_files(&repo.join("packs")), packs);
    assert_eq!(count_files(&repo.join("index")), shards);
    assert_eq!(count_files(&repo.join("snapshots")), 2);
}

#[test]
fn backup_tags_and_host_in_manifest() {
    let (_tmp, repo, src) = setup();
    three_files(&src);
    lode(&repo)
        .args([
            "backup", "--host", "box-7", "--tag", "nightly", "--tag", "db",
        ])
        .arg(&src)
        .assert()
        .success();

    let snapshots = snapshots_json(&repo);
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0]["hostname"], "box-7");
    assert_eq!(snapshots[0]["tags"], serde_json::json!(["nightly", "db"]));
}

#[test]
fn backup_parent_rejected_with_writer_keys() {
    let (tmp, repo, src) = setup();
    three_files(&src);
    let first = backup_json(&repo, &src, &[]);
    let id = first["snapshot_id"].as_str().unwrap();

    let keys = tmp.path().join("writer-keys");
    lode(&repo)
        .args(["key", "export-writer", "--output"])
        .arg(&keys)
        .assert()
        .success();
    let writer = |extra: &[&str]| {
        let mut cmd = lode(&repo);
        cmd.env_remove("LODE_PASSWORD")
            .arg("--writer-keys")
            .arg(&keys)
            .args(["backup", "--json"])
            .args(extra)
            .arg(&src);
        cmd
    };

    let refused = writer(&["--parent", id]).assert().failure();
    assert_eq!(refused.get_output().status.code(), Some(2));

    let summary = json_summary(&mut writer(&[]));
    assert!(summary["parent"].is_null());
    assert_eq!(summary["stats"]["files_unchanged"], 0);
}

/// Every path below `root` relative to it, directories with a trailing `/`.
fn list_tree(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = format!("{prefix}{}", entry.file_name().to_string_lossy());
            if entry.file_type().unwrap().is_dir() {
                out.push(format!("{name}/"));
                walk(&entry.path(), &format!("{name}/"), out);
            } else {
                out.push(name);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, "", &mut out);
    out.sort();
    out
}

/// Backs `src` up with `extra` flags, restores the new snapshot and lists it.
fn backup_and_list(tmp: &Path, repo: &Path, src: &Path, extra: &[&str]) -> Vec<String> {
    let summary = backup_json(repo, src, extra);
    let id = summary["snapshot_id"].as_str().unwrap();
    let out = tmp.join(format!("out-{}", &id[..8]));
    lode(repo)
        .args(["restore", &id[..8], "--target"])
        .arg(&out)
        .assert()
        .success();
    list_tree(&restored_path(&out, src))
}

fn touch(path: &Path, data: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
}

#[test]
fn backup_exclude_gitignore_semantics() {
    let (tmp, repo, src) = setup();
    let canonical = src.canonicalize().unwrap();
    for file in [
        "a.log",
        "keep.log",
        "sub/b.log",
        "sub/keep.log",
        "build/inner.txt",
        "sub/build",
        "anchored.txt",
        "sub/anchored.txt",
        "deep/x/node_modules/m.js",
        "deep/y/z/node_modules/m.js",
        "deep/x/other.js",
    ] {
        touch(&src.join(file), b"data");
    }
    let anchored = format!("{}/anchored.txt", canonical.display());
    let listing = backup_and_list(
        tmp.path(),
        &repo,
        &src,
        &[
            "--exclude",
            "*.log",
            "--exclude",
            "!keep.log",
            "--exclude",
            "build/",
            "--exclude",
            &anchored,
            "--exclude",
            "**/node_modules",
        ],
    );
    assert_eq!(
        listing,
        [
            "deep/",
            "deep/x/",
            "deep/x/other.js",
            "deep/y/",
            "deep/y/z/",
            "keep.log",
            "sub/",
            "sub/anchored.txt",
            "sub/build",
            "sub/keep.log",
        ]
    );
}

#[test]
fn backup_exclude_file() {
    let (tmp, repo, src) = setup();
    for file in ["a.tmp", "b.txt", "skip/c.txt", "d.bak", "keep/e.txt"] {
        touch(&src.join(file), b"data");
    }
    let patterns = tmp.path().join("patterns");
    fs::write(&patterns, "# scratch files\n*.tmp\n\n   \nskip/\n").unwrap();
    let more = tmp.path().join("more");
    fs::write(&more, "*.bak\n").unwrap();

    let listing = backup_and_list(
        tmp.path(),
        &repo,
        &src,
        &[
            "--exclude-file",
            patterns.to_str().unwrap(),
            "--exclude-file",
            more.to_str().unwrap(),
        ],
    );
    assert_eq!(listing, ["b.txt", "keep/", "keep/e.txt"]);
}

#[test]
fn backup_exclude_caches() {
    let (tmp, repo, src) = setup();
    let signature = "Signature: 8a477f597d28d172789f06886806bc55\n# created by a cache\n";
    touch(&src.join("cache/CACHEDIR.TAG"), signature.as_bytes());
    touch(&src.join("cache/data.bin"), b"cached");
    touch(&src.join("cache/nested/more.bin"), b"cached");
    touch(
        &src.join("notcache/CACHEDIR.TAG"),
        b"Signature: not-the-one\n",
    );
    touch(&src.join("notcache/data.bin"), b"kept");
    touch(&src.join("plain/file.txt"), b"kept");

    let listing = backup_and_list(tmp.path(), &repo, &src, &["--exclude-caches"]);
    assert_eq!(
        listing,
        [
            "cache/",
            "cache/CACHEDIR.TAG",
            "notcache/",
            "notcache/CACHEDIR.TAG",
            "notcache/data.bin",
            "plain/",
            "plain/file.txt",
        ]
    );
}

#[test]
fn backup_unreadable_file_incomplete_exit3() {
    use std::os::unix::fs::PermissionsExt;

    let (tmp, repo, src) = setup();
    touch(&src.join("ok.txt"), b"readable");
    touch(&src.join("secret.txt"), b"not for this user");
    let secret = src.join("secret.txt");
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&secret).is_ok() {
        eprintln!("skipped: mode 000 does not lock out this user (root)");
        return;
    }

    let out = lode(&repo)
        .args(["backup", "--json"])
        .arg(&src)
        .assert()
        .code(3);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let summary: Value = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["type"] == "backup_summary")
        .unwrap();
    assert_eq!(summary["incomplete"], true);
    assert_eq!(summary["errors_total"], 1);

    let snapshots = snapshots_json(&repo);
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0]["incomplete"], true);

    let restored = tmp.path().join("out");
    let id = snapshots[0]["id"].as_str().unwrap();
    lode(&repo)
        .args(["restore", &id[..8], "--target"])
        .arg(&restored)
        .assert()
        .success();
    assert_eq!(
        list_tree(&restored_path(&restored, &src)),
        ["ok.txt".to_owned()]
    );
}

#[test]
fn backup_missing_root_fails_no_snapshot() {
    let (tmp, repo, src) = setup();
    touch(&src.join("a.txt"), b"data");
    let missing = tmp.path().join("does-not-exist");

    lode(&repo)
        .arg("backup")
        .arg(&src)
        .arg(&missing)
        .assert()
        .code(1);
    assert_eq!(count_files(&repo.join("snapshots")), 0);
    assert_eq!(count_files(&repo.join("packs")), 0);
    assert_eq!(count_files(&repo.join("index")), 0);
}

#[test]
fn backup_sigint_exit130_then_resume_dedups() {
    use std::process::{Command, Stdio};
    use std::time::Instant;

    const FILES: u64 = 5;
    const FILE_SIZE: usize = 32 << 20;

    let (_tmp, repo, src) = setup();
    for i in 0..FILES {
        fs::write(
            src.join(format!("big{i}.bin")),
            random_bytes(FILE_SIZE, i << 32),
        )
        .unwrap();
    }

    let program = lode(&repo).get_program().to_owned();
    let mut child = Command::new(program)
        .env("LODE_PASSWORD", common::PASSWORD)
        .env_remove("LODE_PASSWORD_FILE")
        .arg("-r")
        .arg(&repo)
        .arg("backup")
        .arg(&src)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(300);
    while count_files(&repo.join("packs")) == 0 {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("backup ended ({status}) before the first pack appeared; raise FILES");
        }
        assert!(Instant::now() < deadline, "no pack was uploaded in time");
        std::thread::sleep(Duration::from_millis(20));
    }
    let killed = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let status = child.wait().unwrap();
    assert_eq!(
        status.code(),
        Some(130),
        "backup did not stop with 130 ({status}); if it exited 0 it finished before the signal, raise FILES"
    );

    assert_eq!(count_files(&repo.join("snapshots")), 0);
    assert!(count_files(&repo.join("index")) > 0);
    let uploaded = count_files(&repo.join("packs"));
    assert!(uploaded > 0);

    let resumed = backup_json(&repo, &src, &[]);
    let total = count_files(&repo.join("packs"));
    assert!(total > uploaded, "the second run uploads the rest");
    assert!(
        resumed["stats"]["bytes_added"].as_u64().unwrap() < FILES * FILE_SIZE as u64,
        "packs from the interrupted run are not uploaded again"
    );
    assert_eq!(count_files(&repo.join("snapshots")), 1);
}
