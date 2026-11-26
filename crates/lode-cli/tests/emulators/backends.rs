use std::fs;
use std::path::Path;

use assert_cmd::Command;
use lodestore_format::store::{BlobError, BlobStore, RepoLocation};
use lodestore_testkit::azurite::AzuriteServer;
use lodestore_testkit::gcs::FakeGcsServer;
use lodestore_testkit::sshd::{SshUser, SshdServer};

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

fn init_with_consent(lode: &dyn Fn() -> Command, tmp: &Path) {
    lode()
        .args([
            "repo",
            "init",
            "--accept-weaker-guarantee",
            "--recovery-key-file",
        ])
        .arg(key_file_of(&tmp.join("repo")))
        .assert()
        .success();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn cli_gcs_init_backup_restore() {
    let gcs = FakeGcsServer::start().await;
    gcs.create_bucket("lodecli").await;
    let tmp = tempfile::tempdir().unwrap();
    let lode_gcs = || {
        let mut cmd = lode(Path::new("gcs://lodecli"));
        cmd.env("LODE_GCS_ENDPOINT", gcs.endpoint())
            .env_remove("LODE_GCS_CREDENTIAL_FILE");
        cmd
    };
    init_with_consent(&lode_gcs, tmp.path());
    backup_and_restore(&lode_gcs, tmp.path());
}

struct Sftp {
    sshd: SshdServer,
    keys: tempfile::TempDir,
}

impl Sftp {
    async fn start() -> Self {
        let sshd = SshdServer::start().await;
        let keys = tempfile::tempdir().unwrap();
        for user in [SshUser::Srv, SshUser::NoLink] {
            fs::write(keys.path().join(user.login()), sshd.private_key(user)).unwrap();
        }
        Sftp { sshd, keys }
    }

    fn url(&self, user: SshUser) -> String {
        format!(
            "sftp://{}@{}{}",
            user.login(),
            self.sshd.endpoint(),
            self.sshd.root()
        )
    }

    fn lode(&self, user: SshUser) -> Command {
        let mut cmd = lode(Path::new(&self.url(user)));
        cmd.env("LODE_SFTP_KEY_FILE", self.keys.path().join(user.login()))
            .env("LODE_SFTP_HOST_KEY", self.sshd.host_key())
            .env_remove("LODE_SFTP_PUBLISH");
        cmd
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn cli_sftp_init_backup_restore() {
    let sftp = Sftp::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let lode_sftp = || sftp.lode(SshUser::Srv);
    init_with_consent(&lode_sftp, tmp.path());
    backup_and_restore(&lode_sftp, tmp.path());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn cli_sftp_direct_init_on_an_empty_root() {
    let sftp = Sftp::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let lode_direct = || {
        let mut cmd = sftp.lode(SshUser::Srv);
        cmd.env("LODE_SFTP_PUBLISH", "direct");
        cmd
    };
    init_with_consent(&lode_direct, tmp.path());
    backup_and_restore(&lode_direct, tmp.path());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn cli_sftp_direct_init_backup_restore() {
    let sftp = Sftp::start().await;
    let tmp = tempfile::tempdir().unwrap();
    init_with_consent(&|| sftp.lode(SshUser::Srv), tmp.path());
    let lode_direct = || {
        let mut cmd = sftp.lode(SshUser::NoLink);
        cmd.env("LODE_SFTP_PUBLISH", "direct");
        cmd
    };
    let key = sftp.sshd.private_key(SshUser::Srv);
    let endpoint = sftp.sshd.endpoint();
    let owner = RepoLocation::new(
        "sftp",
        [
            ("endpoint", endpoint.as_str()),
            ("user", SshUser::Srv.login()),
            ("key", key.as_str()),
            ("host_key", sftp.sshd.host_key().as_str()),
            ("root", sftp.sshd.root()),
        ],
    )
    .open(None)
    .unwrap();
    owner
        .ensure_prefixes(&["packs", "index", "snapshots", "locks"])
        .await
        .unwrap();
    let chmod = std::process::Command::new("docker")
        .args(["exec", sftp.sshd.container(), "chmod", "2775"])
        .args(["/srv/repo/data/index", "/srv/repo/data/snapshots"])
        .status()
        .unwrap();
    assert!(chmod.success());
    backup_and_restore(&lode_direct, tmp.path());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn cli_sftp_without_host_key_refuses_before_connecting() {
    let sftp = Sftp::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let mark = sftp.sshd.log().len();
    let refused = sftp
        .lode(SshUser::Srv)
        .env_remove("LODE_SFTP_HOST_KEY")
        .args([
            "repo",
            "init",
            "--accept-weaker-guarantee",
            "--recovery-key-file",
        ])
        .arg(key_file_of(&tmp.path().join("repo")))
        .assert()
        .failure();
    let stderr = stderr_of(&refused);
    assert!(stderr.contains("LODE_SFTP_HOST_KEY"), "{stderr}");
    assert!(stderr.contains("ssh-keyscan"), "{stderr}");
    let log = sftp.sshd.log();
    assert!(
        !log.get(mark..).unwrap_or("").contains("Connection from"),
        "lode connected without a pinned host key"
    );
}
