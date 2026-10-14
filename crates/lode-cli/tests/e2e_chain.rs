mod common;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use common::{assert_trees_equal, key_file_of, lode, make_tree, restored_path};
use lodestore_testkit::minio::MinioServer;
use serde_json::Value;

fn json_lines(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8(stdout.to_vec())
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn path_bytes(v: &Value) -> Vec<u8> {
    match v {
        Value::String(s) => s.as_bytes().to_vec(),
        other => data_encoding::BASE64
            .decode(other["b64"].as_str().unwrap().as_bytes())
            .unwrap(),
    }
}

fn all_paths(dir: &Path, out: &mut BTreeSet<Vec<u8>>) {
    use std::os::unix::ffi::OsStrExt;
    out.insert(dir.as_os_str().as_bytes().to_vec());
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            all_paths(&path, out);
        } else {
            out.insert(path.as_os_str().as_bytes().to_vec());
        }
    }
}

fn stdout_bytes(cmd: &mut Command) -> Vec<u8> {
    cmd.assert().success().get_output().stdout.clone()
}

/// init, backup, snapshots, ls, restore with the password and with the
/// recovery key, and dump, all through the `lode` binary. `lode` builds a
/// command for the repository under test.
fn run_chain(lode: &dyn Fn() -> Command, key_file: &Path, tmp: &Path) {
    let src = tmp.join("src");
    fs::create_dir(&src).unwrap();
    make_tree(&src);
    let src = src.canonicalize().unwrap();

    lode()
        .args(["repo", "init", "--recovery-key-file"])
        .arg(key_file)
        .assert()
        .success();
    lode().arg("backup").arg(&src).assert().success();

    let snapshots = json_lines(&stdout_bytes(lode().args(["snapshots", "--json"])));
    let list = snapshots[0]["snapshots"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    let id = list[0]["id"].as_str().unwrap().to_owned();

    let listed: BTreeSet<Vec<u8>> =
        json_lines(&stdout_bytes(lode().args(["ls", "--json", "latest"])))
            .iter()
            .inspect(|l| assert_eq!(l["type"], "node"))
            .map(|l| path_bytes(&l["path"]))
            .collect();
    let mut expected = BTreeSet::new();
    all_paths(&src, &mut expected);
    assert!(expected.is_subset(&listed), "ls lists every source path");

    let by_password = tmp.join("out-password");
    lode()
        .args(["restore", &id[..8], "--target"])
        .arg(&by_password)
        .assert()
        .success();
    assert_trees_equal(&src, &restored_path(&by_password, &src));

    let by_key = tmp.join("out-key");
    lode()
        .env_remove("LODE_PASSWORD")
        .env("LODE_PASSWORD_FILE", key_file)
        .args(["restore", "latest", "--target"])
        .arg(&by_key)
        .assert()
        .success();
    assert_trees_equal(&src, &restored_path(&by_key, &src));

    let dumped = stdout_bytes(lode().args(["dump", &id]).arg(src.join("a/b/random.bin")));
    assert_eq!(dumped, fs::read(src.join("a/b/random.bin")).unwrap());
    let long = String::from_utf8_lossy(&stdout_bytes(lode().args(["ls", "--long", "latest"])))
        .into_owned();
    assert!(
        long.lines()
            .any(|l| l.starts_with("-0751") && l.ends_with("script.sh"))
    );
}

#[test]
fn e2e_chain_local_password_and_recovery() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    run_chain(&|| lode(&repo), &key_file_of(&repo), tmp.path());
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_chain_minio() {
    let minio = MinioServer::start().await;
    minio.create_bucket("chain").await;
    let url = format!("s3:{}/chain/repo", minio.endpoint());
    let (user, secret) = minio.root_credentials();
    let tmp = tempfile::tempdir().unwrap();
    let lode_s3 = || {
        let mut cmd = lode(Path::new(&url));
        cmd.env("AWS_ACCESS_KEY_ID", &user)
            .env("AWS_SECRET_ACCESS_KEY", &secret)
            .env("AWS_EC2_METADATA_DISABLED", "true");
        cmd
    };
    run_chain(&lode_s3, &tmp.path().join("repo.recovery-key"), tmp.path());
}

fn backed_up(tmp: &Path) -> (PathBuf, PathBuf) {
    let (repo, src) = (tmp.join("repo"), tmp.join("src"));
    fs::create_dir(&src).unwrap();
    make_tree(&src);
    common::init_repo(&repo);
    lode(&repo).arg("backup").arg(&src).assert().success();
    (repo, src.canonicalize().unwrap())
}

#[test]
fn e2e_ls_subpath_and_latest() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = backed_up(tmp.path());
    let sub = src.join("a");

    let spec = format!("latest:{}", sub.display());
    let out = stdout_bytes(lode(&repo).args(["ls", "--json", &spec]));
    let listed: Vec<Vec<u8>> = json_lines(&out)
        .iter()
        .map(|l| path_bytes(&l["path"]))
        .collect();
    use std::os::unix::ffi::OsStrExt;
    let prefix = sub.as_os_str().as_bytes();
    assert!(
        listed.iter().all(|p| p.starts_with(prefix)),
        "only the subtree"
    );
    assert_eq!(listed[0], prefix, "the path itself comes first");
    let has = |p: PathBuf| listed.iter().any(|l| l == p.as_os_str().as_bytes());
    assert!(has(sub.join("script.sh")) && has(sub.join("b/random.bin")));
    assert!(!has(src.join("top.txt")));

    lode(&repo)
        .args(["ls", "latest:/no/such/path"])
        .assert()
        .code(1);
}

#[test]
fn e2e_dump_directory_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = backed_up(tmp.path());

    let out = lode(&repo)
        .args(["dump", "latest"])
        .arg(src.join("a"))
        .assert()
        .code(1);
    assert!(out.get_output().stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.get_output().stderr).contains("not a regular file"));
}
