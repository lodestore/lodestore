mod common;

use std::path::{Path, PathBuf};

use common::{PASSWORD, assert_trees_equal, init_repo, lode, make_tree, restored_path, stdout_of};
use lodestore_format::attest::{ensure_upgraded, put_attestation, seal_provenance};
use lodestore_format::crypto::{BlobId, Entropy};
use lodestore_format::index::parse_hex32;
use lodestore_format::pb;
use lodestore_format::repo::Repository;
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, Writer};

const YEAR_9999: i64 = 253_370_764_800;

fn backup(repo: &Path, src: &Path) -> BlobId {
    let saved = stdout_of(lode(repo).arg("backup").arg(src));
    let id = saved
        .strip_prefix("snapshot ")
        .and_then(|s| s.split_whitespace().next())
        .expect("backup prints the snapshot id");
    parse_hex32(id).unwrap()
}

fn record(id: BlobId, host: &str, job: &str) -> pb::ProvenanceRecord {
    pb::ProvenanceRecord {
        snapshot_id: id.to_vec(),
        host_id: host.into(),
        hostname: "h".into(),
        job_id: job.into(),
        verified_at: Some(pb::Timestamp {
            seconds: 1_700_000_000,
            nanos: 0,
        }),
    }
}

/// Upgrades the repository and signs `records` the way the server would.
async fn sign(repo: &Path, records: &[pb::ProvenanceRecord]) {
    let store = LocalFsStore::new(repo.to_path_buf());
    let repository = Repository::open(&store, PASSWORD.as_bytes()).await.unwrap();
    let keys = repository.read_keys();
    ensure_upgraded(&store, keys, &mut Entropy::os())
        .await
        .unwrap();
    let sealed = records
        .iter()
        .map(|r| seal_provenance(keys, r, &mut Entropy::os()).unwrap())
        .collect();
    put_attestation(&store, keys, &[], &[], sealed, &mut Entropy::os())
        .await
        .unwrap();
}

/// A snapshot written with the writer keys every backing-up host holds.
async fn plant_forged(repo: &Path, src: &Path) -> BlobId {
    let store = LocalFsStore::new(repo.to_path_buf());
    let repository = Repository::open(&store, PASSWORD.as_bytes()).await.unwrap();
    let keys = repository.read_keys().write_keys(repository.config_hash());
    let mut writer = Writer::new(&store, keys, repository.config())
        .await
        .unwrap();
    let options = BackupOptions {
        time: Some(pb::Timestamp {
            seconds: YEAR_9999,
            nanos: 0,
        }),
        ..Default::default()
    };
    writer
        .backup(&[src.to_path_buf()], options, &|_| {})
        .await
        .unwrap()
        .snapshot_id
}

struct Fx {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    src: PathBuf,
    forged_src: PathBuf,
    target: PathBuf,
}

fn fixture() -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let (repo, src, forged_src) = (root.join("repo"), root.join("src"), root.join("forged"));
    std::fs::create_dir(&src).unwrap();
    make_tree(&src);
    std::fs::create_dir(&forged_src).unwrap();
    std::fs::write(forged_src.join("evil"), b"forged").unwrap();
    init_repo(&repo);
    Fx {
        target: root.join("out"),
        _tmp: tmp,
        repo,
        src,
        forged_src,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_latest_strict_skips_forged_and_warns() {
    let fx = fixture();
    let honest = backup(&fx.repo, &fx.src);
    sign(&fx.repo, &[record(honest, "host", "job")]).await;
    plant_forged(&fx.repo, &fx.forged_src).await;

    let listed = lode(&fx.repo)
        .args(["ls", "latest"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&listed.stderr);
    assert!(stderr.contains("claim a time in the future"), "{stderr}");
    assert!(stderr.contains("unsigned snapshot"), "{stderr}");
    assert!(!String::from_utf8_lossy(&listed.stdout).contains("evil"));

    lode(&fx.repo)
        .args(["restore", "latest", "--target"])
        .arg(&fx.target)
        .assert()
        .success();
    assert_trees_equal(&fx.src, &restored_path(&fx.target, &fx.src));
    assert!(!restored_path(&fx.target, &fx.forged_src).exists());
}

#[test]
fn cli_latest_legacy_warns_on_v1_fixture() {
    let tmp = tempfile::tempdir().unwrap();
    let fixtures =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../lodestore-format/tests/fixtures/v1");
    let repo = tmp.path().join("repo");
    copy_dir(&fixtures.join("repo"), &repo);
    let password = std::fs::read_to_string(fixtures.join("PASSWORD")).unwrap();

    let out = assert_cmd::Command::cargo_bin("lode")
        .unwrap()
        .env("LODE_PASSWORD", password.trim_end())
        .env_remove("LODE_PASSWORD_FILE")
        .arg("-r")
        .arg(&repo)
        .args(["ls", "latest"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no server signatures yet"), "{stderr}");
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), dest).unwrap();
        }
    }
}
