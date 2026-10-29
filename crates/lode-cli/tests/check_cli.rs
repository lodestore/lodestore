mod common;

use std::path::{Path, PathBuf};

use common::{init_repo, lode, pseudo_random, stdout_of};

fn repo_with_backup(tmp: &Path) -> PathBuf {
    let (repo, src) = (tmp.join("repo"), tmp.join("src"));
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("one.bin"), pseudo_random(300 << 10)).unwrap();
    std::fs::write(src.join("sub/two.txt"), b"two").unwrap();
    init_repo(&repo);
    lode(&repo).arg("backup").arg(&src).assert().success();
    repo
}

fn first_pack(repo: &Path) -> PathBuf {
    let mut packs = Vec::new();
    for shard in std::fs::read_dir(repo.join("packs")).unwrap() {
        for pack in std::fs::read_dir(shard.unwrap().path()).unwrap() {
            packs.push(pack.unwrap().path());
        }
    }
    packs.sort();
    packs.remove(0)
}

#[test]
fn check_cli_structure_passes() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo_with_backup(tmp.path());
    let text = stdout_of(lode(&repo).arg("check"));
    let first = text.lines().next().unwrap();
    assert!(
        first.starts_with("Structure check for repository "),
        "{text}"
    );
    assert!(first.ends_with("(direct)"), "{text}");
    assert!(text.contains("Result            Passed"), "{text}");
    assert!(text.contains("Snapshots checked 1"), "{text}");
    assert!(!text.contains("Problems"), "{text}");
}

#[test]
fn check_cli_structure_missing_pack() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo_with_backup(tmp.path());
    let pack = first_pack(&repo);
    std::fs::remove_file(&pack).unwrap();
    let out = lode(&repo).arg("check").assert().code(3);
    let text = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(text.contains("Result            Failed"), "{text}");
    let problems = text.split("Problems\n").nth(1).expect("a Problems block");
    let name = pack.file_name().unwrap().to_str().unwrap();
    assert!(problems.contains(name), "{text}");
}

#[test]
fn check_cli_json() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo_with_backup(tmp.path());
    std::fs::remove_file(first_pack(&repo)).unwrap();
    let out = lode(&repo).args(["--json", "check"]).assert().code(3);
    let line = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let report: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(report["type"], "check");
    assert_eq!(report["level"], "structure");
    assert_eq!(report["result"], "failed");
    assert!(report["findings_total"].as_u64().unwrap() >= 1);
    assert_eq!(report["findings"][0]["kind"], "missing_pack");
    assert!(report["facts"]["storage_used"].as_u64().unwrap() > 0);
    assert!(report["notes"].is_array());
}

#[test]
fn check_cli_packs_corrupted() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo_with_backup(tmp.path());

    let clean = stdout_of(lode(&repo).args(["check", "--level", "packs"]));
    let first = clean.lines().next().unwrap();
    assert!(
        first.starts_with("Pack reading check for repository "),
        "{clean}"
    );
    assert!(first.ends_with("(direct)"), "{clean}");
    assert!(clean.contains("Damaged blobs     0"), "{clean}");

    let pack = first_pack(&repo);
    let mut bytes = std::fs::read(&pack).unwrap();
    bytes[1175 + 10] ^= 0x01;
    std::fs::write(&pack, bytes).unwrap();
    let out = lode(&repo)
        .args(["check", "--level", "packs"])
        .assert()
        .code(3);
    let text = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(text.contains("Result            Failed"), "{text}");
    let problems = text.split("Problems\n").nth(1).expect("a Problems block");
    assert!(
        problems.contains(pack.file_name().unwrap().to_str().unwrap()),
        "{text}"
    );

    let out = lode(&repo)
        .args(["check", "--level", "packs", "--read-limit", "10GB"])
        .assert()
        .code(2);
    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("Use a size such as 5GiB or 500MiB."),
        "{stderr}"
    );
}

#[test]
fn check_cli_restore_passes() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo_with_backup(tmp.path());
    let text = stdout_of(lode(&repo).args(["check", "--level", "restore", "--files", "3"]));
    let first = text.lines().next().unwrap();
    assert!(
        first.starts_with("Test restore check for repository "),
        "{text}"
    );
    assert!(first.ends_with("(direct)"), "{text}");
    assert!(text.contains("Result            Passed"), "{text}");
    assert!(text.contains("Snapshots sampled 1"), "{text}");
    assert!(text.contains("Files restored    2"), "{text}");
    assert!(text.contains("Hash mismatches   0"), "{text}");
}

#[test]
fn check_cli_restore_size_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("big.bin"), pseudo_random(300 << 10)).unwrap();
    init_repo(&repo);
    lode(&repo).arg("backup").arg(&src).assert().success();
    let out = lode(&repo)
        .args(["check", "--level", "restore", "--max-data", "100KiB"])
        .assert()
        .code(1);
    let text = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(text.contains("Stopped at the size limit."), "{text}");
    assert!(text.contains("Files restored    0"), "{text}");
}

#[test]
fn check_cli_json_stopped_is_not_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("big.bin"), pseudo_random(300 << 10)).unwrap();
    init_repo(&repo);
    lode(&repo).arg("backup").arg(&src).assert().success();
    let out = lode(&repo)
        .args([
            "--json",
            "check",
            "--level",
            "restore",
            "--max-data",
            "100KiB",
        ])
        .assert()
        .code(1);
    let line = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let report: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(report["result"], "stopped");
    assert_eq!(report["stopped"], "Stopped at the size limit.");
}

#[test]
fn check_cli_empty_repository_has_nothing_to_check() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    for level in ["packs", "restore"] {
        let text = stdout_of(lode(&repo).args(["check", "--level", level]));
        assert!(
            text.contains("Result            Nothing to check"),
            "{text}"
        );
        assert!(!text.contains("Passed"), "{text}");
        let out = lode(&repo)
            .args(["--json", "check", "--level", level])
            .assert()
            .success();
        let line = String::from_utf8(out.get_output().stdout.clone()).unwrap();
        let report: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(report["result"], "nothing_to_check");
        assert!(report["nothing_to_check"].is_string());
    }
}
