mod common;

use common::{PASSWORD, init_repo, lode};

fn stderr_of(out: &assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&out.get_output().stderr).into_owned()
}

#[test]
fn secrets_password_file() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);

    for content in [format!("{PASSWORD}\n"), format!("{PASSWORD}\r\n")] {
        let file = tmp.path().join("pw");
        std::fs::write(&file, content).unwrap();
        lode(&repo)
            .env("LODE_PASSWORD", "wrong")
            .arg("--password-file")
            .arg(&file)
            .arg("snapshots")
            .assert()
            .success();
        lode(&repo)
            .env_remove("LODE_PASSWORD")
            .env("LODE_PASSWORD_FILE", &file)
            .arg("snapshots")
            .assert()
            .success();
    }

    let missing = tmp.path().join("missing");
    let out = lode(&repo)
        .arg("--password-file")
        .arg(&missing)
        .arg("snapshots")
        .assert()
        .failure();
    assert!(stderr_of(&out).contains("--password-file"));
}

#[test]
fn secrets_password_command() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);

    lode(&repo)
        .env_remove("LODE_PASSWORD")
        .args(["--password-command", &format!("printf '{PASSWORD}'")])
        .arg("snapshots")
        .assert()
        .success();

    let out = lode(&repo)
        .env_remove("LODE_PASSWORD")
        .args(["--password-command", "echo leaked-output; exit 3"])
        .arg("snapshots")
        .assert()
        .failure();
    let stderr = stderr_of(&out);
    assert!(stderr.contains("exit code 3"), "{stderr}");
    assert!(!stderr.contains("leaked-output"), "{stderr}");
    assert!(
        !String::from_utf8_lossy(&out.get_output().stdout).contains("leaked-output"),
        "the command's output is never echoed"
    );

    let out = lode(&repo)
        .args(["--password-command", "true"])
        .arg("snapshots")
        .assert()
        .failure();
    assert!(stderr_of(&out).contains("printed nothing"));
}

#[test]
fn secrets_conflicting_flags_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let out = lode(&repo)
        .args(["--password-file", "x", "--password-command", "true"])
        .arg("snapshots")
        .assert()
        .failure()
        .code(2);
    assert!(stderr_of(&out).contains("cannot be used with"));
}

#[test]
fn secrets_precedence_flag_over_env() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let right = tmp.path().join("right");
    let wrong = tmp.path().join("wrong");
    std::fs::write(&right, PASSWORD).unwrap();
    std::fs::write(&wrong, "wrong").unwrap();

    // flag beats LODE_PASSWORD_FILE and LODE_PASSWORD
    lode(&repo)
        .env("LODE_PASSWORD", "wrong")
        .env("LODE_PASSWORD_FILE", &wrong)
        .arg("--password-file")
        .arg(&right)
        .arg("snapshots")
        .assert()
        .success();
    // command beats the environment too
    lode(&repo)
        .env("LODE_PASSWORD", "wrong")
        .env("LODE_PASSWORD_FILE", &wrong)
        .args(["--password-command", &format!("printf '{PASSWORD}'")])
        .arg("snapshots")
        .assert()
        .success();
    // LODE_PASSWORD_FILE beats LODE_PASSWORD
    lode(&repo)
        .env("LODE_PASSWORD", "wrong")
        .env("LODE_PASSWORD_FILE", &right)
        .arg("snapshots")
        .assert()
        .success();
    // and a wrong higher-priority source is not skipped in favour of a right lower one
    lode(&repo)
        .env("LODE_PASSWORD_FILE", &wrong)
        .arg("snapshots")
        .assert()
        .failure();
}

#[test]
fn secrets_no_source_non_tty_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    for args in [&["repo", "init"][..], &["snapshots"]] {
        let out = lode(&repo)
            .env_remove("LODE_PASSWORD")
            .args(args)
            .assert()
            .failure();
        let stderr = stderr_of(&out);
        assert!(stderr.contains("LODE_PASSWORD_FILE"), "{stderr}");
        assert!(stderr.contains("--password-command"), "{stderr}");
    }
    assert!(!repo.exists());
}
