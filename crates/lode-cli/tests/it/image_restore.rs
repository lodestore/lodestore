use crate::common::{PASSWORD, init_repo, lode, pseudo_random, stdout_of};

use std::fs;
use std::path::{Path, PathBuf};

use lodestore_format::attest::ensure_upgraded;
use lodestore_format::crypto::Entropy;
use lodestore_format::repo::Repository;
use lodestore_format::store::LocalFsStore;
use serde_json::Value;

const MIB: usize = 1 << 20;
const CHUNK: usize = 4 * MIB;
const IMAGE_LEN: usize = 5 * CHUNK;

/// Five chunks in three two-chunk segments: holes at the start, in the middle
/// and at the end, data in chunks 1 and 3.
fn write_image_with_holes(path: &Path) {
    use std::os::unix::fs::FileExt;
    let file = fs::File::create(path).unwrap();
    file.set_len(IMAGE_LEN as u64).unwrap();
    for chunk in [1, 3] {
        file.write_all_at(&pseudo_random(100 << 10), (chunk * CHUNK + 4096) as u64)
            .unwrap();
    }
}

fn backup_image(repo: &Path, image: &Path) -> String {
    let out = stdout_of(
        lode(repo)
            .env("LODE_TEST_IMAGE_SEGMENT_CHUNKS", "2")
            .args(["backup", "--json", "--image"])
            .arg(image)
            .args(["--name", "disk"]),
    );
    out.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["type"] == "backup_summary")
        .expect("a backup_summary line")["snapshot_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

struct Fx {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    image: PathBuf,
    device: PathBuf,
    id: String,
}

fn setup(device_len: usize) -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let image = tmp.path().join("disk.raw");
    write_image_with_holes(&image);
    let id = backup_image(&repo, &image);
    let device = tmp.path().join("device");
    fs::write(&device, vec![0xFFu8; device_len]).unwrap();
    Fx {
        _tmp: tmp,
        repo,
        image,
        device,
        id,
    }
}

impl Fx {
    fn to_device(&self) -> assert_cmd::Command {
        let mut cmd = lode(&self.repo);
        cmd.env("LODE_TEST_DEVICE_IS_FILE", "1")
            .args(["restore", &self.id, "--to-device"])
            .arg(&self.device);
        cmd
    }

    fn assert_device_written(&self) {
        let written = fs::read(&self.device).unwrap();
        assert_eq!(written.len(), 8 * CHUNK);
        assert_eq!(written[..IMAGE_LEN], fs::read(&self.image).unwrap()[..]);
        assert!(written[IMAGE_LEN..].iter().all(|&b| b == 0xFF));
    }
}

fn stderr_of(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stderr).into_owned()
}

#[test]
fn image_to_device_writes_holes_as_zeros() {
    let fx = setup(8 * CHUNK);
    let out = stdout_of(fx.to_device().arg("--json"));
    let summary = out
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["type"] == "restore_summary")
        .expect("a restore_summary line");
    assert_eq!(summary["files_restored"], 1);
    assert_eq!(summary["bytes_restored"], IMAGE_LEN as u64);
    fx.assert_device_written();
}

#[test]
fn image_to_device_refuses_a_smaller_device() {
    let fx = setup(2 * CHUNK);
    let assert = fx.to_device().assert().failure();
    let stderr = stderr_of(&assert);
    assert!(
        stderr.contains("holds") && stderr.contains("needs"),
        "{stderr}"
    );
    assert!(!stderr.contains("partial"), "{stderr}");
    assert_eq!(fs::read(&fx.device).unwrap(), vec![0xFFu8; 2 * CHUNK]);
}

#[test]
fn image_to_device_refuses_a_regular_file() {
    let fx = setup(8 * CHUNK);
    let assert = lode(&fx.repo)
        .args(["restore", &fx.id, "--to-device"])
        .arg(&fx.device)
        .assert()
        .failure();
    let stderr = stderr_of(&assert);
    let reason = if cfg!(target_os = "linux") {
        "not a block device"
    } else {
        "Linux only"
    };
    assert!(stderr.contains(reason), "{stderr}");
    assert_eq!(fs::read(&fx.device).unwrap(), vec![0xFFu8; 8 * CHUNK]);
}

#[test]
fn image_to_device_needs_an_image_snapshot() {
    let fx = setup(8 * CHUNK);
    let src = fx._tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("f"), b"just a file").unwrap();
    lode(&fx.repo).arg("backup").arg(&src).assert().success();
    lode(&fx.repo)
        .env("LODE_TEST_DEVICE_IS_FILE", "1")
        .args(["restore", "latest", "--to-device"])
        .arg(&fx.device)
        .assert()
        .code(2);
    lode(&fx.repo)
        .env("LODE_TEST_DEVICE_IS_FILE", "1")
        .args(["restore", &format!("{}:/disks/disk", fx.id), "--to-device"])
        .arg(&fx.device)
        .assert()
        .code(2);
    assert_eq!(fs::read(&fx.device).unwrap(), vec![0xFFu8; 8 * CHUNK]);
}

#[test]
fn image_to_device_conflicts_with_other_modes() {
    let fx = setup(8 * CHUNK);
    for extra in [
        &["--target", "/nowhere"][..],
        &["--verify"],
        &["--dry-run"],
        &["--include", "/a"],
        &["--overwrite", "always"],
    ] {
        fx.to_device().args(extra).assert().code(2);
    }
    lode(&fx.repo)
        .args([
            "restore",
            &fx.id,
            "--accept-unverified",
            "--target",
            "/nowhere",
        ])
        .assert()
        .code(2);
    lode(&fx.repo).args(["restore", &fx.id]).assert().code(2);
    assert_eq!(fs::read(&fx.device).unwrap(), vec![0xFFu8; 8 * CHUNK]);
}

#[tokio::test(flavor = "multi_thread")]
async fn image_to_device_refuses_an_unsigned_snapshot_without_the_flag() {
    let fx = setup(8 * CHUNK);
    let store = LocalFsStore::new(fx.repo.clone());
    let repository = Repository::open(&store, PASSWORD.as_bytes()).await.unwrap();
    ensure_upgraded(&store, repository.read_keys(), &mut Entropy::os())
        .await
        .unwrap();

    let assert = fx.to_device().assert().failure();
    assert!(
        stderr_of(&assert).contains("--accept-unverified"),
        "{}",
        stderr_of(&assert)
    );
    assert_eq!(fs::read(&fx.device).unwrap(), vec![0xFFu8; 8 * CHUNK]);

    fx.to_device().arg("--accept-unverified").assert().success();
    fx.assert_device_written();
}

#[test]
fn image_to_device_in_an_unsigned_repo_needs_no_flag() {
    let fx = setup(8 * CHUNK);
    fx.to_device().assert().success();
    fx.assert_device_written();
}

#[test]
fn image_restore_verify_passes_on_an_image() {
    let fx = setup(8 * CHUNK);
    let out = fx._tmp.path().join("out");
    let stdout = stdout_of(
        lode(&fx.repo)
            .args(["restore", &fx.id, "--verify", "--json", "--target"])
            .arg(&out),
    );
    let summary = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["type"] == "restore_summary")
        .expect("a restore_summary line");
    assert_eq!(summary["files_verified"], 1);
    assert_eq!(
        fs::read(out.join("disk.img")).unwrap(),
        fs::read(&fx.image).unwrap()
    );
}

#[test]
fn image_check_restore_level_has_no_false_mismatch() {
    let fx = setup(8 * CHUNK);
    let src = fx._tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("f"), pseudo_random(300 << 10)).unwrap();
    lode(&fx.repo).arg("backup").arg(&src).assert().success();

    let text = stdout_of(lode(&fx.repo).args(["check", "--level", "restore", "--files", "50"]));
    assert!(text.contains("Result            Passed"), "{text}");
    assert!(text.contains("Hash mismatches   0"), "{text}");
    assert!(!text.contains("restore_mismatch"), "{text}");
}
