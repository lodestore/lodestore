mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use common::{assert_trees_equal, count_files, init_repo, lode, make_tree, restored_path};
use serde_json::Value;

/// A backed-up copy of `make_tree`; returns the repository and the
/// canonical source directory.
fn backed_up(tmp: &Path) -> (PathBuf, PathBuf) {
    let (repo, src) = (tmp.join("repo"), tmp.join("src"));
    fs::create_dir(&src).unwrap();
    make_tree(&src);
    init_repo(&repo);
    lode(&repo).arg("backup").arg(&src).assert().success();
    (repo, src.canonicalize().unwrap())
}

fn restore_cmd(repo: &Path, spec: &str, target: &Path, extra: &[&str]) -> Command {
    let mut cmd = lode(repo);
    cmd.args(["restore", spec, "--json", "--target"])
        .arg(target)
        .args(extra);
    cmd
}

/// Runs the restore and returns its `restore_summary` line.
fn summary_of(cmd: &mut Command) -> Value {
    let out = cmd.assert().success().get_output().stdout.clone();
    let line = String::from_utf8(out)
        .unwrap()
        .lines()
        .last()
        .unwrap()
        .to_owned();
    let summary: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(summary["type"], "restore_summary");
    summary
}

fn restore(repo: &Path, target: &Path, extra: &[&str]) -> Value {
    summary_of(&mut restore_cmd(repo, "latest", target, extra))
}

fn n(v: &Value, key: &str) -> u64 {
    v[key].as_u64().unwrap()
}

fn inodes(dir: &Path, out: &mut BTreeMap<PathBuf, u64>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            inodes(&path, out);
        } else {
            out.insert(path.clone(), fs::metadata(&path).unwrap().ino());
        }
    }
}

fn leftovers(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".lode-restore-")
        {
            found.push(path.clone());
        }
        if path.is_dir() {
            found.extend(leftovers(&path));
        }
    }
    found
}

#[test]
fn restore_overwrite_never_keeps_existing() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = backed_up(tmp.path());
    let target = tmp.path().join("out");
    restore(&repo, &target, &[]);
    let top = restored_path(&target, &src).join("top.txt");
    fs::write(&top, b"edited").unwrap();

    let files = count_files(&src) as u64;
    let summary = restore(&repo, &target, &["--overwrite", "never"]);
    assert_eq!(fs::read(&top).unwrap(), b"edited");
    assert_eq!(n(&summary, "files_skipped"), files);
    assert_eq!(n(&summary, "files_restored"), 0);

    let fresh = tmp.path().join("fresh");
    let summary = restore(&repo, &fresh, &["--overwrite", "never"]);
    assert_eq!(n(&summary, "files_restored"), files);
    assert_trees_equal(&src, &restored_path(&fresh, &src));
}

#[test]
fn restore_overwrite_always_replaces() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = backed_up(tmp.path());
    let target = tmp.path().join("out");
    restore(&repo, &target, &[]);
    let top = restored_path(&target, &src).join("top.txt");
    fs::write(&top, b"HELLO").unwrap();

    let files = count_files(&src) as u64;
    let summary = restore(&repo, &target, &["--overwrite", "always"]);
    assert_eq!(n(&summary, "files_restored"), files);
    assert_eq!(n(&summary, "files_skipped"), 0);
    assert_trees_equal(&src, &restored_path(&target, &src));
}

#[test]
fn restore_if_changed_skips_identical() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = backed_up(tmp.path());
    let target = tmp.path().join("out");
    restore(&repo, &target, &[]);
    let root = restored_path(&target, &src);
    let files = count_files(&src) as u64;

    let mut before = BTreeMap::new();
    inodes(&root, &mut before);
    let summary = restore(&repo, &target, &[]);
    let mut after = BTreeMap::new();
    inodes(&root, &mut after);
    assert_eq!(before, after, "identical files are not rewritten");
    assert_eq!(n(&summary, "files_skipped"), files);
    assert_eq!(n(&summary, "files_restored"), 0);

    // Same size, different bytes: only that file is written again.
    fs::write(root.join("top.txt"), b"HELLO").unwrap();
    let summary = restore(&repo, &target, &[]);
    assert_eq!(n(&summary, "files_skipped"), files - 1);
    assert_eq!(n(&summary, "files_restored"), 1);
    assert_trees_equal(&src, &root);
    let mut again = BTreeMap::new();
    inodes(&root, &mut again);
    for (path, ino) in &before {
        if !path.ends_with("top.txt") {
            assert_eq!(again[path], *ino, "{}", path.display());
        }
    }
}

#[test]
fn restore_resume_after_partial() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = backed_up(tmp.path());
    let target = tmp.path().join("out");
    restore(&repo, &target, &[]);
    let root = restored_path(&target, &src);

    fs::remove_file(root.join("a/b/c/repeat.bin")).unwrap();
    fs::remove_file(root.join("top.txt")).unwrap();
    fs::write(root.join("a/b/random.bin"), b"torn").unwrap();
    fs::write(root.join("a/b/.lode-restore-x"), b"half a file").unwrap();
    fs::write(root.join(".lode-restore-y"), b"half a file").unwrap();
    assert_eq!(leftovers(&target).len(), 2);

    restore(&repo, &target, &[]);
    assert_trees_equal(&src, &root);
    assert!(leftovers(&target).is_empty(), "{:?}", leftovers(&target));
}

#[test]
fn restore_leaves_foreign_temp_files_and_never_mode_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = backed_up(tmp.path());
    let target = tmp.path().join("out");
    restore(&repo, &target, &[]);
    let root = restored_path(&target, &src);

    let store_temp = root.join("a/.lode-tmp-inflight");
    let own_temp = root.join("a/.lode-restore-stale");
    fs::write(&store_temp, b"a local store write in progress").unwrap();
    fs::write(&own_temp, b"half a file").unwrap();

    restore(&repo, &target, &["--overwrite", "never"]);
    assert!(store_temp.exists());
    assert!(own_temp.exists(), "--overwrite never deletes nothing");

    restore(&repo, &target, &[]);
    assert!(
        store_temp.exists(),
        "the store's temp files are not restore's"
    );
    assert!(!own_temp.exists());
}

#[test]
fn restore_include_glob_and_subpath() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = backed_up(tmp.path());

    let globbed = tmp.path().join("globbed");
    let summary = restore(&repo, &globbed, &["--include", "*.txt"]);
    assert_eq!(n(&summary, "files_restored"), 2);
    let root = restored_path(&globbed, &src);
    assert_eq!(fs::read(root.join("top.txt")).unwrap(), b"hello");
    assert_eq!(fs::read(root.join("a/b/c/ro.txt")).unwrap(), b"read only");
    assert!(!root.join("a/script.sh").exists());
    assert!(!root.join("a/b/random.bin").exists());
    assert!(
        !root.join("empty").exists(),
        "directories with no match are left out"
    );
    assert_eq!(
        fs::metadata(root.join("a/b/c")).unwrap().mtime(),
        fs::metadata(src.join("a/b/c")).unwrap().mtime(),
        "the directories above a match get their metadata"
    );

    let by_path = tmp.path().join("by-path");
    let spec = src.join("a/b");
    let summary = restore(&repo, &by_path, &["--include", spec.to_str().unwrap()]);
    assert_eq!(n(&summary, "files_restored"), 3);
    assert_trees_equal(&src.join("a/b"), &restored_path(&by_path, &src).join("a/b"));
    assert!(!restored_path(&by_path, &src).join("top.txt").exists());

    let sub = tmp.path().join("sub");
    let snapshot = format!("latest:{}", src.join("a").display());
    let summary = summary_of(&mut restore_cmd(&repo, &snapshot, &sub, &[]));
    assert_eq!(
        n(&summary, "files_restored"),
        4 + u64::from(cfg!(target_os = "linux"))
    );
    assert_trees_equal(&src.join("a/b"), &sub.join("b"));
    assert_eq!(fs::read(sub.join("script.sh")).unwrap(), b"#!/bin/sh\n");
    assert!(!sub.join("a").exists());

    let single = tmp.path().join("single");
    let snapshot = format!("latest:{}", src.join("top.txt").display());
    let summary = summary_of(&mut restore_cmd(&repo, &snapshot, &single, &[]));
    assert_eq!(n(&summary, "files_restored"), 1);
    assert_eq!(fs::read(single.join("top.txt")).unwrap(), b"hello");
}

#[test]
fn restore_verify_reports_verified() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = backed_up(tmp.path());
    let target = tmp.path().join("out");

    let summary = restore(&repo, &target, &["--verify"]);
    let files = count_files(&src) as u64;
    assert_eq!(n(&summary, "files_verified"), files);
    assert_eq!(n(&summary, "files_restored"), files);
    assert!(summary["errors"].as_array().unwrap().is_empty());
    assert_trees_equal(&src, &restored_path(&target, &src));

    let plain = tmp.path().join("plain");
    assert_eq!(n(&restore(&repo, &plain, &[]), "files_verified"), 0);

    let human = tmp.path().join("human");
    let out = lode(&repo)
        .args(["restore", "latest", "--verify", "--target"])
        .arg(&human)
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    assert!(stdout.contains(&format!("{files} verified")), "{stdout}");
}

#[test]
fn restore_dry_run_touches_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = backed_up(tmp.path());
    let files = count_files(&src) as u64;

    let missing = tmp.path().join("missing");
    let summary = restore(&repo, &missing, &["--dry-run"]);
    assert!(!missing.exists(), "a dry run does not create the target");
    assert_eq!(n(&summary, "would_write"), files);
    assert_eq!(n(&summary, "files_restored"), 0);
    assert_eq!(summary["dry_run"], true);

    // A partly filled target stays exactly as it is.
    let target = tmp.path().join("out");
    restore(&repo, &target, &[]);
    let root = restored_path(&target, &src);
    fs::remove_file(root.join("top.txt")).unwrap();
    fs::write(root.join("a/script.sh"), b"#!/bin/sh\n\n").unwrap();
    fs::write(root.join("a/.lode-restore-z"), b"x").unwrap();
    let state = |dir: &Path| {
        let mut out = BTreeMap::new();
        fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, (u64, i64, Vec<u8>)>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                let meta = fs::symlink_metadata(&path).unwrap();
                let bytes = if meta.is_file() {
                    fs::read(&path).unwrap()
                } else {
                    Vec::new()
                };
                out.insert(path.clone(), (meta.ino(), meta.mtime_nsec(), bytes));
                if meta.is_dir() {
                    walk(&path, out);
                }
            }
        }
        walk(dir, &mut out);
        out
    };
    let before = state(&target);
    let summary = restore(&repo, &target, &["--dry-run"]);
    assert_eq!(n(&summary, "would_write"), 2);
    assert_eq!(n(&summary, "would_skip"), files - 2);
    assert_eq!(state(&target), before);

    // Onto the real filesystem root: every file is already there and identical.
    let before = state(&src);
    let summary = restore(&repo, Path::new("/"), &["--dry-run"]);
    assert_eq!(n(&summary, "would_skip"), files);
    assert_eq!(n(&summary, "would_write"), 0);
    assert_eq!(state(&src), before);
}

/// A snapshot from before backups refused large directories restores only
/// when asked to; the debug build lowers the limits for the test.
#[test]
fn restore_large_directory_needs_the_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src, target) = (
        tmp.path().join("repo"),
        tmp.path().join("src"),
        tmp.path().join("out"),
    );
    fs::create_dir(&src).unwrap();
    for i in 0..5 {
        fs::write(src.join(format!("f{i}")), b"x").unwrap();
    }
    init_repo(&repo);
    lode(&repo).arg("backup").arg(&src).assert().success();

    let out = lode(&repo)
        .env("LODE_TEST_DIR_LIMITS", "4,1000")
        .args(["restore", "latest", "--target"])
        .arg(&target)
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(out).unwrap();
    assert!(
        stderr.contains("more than 4 entries in one directory"),
        "{stderr}"
    );
    assert!(stderr.contains("--allow-large-directories"), "{stderr}");
    assert!(!restored_path(&target, &src).join("f0").exists());

    lode(&repo)
        .env("LODE_TEST_DIR_LIMITS", "4,1000")
        .args(["restore", "latest", "--allow-large-directories", "--target"])
        .arg(&target)
        .assert()
        .success();
    assert_eq!(
        fs::read(restored_path(&target, &src).join("f4")).unwrap(),
        b"x"
    );
}

/// A snapshot from before backups refused directories with much tree data
/// restores only when asked to; the debug build lowers the byte limits.
#[test]
fn restore_large_directory_by_bytes_needs_the_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src, target) = (
        tmp.path().join("repo"),
        tmp.path().join("src"),
        tmp.path().join("out"),
    );
    fs::create_dir(&src).unwrap();
    for i in 0..5 {
        fs::write(src.join(format!("f{i}")), b"x").unwrap();
    }
    init_repo(&repo);
    lode(&repo).arg("backup").arg(&src).assert().success();

    let out = lode(&repo)
        .env("LODE_TEST_DIR_BYTE_LIMITS", "64,100000")
        .args(["restore", "latest", "--target"])
        .arg(&target)
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(out).unwrap();
    assert!(
        stderr.contains("more than 64 bytes in one directory"),
        "{stderr}"
    );
    assert!(stderr.contains("--allow-large-directories"), "{stderr}");
    assert!(!restored_path(&target, &src).join("f0").exists());

    lode(&repo)
        .env("LODE_TEST_DIR_BYTE_LIMITS", "64,100000")
        .args(["restore", "latest", "--allow-large-directories", "--target"])
        .arg(&target)
        .assert()
        .success();
    assert_eq!(
        fs::read(restored_path(&target, &src).join("f4")).unwrap(),
        b"x"
    );
}
