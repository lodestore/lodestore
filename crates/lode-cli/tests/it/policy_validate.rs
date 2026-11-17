use std::fs;
use std::path::Path;

use assert_cmd::Command;
use serde_json::Value;

const GOOD: &str = "apiVersion: lodestore.io/v1alpha2
kind: BackupPolicy
metadata:
  name: vms
spec:
  repository: main
  source:
    type: proxmox-vm
    vmids: [101, 102]
";

fn run(args: &[&str], file: &Path, code: i32) -> (String, String) {
    let home = tempfile::tempdir().unwrap();
    let output = Command::cargo_bin("lode")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("LODESTORE_SERVER")
        .env_remove("LODESTORE_TOKEN")
        .args(args)
        .arg(file)
        .assert()
        .code(code)
        .get_output()
        .clone();
    (
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[test]
fn policy_validate_accepts_a_source_policy() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("good.yaml");
    fs::write(&file, GOOD).unwrap();
    let (stdout, stderr) = run(&["policy", "validate"], &file, 0);
    assert!(stdout.contains("1 policy is valid"), "{stdout}");
    assert_eq!(stderr, "");
}

#[test]
fn policy_validate_reports_problems_with_position() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("bad.yaml");
    fs::write(&file, GOOD.replace("[101, 102]", "[101]\n    bogus: 1")).unwrap();
    let name = file.display().to_string();
    let (stdout, stderr) = run(&["policy", "validate"], &file, 3);
    assert_eq!(stdout, "");
    let line = stderr.lines().next().unwrap();
    assert!(line.starts_with(&format!("{name}:")), "{stderr}");
    assert!(
        line.contains(": spec.source: unknown field `bogus`"),
        "{stderr}"
    );
    assert!(!line.contains(":1:1:"), "{stderr}");
}

#[test]
fn policy_validate_json_lists_files() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("bad.yaml");
    fs::write(&file, GOOD.replace("[101, 102]", "[]")).unwrap();
    let (stdout, _) = run(&["--json", "policy", "validate"], &file, 3);
    let value: Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
    let problems = &value["files"][0]["problems"];
    assert_eq!(problems[0]["path"], "spec.source", "{value}");
}
