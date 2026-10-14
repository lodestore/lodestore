mod common;

use common::{
    assert_trees_equal, count_files, init_repo, lode, make_tree, restored_path, stdout_of,
};

#[test]
fn e2e_local_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src, target) = (
        tmp.path().join("repo"),
        tmp.path().join("src"),
        tmp.path().join("out"),
    );
    std::fs::create_dir(&src).unwrap();
    make_tree(&src);

    init_repo(&repo);
    let saved = stdout_of(lode(&repo).arg("backup").arg(&src));
    let id = saved
        .strip_prefix("snapshot ")
        .and_then(|s| s.split_whitespace().next())
        .expect("backup prints the snapshot id")
        .to_owned();

    let listing = stdout_of(lode(&repo).arg("snapshots"));
    assert_eq!(listing.lines().count(), 1);
    assert!(listing.starts_with(&id[..8]), "{listing}");

    lode(&repo)
        .args(["restore", &id[..8], "--target"])
        .arg(&target)
        .assert()
        .success();
    assert_trees_equal(&src, &restored_path(&target, &src));
}

#[test]
fn e2e_local_restore_requires_target() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);

    let out = lode(&repo).args(["restore", "abcd"]).assert().failure();
    let code = out.get_output().status.code().unwrap();
    assert_ne!(code, 0);
    assert!(
        String::from_utf8_lossy(&out.get_output().stderr).contains("--target"),
        "usage error names the missing flag"
    );
    assert!(!tmp.path().join("out").exists());
}

#[test]
fn e2e_local_layout() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);

    assert!(repo.join("config").is_file());
    assert!(std::fs::read_dir(repo.join("keys")).unwrap().count() > 0);
    for dir in ["packs", "index", "snapshots", "locks"] {
        assert!(repo.join(dir).is_dir(), "{dir}");
    }
}

#[test]
fn e2e_local_rebackup_no_new_packs() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    std::fs::create_dir(&src).unwrap();
    make_tree(&src);
    init_repo(&repo);

    lode(&repo).arg("backup").arg(&src).assert().success();
    let packs = count_files(&repo.join("packs"));
    assert!(packs > 0);

    lode(&repo).arg("backup").arg(&src).assert().success();
    assert_eq!(count_files(&repo.join("packs")), packs);
}

#[test]
fn e2e_local_empty_inputs() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    std::fs::create_dir_all(src.join("empty_dir")).unwrap();
    std::fs::write(src.join("zero"), b"").unwrap();
    init_repo(&repo);

    assert_eq!(stdout_of(lode(&repo).arg("snapshots")), "");

    lode(&repo).arg("backup").arg(&src).assert().success();
    let target = tmp.path().join("out");
    let id = stdout_of(lode(&repo).arg("snapshots"))[..8].to_owned();
    lode(&repo)
        .args(["restore", &id, "--target"])
        .arg(&target)
        .assert()
        .success();
    let restored = restored_path(&target, &src);
    assert!(restored.join("empty_dir").is_dir());
    assert_eq!(std::fs::metadata(restored.join("zero")).unwrap().len(), 0);
}

#[test]
fn e2e_local_password_required() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    for args in [&["repo", "init"][..], &["snapshots"]] {
        let out = lode(&repo)
            .env_remove("LODE_PASSWORD")
            .args(args)
            .assert()
            .failure();
        let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
        assert!(stderr.contains("LODE_PASSWORD"), "{stderr}");
    }
    assert!(!repo.exists());
}
