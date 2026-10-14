mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use common::{
    PASSWORD, assert_trees_equal, count_files, init_repo, lode, make_tree, restored_path, stdout_of,
};

fn export_writer(repo: &Path) -> PathBuf {
    let file = repo.with_extension("writer-keys");
    lode(repo)
        .args(["key", "export-writer", "--output"])
        .arg(&file)
        .assert()
        .success();
    file
}

/// `lode --writer-keys <file>` with no password anywhere in the environment.
fn lode_writer(repo: &Path, keys: &Path) -> assert_cmd::Command {
    let mut cmd = lode(repo);
    cmd.env_remove("LODE_PASSWORD")
        .arg("--writer-keys")
        .arg(keys);
    cmd
}

fn backup_summary(cmd: &mut assert_cmd::Command) -> serde_json::Value {
    let out = stdout_of(cmd);
    out.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["type"] == "backup_summary")
        .expect("a backup_summary line")
}

fn setup() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    std::fs::create_dir(&src).unwrap();
    make_tree(&src);
    init_repo(&repo);
    let keys = export_writer(&repo);
    (tmp, repo, src, keys)
}

#[test]
fn keys_writer_backup_then_password_restore() {
    let (tmp, repo, src, keys) = setup();
    lode_writer(&repo, &keys)
        .arg("backup")
        .arg(&src)
        .assert()
        .success();

    let id = stdout_of(lode(&repo).arg("snapshots"))[..8].to_owned();
    let out = tmp.path().join("out");
    lode(&repo)
        .args(["restore", &id, "--target"])
        .arg(&out)
        .assert()
        .success();
    assert_trees_equal(&src, &restored_path(&out, &src));
}

#[test]
fn keys_writer_rebackup_no_new_packs() {
    let (_tmp, repo, src, keys) = setup();
    let first = backup_summary(
        lode_writer(&repo, &keys)
            .args(["--json", "backup"])
            .arg(&src),
    );
    let packs = count_files(&repo.join("packs"));
    assert!(packs > 0);

    let second = backup_summary(
        lode_writer(&repo, &keys)
            .args(["--json", "backup"])
            .arg(&src),
    );
    let stats = &second["stats"];
    assert!(stats["bytes_total"].as_u64().unwrap() >= 8 << 20);
    assert_eq!(stats["bytes_read"], stats["bytes_total"]);
    assert_eq!(stats["bytes_read"], first["stats"]["bytes_read"]);
    assert_eq!(stats["bytes_added"], 0);
    assert_eq!(count_files(&repo.join("packs")), packs);
}

#[test]
fn keys_writer_refused_for_read() {
    let (tmp, repo, src, keys) = setup();
    lode(&repo).arg("backup").arg(&src).assert().success();

    let snapshots = lode_writer(&repo, &keys).arg("snapshots").assert().code(12);
    assert!(String::from_utf8_lossy(&snapshots.get_output().stderr).contains("writer keys"));

    let restore = lode_writer(&repo, &keys)
        .args(["restore", "00000000", "--target"])
        .arg(tmp.path().join("out"))
        .assert()
        .code(12);
    assert!(String::from_utf8_lossy(&restore.get_output().stderr).contains("writer keys"));

    lode_writer(&repo, &keys)
        .args(["key", "export-writer", "--output"])
        .arg(tmp.path().join("again"))
        .assert()
        .code(12);
}

#[test]
fn keys_writer_config_pin_mismatch_refused() {
    let (_tmp, repo, src, keys) = setup();
    let config = repo.join("config");
    let mut bytes = std::fs::read(&config).unwrap();
    *bytes.last_mut().unwrap() ^= 1;
    std::fs::write(&config, bytes).unwrap();

    let packs = count_files(&repo.join("packs"));
    let failed = lode_writer(&repo, &keys)
        .arg("backup")
        .arg(&src)
        .assert()
        .failure();
    assert!(String::from_utf8_lossy(&failed.get_output().stderr).contains("config changed"));
    assert_eq!(count_files(&repo.join("packs")), packs);
}

#[test]
fn keys_writer_file_is_0600_and_exclusive() {
    let (_tmp, repo, _src, keys) = setup();
    let mode = std::fs::metadata(&keys).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let text = std::fs::read_to_string(&keys).unwrap();
    assert!(text.starts_with("LODE-WRITER-KEYS v1\n"));

    lode(&repo)
        .args(["key", "export-writer", "--output"])
        .arg(&keys)
        .assert()
        .failure();
    assert_eq!(std::fs::read_to_string(&keys).unwrap(), text);
}

fn opens_with(repo: &Path, secret: &str) -> assert_cmd::assert::Assert {
    lode(repo)
        .env("LODE_PASSWORD", secret)
        .arg("snapshots")
        .assert()
}

fn new_password_file(dir: &Path, password: &str) -> PathBuf {
    let file = dir.join("new-password");
    std::fs::write(&file, format!("{password}\n")).unwrap();
    file
}

#[test]
fn keys_add_password_both_open() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let file = new_password_file(tmp.path(), "second password");

    lode(&repo)
        .args(["key", "add", "--new-password-file"])
        .arg(&file)
        .assert()
        .success();

    assert_eq!(count_files(&repo.join("keys")), 3);
    opens_with(&repo, PASSWORD).success();
    opens_with(&repo, "second password").success();
}

#[test]
fn keys_add_recovery_key() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let first = init_repo(&repo);
    let second = tmp.path().join("second.recovery-key");

    lode(&repo)
        .args(["key", "add", "--recovery", "--recovery-key-file"])
        .arg(&second)
        .assert()
        .success();

    let (first, second) = (
        std::fs::read_to_string(first).unwrap(),
        std::fs::read_to_string(second).unwrap(),
    );
    assert_ne!(first, second);
    assert_eq!(count_files(&repo.join("keys")), 3);
    opens_with(&repo, first.trim()).success();
    opens_with(&repo, second.trim()).success();
    opens_with(&repo, PASSWORD).success();
}

#[test]
fn keys_passwd_old_rejected_new_accepted() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let recovery = init_repo(&repo);
    let file = new_password_file(tmp.path(), "brand new");
    assert_eq!(count_files(&repo.join("keys")), 2);

    lode(&repo)
        .args(["key", "passwd", "--new-password-file"])
        .arg(&file)
        .assert()
        .success();

    opens_with(&repo, PASSWORD).code(12);
    opens_with(&repo, "brand new").success();
    assert_eq!(count_files(&repo.join("keys")), 2);
    let recovery = std::fs::read_to_string(recovery).unwrap();
    opens_with(&repo, recovery.trim()).success();
}

#[test]
fn keys_passwd_refused_when_opened_by_recovery_key() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let recovery = std::fs::read_to_string(init_repo(&repo)).unwrap();
    let file = new_password_file(tmp.path(), "brand new");

    lode(&repo)
        .env("LODE_PASSWORD", recovery.trim())
        .args(["key", "passwd", "--new-password-file"])
        .arg(&file)
        .assert()
        .failure();
    assert_eq!(count_files(&repo.join("keys")), 2);
    opens_with(&repo, PASSWORD).success();
}
