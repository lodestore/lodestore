mod common;

use std::path::{Path, PathBuf};

use common::{PASSWORD, init_repo, lode, pseudo_random, stdout_of};
use lodestore_format::crypto::Entropy;
use lodestore_format::index::parse_hex32;
use lodestore_format::locks::{new_lock_id, publish_gc_manifest};
use lodestore_format::repo::Repository;
use lodestore_format::store::{BlobStore, LocalFsStore};
use lodestore_testkit::minio::MinioServer;
use serde_json::Value;

fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("random.bin"), pseudo_random(2 << 20)).unwrap();
    init_repo(&repo);
    (tmp, repo, src)
}

fn lock_names(repo: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(repo.join("locks"))
        .map(|dir| {
            dir.map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn assert_closed_pair(names: &[String]) {
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names[0].starts_with("writer-") && !names[0].ends_with(".done"));
    assert_eq!(names[1], format!("{}.done", names[0]));
}

fn bytes_added(cmd: &mut assert_cmd::Command) -> u64 {
    let out = stdout_of(cmd);
    let summary: Value = out
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["type"] == "backup_summary")
        .expect("a backup_summary line");
    summary["stats"]["bytes_added"].as_u64().unwrap()
}

#[test]
fn gc_writer_marker_closed_after_backup() {
    let (_tmp, repo, src) = setup();
    lode(&repo).arg("backup").arg(&src).assert().success();
    assert_closed_pair(&lock_names(&repo));
}

#[test]
fn gc_writer_marker_closed_after_failure() {
    let (_tmp, repo, src) = setup();
    lode(&repo)
        .args(["backup", "--parent", "deadbeef"])
        .arg(&src)
        .assert()
        .failure();
    assert_closed_pair(&lock_names(&repo));
}

#[tokio::test(flavor = "multi_thread")]
async fn gc_writer_marker_excludes_candidates() {
    let (_tmp, repo, src) = setup();
    let rescan = ["backup", "--json", "--force-rescan"];
    lode(&repo).arg("backup").arg(&src).assert().success();
    assert_eq!(
        bytes_added(lode(&repo).args(rescan).arg(&src)),
        0,
        "unmarked packs are reused"
    );

    let store = LocalFsStore::new(repo.clone());
    let packs: Vec<_> = store
        .list("packs/")
        .await
        .unwrap()
        .iter()
        .map(|e| parse_hex32(e.key.rsplit('/').next().unwrap()).unwrap())
        .collect();
    let opened = Repository::open(&store, PASSWORD.as_bytes()).await.unwrap();
    let keys = opened.read_keys();
    let mut entropy = Entropy::os();
    publish_gc_manifest(
        &store,
        keys.k_idx(),
        keys.repo_id(),
        &new_lock_id(&mut entropy),
        &packs,
        &mut entropy,
    )
    .await
    .unwrap();

    assert!(
        bytes_added(lode(&repo).args(rescan).arg(&src)) > 0,
        "marked packs are uploaded again"
    );
}

fn export_writer_keys(repo: &str, envs: &[(&str, &str)], out: &Path) {
    let mut cmd = lode(Path::new(repo));
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.args(["key", "export-writer", "--output"])
        .arg(out)
        .assert()
        .success();
}

struct S3Case {
    _minio: MinioServer,
    url: String,
    keys: PathBuf,
    src: PathBuf,
    user: (String, String),
    _tmp: tempfile::TempDir,
}

async fn s3_case(policy: impl Fn(&str) -> String) -> S3Case {
    let minio = MinioServer::start().await;
    minio.create_bucket("marker").await;
    let url = format!("s3:{}/marker", minio.endpoint());
    let (root_user, root_secret) = minio.root_credentials();
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("file"), b"hello").unwrap();
    let root_env = [
        ("AWS_ACCESS_KEY_ID", root_user.as_str()),
        ("AWS_SECRET_ACCESS_KEY", root_secret.as_str()),
        ("AWS_EC2_METADATA_DISABLED", "true"),
    ];
    let key_file = tmp.path().join("recovery-key");
    let mut init = lode(Path::new(&url));
    for (k, v) in root_env {
        init.env(k, v);
    }
    init.args(["repo", "init", "--recovery-key-file"])
        .arg(&key_file)
        .assert()
        .success();
    let keys = tmp.path().join("writer-keys");
    export_writer_keys(&url, &root_env, &keys);
    let user = minio
        .create_user_with_policy("marker", &policy("marker"))
        .await;
    S3Case {
        _minio: minio,
        url,
        keys,
        src,
        user,
        _tmp: tmp,
    }
}

impl S3Case {
    fn backup(&self, extra: &[&str]) -> assert_cmd::Command {
        let mut cmd = lode(Path::new(&self.url));
        cmd.env_remove("LODE_PASSWORD")
            .env("AWS_ACCESS_KEY_ID", &self.user.0)
            .env("AWS_SECRET_ACCESS_KEY", &self.user.1)
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .arg("--writer-keys")
            .arg(&self.keys)
            .arg("backup")
            .args(extra)
            .arg(&self.src);
        cmd
    }
}

fn policy_without_locks(bucket: &str) -> String {
    let arn = |path: &str| format!("arn:aws:s3:::{bucket}/{path}");
    serde_json::json!({
        "Version": "2012-10-17",
        "Statement": [
            {"Effect": "Allow", "Action": ["s3:PutObject"],
             "Resource": [arn("packs/*"), arn("index/*"), arn("snapshots/*")]},
            {"Effect": "Allow", "Action": ["s3:GetObject"],
             "Resource": [arn("config"), arn("index/*")]},
            {"Effect": "Allow", "Action": ["s3:ListBucket"],
             "Resource": [format!("arn:aws:s3:::{bucket}")],
             "Condition": {"StringLike": {"s3:prefix": ["index/*", "index/"]}}},
        ],
    })
    .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn gc_writer_marker_denied_s3() {
    let case = s3_case(|b| lodestore_testkit::minio::write_only_policy(b).to_string()).await;
    let out = case.backup(&[]).assert().success();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(
        stderr.contains("cannot write the cleanup marker"),
        "{stderr}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn gc_writer_marker_locks_unreadable_s3() {
    let case = s3_case(policy_without_locks).await;
    let out = case.backup(&[]).assert().code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(stderr.contains("--no-gc-check"), "{stderr}");
    case.backup(&["--no-gc-check"]).assert().success();
}
