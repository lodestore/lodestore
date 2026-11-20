use std::fs;
use std::path::Path;

use assert_cmd::Command;
use lodestore_format::store::{BlobError, BlobStore, RepoLocation};
use lodestore_testkit::azurite::AzuriteServer;

use crate::common::{assert_trees_equal, key_file_of, lode, make_tree, restored_path};

fn backup_and_restore(lode: &dyn Fn() -> Command, tmp: &Path) {
    let src = tmp.join("src");
    fs::create_dir(&src).unwrap();
    make_tree(&src);
    let src = src.canonicalize().unwrap();
    lode().arg("backup").arg(&src).assert().success();
    let out = tmp.join("restored");
    lode()
        .args(["restore", "latest", "--target"])
        .arg(&out)
        .assert()
        .success();
    assert_trees_equal(&src, &restored_path(&out, &src));
}

fn stderr_of(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stderr).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn cli_azblob_init_backup_restore() {
    let az = AzuriteServer::start().await;
    az.create_container("lodecli").await;
    let tmp = tempfile::tempdir().unwrap();
    let url = "azblob://lodecli";
    let lode_az = || {
        let mut cmd = lode(Path::new(url));
        cmd.env("LODE_AZBLOB_ACCOUNT_NAME", az.account_name())
            .env("LODE_AZBLOB_ACCOUNT_KEY", az.account_key())
            .env("LODE_AZBLOB_ENDPOINT", az.endpoint())
            .env_remove("LODE_AZBLOB_SAS_TOKEN");
        cmd
    };
    let key_file = key_file_of(&tmp.path().join("repo"));

    let refused = lode_az()
        .args(["repo", "init", "--recovery-key-file"])
        .arg(&key_file)
        .assert()
        .failure();
    let stderr = stderr_of(&refused);
    assert!(stderr.contains("--accept-weaker-guarantee"), "{stderr}");
    assert!(!stderr.contains(az.account_key()), "{stderr}");
    assert!(!key_file.exists(), "a refused init leaves no key file");
    let store = RepoLocation::new(
        "azblob",
        [
            ("container", "lodecli"),
            ("endpoint", az.endpoint().as_str()),
            ("account_name", az.account_name()),
            ("account_key", az.account_key()),
        ],
    )
    .open(None)
    .unwrap();
    assert!(
        matches!(store.get("config").await, Err(BlobError::NotFound)),
        "a refused init creates no config"
    );

    lode_az()
        .args([
            "repo",
            "init",
            "--accept-weaker-guarantee",
            "--recovery-key-file",
        ])
        .arg(&key_file)
        .assert()
        .success();
    backup_and_restore(&lode_az, tmp.path());
}
