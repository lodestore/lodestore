mod common;

use std::os::unix::fs::PermissionsExt;

use common::{
    PASSWORD, assert_trees_equal, init_repo, key_file_of, lode, make_tree, restored_path, stdout_of,
};

fn snapshot_id(repo: &std::path::Path) -> String {
    stdout_of(lode(repo).arg("snapshots"))[..8].to_owned()
}

#[test]
fn recovery_e2e_restore_with_recovery_key() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    std::fs::create_dir(&src).unwrap();
    make_tree(&src);
    let key_file = init_repo(&repo);
    lode(&repo).arg("backup").arg(&src).assert().success();
    let id = snapshot_id(&repo);

    let key = std::fs::read_to_string(&key_file).unwrap();
    assert!(key.starts_with("lode-rk1-"), "{key}");

    let by_env = tmp.path().join("out-env");
    lode(&repo)
        .env("LODE_PASSWORD", &key)
        .args(["restore", &id, "--target"])
        .arg(&by_env)
        .assert()
        .success();
    assert_trees_equal(&src, &restored_path(&by_env, &src));

    let by_file = tmp.path().join("out-file");
    lode(&repo)
        .env("LODE_PASSWORD_FILE", &key_file)
        .args(["restore", &id, "--target"])
        .arg(&by_file)
        .assert()
        .success();
    assert_trees_equal(&src, &restored_path(&by_file, &src));

    // The password still works, and lower case with spaces is accepted.
    let spaced = key
        .trim()
        .to_lowercase()
        .replace('-', " ")
        .replacen("lode rk1 ", "lode-rk1-", 1);
    lode(&repo)
        .env("LODE_PASSWORD", spaced)
        .arg("snapshots")
        .assert()
        .success();
    lode(&repo).arg("snapshots").assert().success();
}

#[test]
fn recovery_key_file_is_0600_and_exclusive() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let key_file = init_repo(&repo);

    let mode = std::fs::metadata(&key_file).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let key = std::fs::read_to_string(&key_file).unwrap();
    let body = key.trim_end().strip_prefix("lode-rk1-").unwrap();
    let groups: Vec<&str> = body.split('-').collect();
    assert_eq!(groups.len(), 14, "{key}");
    assert!(groups.iter().all(|g| g.len() == 4));
    assert!(key.ends_with('\n') && !key.ends_with("\n\n"));

    let second = tmp.path().join("second");
    lode(&second)
        .args(["repo", "init", "--recovery-key-file"])
        .arg(&key_file)
        .assert()
        .failure();
    assert!(!second.exists(), "no second repository was created");
    assert_eq!(std::fs::read_to_string(&key_file).unwrap(), key);
}

#[test]
fn recovery_init_refuses_without_confirmation() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let out = lode(&repo).args(["repo", "init"]).assert().failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(stderr.contains("--recovery-key-file"), "{stderr}");

    assert!(!repo.join("config").exists());
    let keys = repo.join("keys");
    assert!(!keys.exists() || std::fs::read_dir(keys).unwrap().count() == 0);
    assert!(!key_file_of(&repo).exists());
}

#[test]
fn recovery_init_failure_leaves_no_key_file() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);

    let again = tmp.path().join("again.key");
    lode(&repo)
        .args(["repo", "init", "--recovery-key-file"])
        .arg(&again)
        .assert()
        .failure();
    assert!(!again.exists(), "an init that fails must not leave a key");
}

#[test]
fn recovery_wrong_key_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, other) = (tmp.path().join("repo"), tmp.path().join("other"));
    let key_file = init_repo(&repo);
    let other_key_file = init_repo(&other);

    let key = std::fs::read_to_string(&key_file).unwrap();
    let mut typo = key.trim_end().to_owned().into_bytes();
    let pos = 12;
    typo[pos] = if typo[pos] == b'A' { b'B' } else { b'A' };
    let out = lode(&repo)
        .env("LODE_PASSWORD", String::from_utf8(typo).unwrap())
        .arg("snapshots")
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(stderr.contains("checksum"), "{stderr}");

    let out = lode(&repo)
        .env("LODE_PASSWORD_FILE", &other_key_file)
        .arg("snapshots")
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(
        stderr.contains("wrong password or recovery key"),
        "{stderr}"
    );
}

#[test]
fn recovery_key_not_on_stdout() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let key_file = key_file_of(&repo);
    let out = lode(&repo)
        .args(["repo", "init", "--recovery-key-file"])
        .arg(&key_file)
        .assert()
        .success();
    let key = std::fs::read_to_string(&key_file).unwrap();
    let key = key.trim_end();
    for stream in [&out.get_output().stdout, &out.get_output().stderr] {
        let text = String::from_utf8_lossy(stream);
        assert!(!text.contains("lode-rk1-"), "{text}");
        assert!(!text.contains(key));
        assert!(!text.contains(PASSWORD));
    }
}

#[test]
fn planted_objects_are_skipped_with_a_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    std::fs::create_dir(&src).unwrap();
    make_tree(&src);
    init_repo(&repo);
    lode(&repo).arg("backup").arg(&src).assert().success();
    let id = snapshot_id(&repo);

    let real = std::fs::read_dir(repo.join("snapshots"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let fake = "ab".repeat(32);
    std::fs::copy(&real, repo.join("snapshots").join(&fake)).unwrap();
    std::fs::write(repo.join("index").join("cd".repeat(32)), b"junk").unwrap();

    let out = lode(&repo).arg("snapshots").assert().success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(
        stdout.starts_with(&id) && stdout.lines().count() == 1,
        "{stdout}"
    );
    assert!(
        stderr.contains(&format!("warning: skipped snapshots/{fake}")),
        "{stderr}"
    );

    let out = lode(&repo).arg("backup").arg(&src).assert().success();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(
        stderr.contains(&format!("warning: skipped snapshots/{fake}")),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("warning: skipped index/{}", "cd".repeat(32))),
        "{stderr}"
    );
}

#[test]
fn backup_uploads_again_what_the_index_names_but_is_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    std::fs::create_dir(&src).unwrap();
    make_tree(&src);
    init_repo(&repo);
    lode(&repo).arg("backup").arg(&src).assert().success();
    std::fs::remove_dir_all(repo.join("packs")).unwrap();
    std::fs::create_dir(repo.join("packs")).unwrap();

    let out = lode(&repo)
        .args(["backup", "--host", "other"])
        .arg(&src)
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(
        stderr.contains("missing or have the wrong size"),
        "{stderr}"
    );

    let target = tmp.path().join("out");
    lode(&repo)
        .args(["restore", "latest", "--target"])
        .arg(&target)
        .assert()
        .success();
    assert_trees_equal(&src, &restored_path(&target, &src));
}

#[test]
fn backup_without_parent_when_parent_tree_is_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    std::fs::create_dir(&src).unwrap();
    make_tree(&src);
    init_repo(&repo);
    lode(&repo).arg("backup").arg(&src).assert().success();
    std::fs::remove_dir_all(repo.join("packs")).unwrap();
    std::fs::create_dir(repo.join("packs")).unwrap();

    let out = lode(&repo).arg("backup").arg(&src).assert().success();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(stderr.contains("without a parent"), "{stderr}");

    let target = tmp.path().join("out");
    lode(&repo)
        .args(["restore", "latest", "--target"])
        .arg(&target)
        .assert()
        .success();
    assert_trees_equal(&src, &restored_path(&target, &src));
}
