mod common;

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use common::{PASSWORD, init_repo, lode, stdout_of};
use serde_json::Value;

/// `lode` with no repository, password or profile in its environment; the
/// config folder is `xdg`.
fn bare(xdg: &Path) -> Command {
    let mut cmd = Command::cargo_bin("lode").unwrap();
    cmd.env_remove("LODE_REPOSITORY")
        .env_remove("LODE_PASSWORD")
        .env_remove("LODE_PASSWORD_FILE")
        .env("XDG_CONFIG_HOME", xdg)
        .env("HOME", xdg);
    cmd
}

fn write_profiles(xdg: &Path, toml: &str) {
    fs::create_dir_all(xdg.join("lode")).unwrap();
    fs::write(xdg.join("lode/config.toml"), toml).unwrap();
}

fn json_of(cmd: &mut Command) -> Value {
    serde_json::from_str(&stdout_of(cmd)).unwrap()
}

fn backup_one(repo: &Path, src: &Path) {
    lode(repo).arg("backup").arg(src).assert().success();
}

fn snapshot_count(cmd: &mut Command) -> usize {
    json_of(cmd)["snapshots"].as_array().unwrap().len()
}

#[test]
fn cli_profile_resolution_precedence() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("f"), b"data").unwrap();
    let (a, b, c) = (
        tmp.path().join("a"),
        tmp.path().join("b"),
        tmp.path().join("c"),
    );
    for repo in [&a, &b, &c] {
        init_repo(repo);
    }
    backup_one(&a, &src);
    backup_one(&b, &src);
    backup_one(&b, &src);
    let xdg = tmp.path().join("xdg");
    write_profiles(
        &xdg,
        &format!("[profiles.p]\nrepository = {:?}\n", a.display().to_string()),
    );

    let mut from_profile = bare(&xdg);
    from_profile
        .env("LODE_PASSWORD", PASSWORD)
        .args(["--profile", "p", "snapshots", "--json"]);
    assert_eq!(snapshot_count(&mut from_profile), 1);

    let mut from_env = bare(&xdg);
    from_env
        .env("LODE_PASSWORD", PASSWORD)
        .env("LODE_REPOSITORY", &b)
        .args(["--profile", "p", "snapshots", "--json"]);
    assert_eq!(snapshot_count(&mut from_env), 2);

    let mut from_flag = bare(&xdg);
    from_flag
        .env("LODE_PASSWORD", PASSWORD)
        .env("LODE_REPOSITORY", &b)
        .arg("-r")
        .arg(&c)
        .args(["--profile", "p", "snapshots", "--json"]);
    assert_eq!(snapshot_count(&mut from_flag), 0);
}

#[test]
fn cli_profile_password_file() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let pw_file = tmp.path().join("pw");
    fs::write(&pw_file, format!("{PASSWORD}\n")).unwrap();
    let xdg = tmp.path().join("xdg");
    write_profiles(
        &xdg,
        &format!(
            "[profiles.p]\nrepository = {:?}\npassword_file = {:?}\n",
            repo.display().to_string(),
            pw_file.display().to_string()
        ),
    );

    let mut cmd = bare(&xdg);
    cmd.args(["--profile", "p", "snapshots", "--json"]);
    assert_eq!(snapshot_count(&mut cmd), 0);
}

#[test]
fn cli_snapshots_json_schema() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    fs::create_dir(&src).unwrap();
    fs::write(src.join("f"), b"data").unwrap();
    init_repo(&repo);
    backup_one(&repo, &src);

    let doc = json_of(lode(&repo).args(["snapshots", "--json"]));
    insta::assert_json_snapshot!(doc, {
        ".snapshots[].id" => "[id]",
        ".snapshots[].short_id" => "[short_id]",
        ".snapshots[].time" => "[time]",
        ".snapshots[].hostname" => "[hostname]",
        ".snapshots[].paths" => "[paths]",
        ".snapshots[].stats.dirs" => "[dirs]",
        ".snapshots[].stats.bytes_added" => "[bytes_added]",
    });

    let snap = &doc["snapshots"][0];
    assert_eq!(snap["id"].as_str().unwrap().len(), 64);
    assert_eq!(snap["short_id"], snap["id"].as_str().unwrap()[..8]);
    let time = snap["time"].as_str().unwrap();
    assert!(time.ends_with('Z') && time.contains('.'), "{time}");
}

#[cfg(target_os = "linux")]
#[test]
fn cli_json_non_utf8_path_b64() {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let src = tmp.path().join(std::ffi::OsStr::from_bytes(b"bad-\xff"));
    fs::create_dir(&src).unwrap();
    fs::write(src.join("f"), b"data").unwrap();
    init_repo(&repo);
    backup_one(&repo, &src);

    let doc = json_of(lode(&repo).args(["snapshots", "--json"]));
    let path = &doc["snapshots"][0]["paths"][0];
    let encoded = path["b64"].as_str().expect("path is a {b64} object");
    let bytes = data_encoding::BASE64.decode(encoded.as_bytes()).unwrap();
    let canonical = fs::canonicalize(&src).unwrap().into_os_string().into_vec();
    assert_eq!(bytes, canonical);
}

fn stderr_of(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8(assert.get_output().stderr.clone()).unwrap()
}

#[test]
fn cli_exit_usage_2() {
    let tmp = tempfile::tempdir().unwrap();
    bare(tmp.path()).arg("--no-such-flag").assert().code(2);

    let assert = bare(tmp.path()).arg("snapshots").assert().code(2);
    let err = stderr_of(&assert);
    assert!(
        err.contains("LODE_REPOSITORY") && err.contains("--profile"),
        "{err}"
    );

    let assert = bare(tmp.path())
        .args(["--profile", "nope", "snapshots"])
        .assert()
        .code(2);
    assert!(stderr_of(&assert).contains("config.toml"));
}

#[test]
fn cli_exit_repo_not_found_10() {
    let tmp = tempfile::tempdir().unwrap();
    let empty = tmp.path().join("empty");
    fs::create_dir(&empty).unwrap();
    lode(&empty).arg("snapshots").assert().code(10);
    lode(&tmp.path().join("missing"))
        .arg("snapshots")
        .assert()
        .code(10);
}

#[test]
fn cli_exit_already_initialized_11() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    lode(&repo)
        .args(["repo", "init", "--recovery-key-file"])
        .arg(tmp.path().join("second.key"))
        .assert()
        .code(11);
}

#[test]
fn cli_exit_wrong_password_12() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    lode(&repo)
        .env("LODE_PASSWORD", "not the password")
        .arg("snapshots")
        .assert()
        .code(12);

    let key = fs::read_to_string(common::key_file_of(&repo)).unwrap();
    let mut mistyped = key.trim().to_owned();
    let last = mistyped.pop().unwrap();
    mistyped.push(if last == 'A' { 'B' } else { 'A' });
    lode(&repo)
        .env("LODE_PASSWORD", mistyped)
        .arg("snapshots")
        .assert()
        .code(12);
}

#[test]
fn cli_json_error_on_stderr() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let assert = lode(&repo)
        .env("LODE_PASSWORD", "not the password")
        .args(["snapshots", "--json"])
        .assert()
        .code(12);
    assert!(assert.get_output().stdout.is_empty());
    let err = stderr_of(&assert);
    assert_eq!(err.trim_end().lines().count(), 1, "{err}");
    let doc: Value = serde_json::from_str(&err).unwrap();
    assert_eq!(doc["v"], 1);
    assert_eq!(doc["type"], "error");
    assert_eq!(doc["exit_code"], 12);
    assert!(!doc["message"].as_str().unwrap().is_empty());
    assert!(!err.contains("not the password"));
}

#[test]
fn cli_init_json_requires_recovery_key_file() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let assert = lode(&repo)
        .args(["repo", "init", "--json"])
        .assert()
        .code(2);
    assert!(assert.get_output().stdout.is_empty());
    let doc: Value = serde_json::from_str(&stderr_of(&assert)).unwrap();
    assert_eq!(doc["exit_code"], 2);
    assert!(!repo.join("config").exists());

    let key_file = tmp.path().join("repo.key");
    let assert = lode(&repo)
        .args(["repo", "init", "--json", "--recovery-key-file"])
        .arg(&key_file)
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let doc: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(doc["v"], 1);
    assert_eq!(doc["type"], "init");
    assert_eq!(doc["repository_id"].as_str().unwrap().len(), 32);
    assert_eq!(doc["recovery_key_file"], key_file.to_str().unwrap());
    let key = fs::read_to_string(&key_file).unwrap();
    assert!(!stdout.contains(key.trim()) && !stderr_of(&assert).contains(key.trim()));
}

fn first_snapshot_id(repo: &Path) -> String {
    let doc = json_of(lode(repo).args(["snapshots", "--json"]));
    doc["snapshots"][0]["id"].as_str().unwrap().to_owned()
}

fn json_lines(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON: {l:?}: {e}")))
        .collect()
}

fn assert_progress_then_summary(lines: &[Value], progress: &str, summary: &str) {
    let (last, before) = lines.split_last().expect("at least the summary line");
    for l in before {
        assert_eq!(l["v"], 1);
        assert_eq!(l["type"], progress);
    }
    assert_eq!(last["v"], 1);
    assert_eq!(last["type"], summary);
}

#[test]
fn cli_backup_json_lines() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    fs::create_dir(&src).unwrap();
    common::make_tree(&src);
    init_repo(&repo);

    let stdout = stdout_of(lode(&repo).arg("backup").arg("--json").arg(&src));
    let lines = json_lines(&stdout);
    assert_progress_then_summary(&lines, "backup_progress", "backup_summary");
    let summary = lines.last().unwrap();
    let id = summary["snapshot_id"].as_str().unwrap();
    assert!(
        id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "{id}"
    );
    assert_eq!(summary["incomplete"], false);
    assert_eq!(summary["errors_total"], 0);
    assert!(summary["stats"]["files_new"].as_u64().unwrap() >= 5);
    assert!(summary["parent"].is_null());
    for l in &lines[..lines.len() - 1] {
        assert!(l["files_done"].is_u64() && l["bytes_done"].is_u64() && l["bytes_added"].is_u64());
        assert!(l["files_total"].is_u64() && l["bytes_total"].is_u64());
        assert!(l["scan_done"].is_boolean() && l["bytes_processed"].is_u64());
        assert!(l["current_path"].is_string() || l["current_path"]["b64"].is_string());
    }
}

fn stdin_backup(repo: &Path, name: &str, data: &[u8]) -> Value {
    let lines = json_lines(&stdout_of(
        lode(repo)
            .args(["backup", "--stdin", "--json", "--name", name])
            .write_stdin(data.to_vec()),
    ));
    assert_progress_then_summary(&lines, "backup_progress", "backup_summary");
    lines.last().unwrap().clone()
}

#[test]
fn cli_backup_stdin_json_lines() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let data = common::pseudo_random(1 << 20);

    let summary = stdin_backup(&repo, "app.dump", &data);
    assert_eq!(summary["incomplete"], false);
    assert_eq!(summary["stats"]["files_new"], 1);
    assert_eq!(summary["stats"]["bytes_total"], data.len());

    let dumped = lode(&repo)
        .args(["dump", "latest", "/app.dump"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(dumped, data);

    let doc = json_of(lode(&repo).args(["snapshots", "--json"]));
    let snap = &doc["snapshots"][0];
    assert_eq!(snap["tags"][0], "stdin=app.dump");
    assert_eq!(snap["paths"][0], "/app.dump");
}

#[test]
fn cli_backup_stdin_requires_name() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);

    lode(&repo)
        .args(["backup", "--stdin"])
        .write_stdin("x")
        .assert()
        .code(2);
    lode(&repo)
        .args(["backup", "--stdin", "--name", ""])
        .write_stdin("x")
        .assert()
        .code(2);
    lode(&repo)
        .args(["backup", "--stdin", "--name", "a/b"])
        .write_stdin("x")
        .assert()
        .failure();
    assert_eq!(snapshot_count(lode(&repo).args(["snapshots", "--json"])), 0);
}

#[test]
fn cli_backup_stdin_parent_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);

    let first = stdin_backup(&repo, "a.dump", b"one");
    assert!(first["parent"].is_null());
    let again = stdin_backup(&repo, "a.dump", b"two");
    assert_eq!(again["parent"], first["snapshot_id"]);
    let other = stdin_backup(&repo, "b.dump", b"three");
    assert!(other["parent"].is_null());
}

#[test]
fn cli_restore_json_lines() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src, target) = (
        tmp.path().join("repo"),
        tmp.path().join("src"),
        tmp.path().join("out"),
    );
    fs::create_dir(&src).unwrap();
    common::make_tree(&src);
    init_repo(&repo);
    backup_one(&repo, &src);

    let stdout = stdout_of(
        lode(&repo)
            .args(["restore", "--json", "--target"])
            .arg(&target)
            .arg(first_snapshot_id(&repo)),
    );
    let lines = json_lines(&stdout);
    assert_progress_then_summary(&lines, "restore_progress", "restore_summary");
    let summary = lines.last().unwrap();
    assert!(summary["unapplied"].is_array());
    assert!(summary["files_restored"].as_u64().unwrap() >= 5);
    assert!(summary["bytes_restored"].as_u64().unwrap() > 8 << 20);
    common::assert_trees_equal(&src, &common::restored_path(&target, &src));
}

#[test]
fn cli_human_output_no_ansi_when_not_tty() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src, target) = (
        tmp.path().join("repo"),
        tmp.path().join("src"),
        tmp.path().join("out"),
    );
    fs::create_dir(&src).unwrap();
    fs::write(src.join("f"), b"data").unwrap();
    init_repo(&repo);

    let mut outputs = Vec::new();
    outputs.push(lode(&repo).arg("backup").arg(&src).assert().success());
    outputs.push(lode(&repo).arg("snapshots").assert().success());
    outputs.push(
        lode(&repo)
            .args(["restore", "--target"])
            .arg(&target)
            .arg(&first_snapshot_id(&repo)[..8])
            .assert()
            .success(),
    );
    for assert in outputs {
        let out = assert.get_output();
        for stream in [&out.stdout, &out.stderr] {
            assert!(!String::from_utf8_lossy(stream).contains("\x1b["));
        }
    }
}

#[test]
fn cli_exit_incomplete_3() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    fs::create_dir(&src).unwrap();
    fs::write(src.join("ok"), b"data").unwrap();
    let locked = src.join("locked");
    fs::write(&locked, b"secret").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::File::open(&locked).is_ok() {
        return; // root reads everything
    }
    init_repo(&repo);

    let assert = lode(&repo)
        .arg("backup")
        .arg("--json")
        .arg(&src)
        .assert()
        .code(3);
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let summary = json_lines(&stdout).pop().unwrap();
    assert_eq!(summary["incomplete"], true);
    assert!(summary["errors_total"].as_u64().unwrap() >= 1);
}

#[test]
fn cli_restore_groups_parent_owner_note() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src, target) = (
        tmp.path().join("repo"),
        tmp.path().join("src"),
        tmp.path().join("out"),
    );
    fs::create_dir(&src).unwrap();
    fs::write(src.join("f"), b"data").unwrap();
    init_repo(&repo);
    backup_one(&repo, &src);

    let assert = lode(&repo)
        .args(["restore", "--target"])
        .arg(&target)
        .arg(first_snapshot_id(&repo))
        .assert()
        .success();
    let err = stderr_of(&assert);
    assert!(!err.contains(": owner:"), "{err}");
    assert!(err.matches("note:").count() <= 1, "{err}");
}
