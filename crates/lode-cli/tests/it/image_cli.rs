use crate::common::{PASSWORD, init_repo, lode, pseudo_random, restored_path, stdout_of};

use std::fs;
use std::path::{Path, PathBuf};

use lodestore_format::attest::{ensure_upgraded, read_attestations};
use lodestore_format::crypto::Entropy;
use lodestore_format::index::parse_hex32;
use lodestore_format::provenance::{ProvenanceSet, Signer};
use lodestore_format::repo::Repository;
use lodestore_format::source::{self, SourceType};
use lodestore_format::store::LocalFsStore;
use serde_json::Value;

const CHUNK: usize = 4 << 20;
const SEGMENT_CHUNKS: &str = "2";

/// Seven chunks (the last one 1 MiB short) in four two-chunk segments: holes
/// at the start, across a segment boundary and at the end, data around them.
fn write_sparse_image(path: &Path) {
    use std::os::unix::fs::FileExt;
    let file = fs::File::create(path).unwrap();
    file.set_len((6 * CHUNK + (1 << 20)) as u64).unwrap();
    for chunk in [1, 4, 5] {
        let data = pseudo_random(100 << 10);
        file.write_all_at(&data, (chunk * CHUNK + 4096) as u64)
            .unwrap();
    }
}

fn backup_image_json(repo: &Path, image: &Path, name: Option<&str>) -> Value {
    let mut cmd = lode(repo);
    cmd.env("LODE_TEST_IMAGE_SEGMENT_CHUNKS", SEGMENT_CHUNKS)
        .args(["backup", "--json", "--image"])
        .arg(image);
    if let Some(name) = name {
        cmd.args(["--name", name]);
    }
    stdout_of(&mut cmd)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["type"] == "backup_summary")
        .expect("a backup_summary line")
}

fn snapshot_of(summary: &Value) -> String {
    summary["snapshot_id"].as_str().unwrap().to_owned()
}

fn restore_image(repo: &Path, spec: &str, target: &Path) {
    lode(repo)
        .args(["restore", spec, "--target"])
        .arg(target)
        .assert()
        .success();
}

fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let image = tmp.path().join("disk.raw");
    write_sparse_image(&image);
    (tmp, repo, image)
}

#[test]
fn image_backup_restore_file_roundtrip() {
    let (tmp, repo, image) = setup();
    let summary = backup_image_json(&repo, &image, Some("disk"));
    assert_eq!(summary["image"]["key"], "image/disk");
    assert_eq!(summary["image"]["read_mode"], "full");
    assert_eq!(
        summary["image"]["size"].as_u64().unwrap(),
        fs::metadata(&image).unwrap().len()
    );
    let id = snapshot_of(&summary);

    let listing = stdout_of(lode(&repo).args(["ls", &format!("{id}:/disks/disk")]));
    for segment in ["000000", "000001", "000002", "000003"] {
        assert!(listing.contains(segment), "{listing}");
    }
    assert!(!listing.contains("000004"), "{listing}");

    let out = tmp.path().join("out");
    restore_image(&repo, &id, &out);
    assert_eq!(
        fs::read(out.join("disk.img")).unwrap(),
        fs::read(&image).unwrap()
    );
    #[cfg(target_os = "linux")]
    assert_holes_stay_holes(&out.join("disk.img"));
}

#[cfg(target_os = "linux")]
fn assert_holes_stay_holes(path: &Path) {
    use lodestore_format::fs::walk::data_extents;
    let file = fs::File::open(path).unwrap();
    let size = file.metadata().unwrap().len();
    let extents = data_extents(&file.into(), size).expect("a sparse restore");
    let in_hole = |chunk: usize| {
        let (from, to) = (chunk * CHUNK, ((chunk + 1) * CHUNK).min(size as usize));
        extents
            .iter()
            .any(|&(o, l)| (o as usize) < to && (o + l) as usize > from)
    };
    for chunk in [0, 2, 3, 6] {
        assert!(!in_hole(chunk), "chunk {chunk} holds data");
    }
}

#[test]
fn image_name_rules() {
    let (_tmp, repo, image) = setup();
    for name in ["a/b", "..", &"n".repeat(201)] {
        lode(&repo)
            .args(["backup", "--image"])
            .arg(&image)
            .args(["--name", name])
            .assert()
            .code(2);
    }
    lode(&repo)
        .args(["backup", "--image", "nbd+unix:///x"])
        .assert()
        .code(2);
    lode(&repo)
        .args(["backup", "--name", "x"])
        .arg(&image)
        .assert()
        .code(2);
    let summary = backup_image_json(&repo, &image, None);
    assert_eq!(summary["image"]["key"], "image/disk.raw");
}

fn small_image(path: &Path, fill: u8) {
    use std::os::unix::fs::FileExt;
    let file = fs::File::create(path).unwrap();
    file.set_len((2 * CHUNK + 4096) as u64).unwrap();
    file.write_all_at(&[fill; 8192], CHUNK as u64).unwrap();
}

async fn open_repo(repo: &Path) -> (LocalFsStore, Repository) {
    let store = LocalFsStore::new(repo.to_path_buf());
    let repository = Repository::open(&store, PASSWORD.as_bytes()).await.unwrap();
    (store, repository)
}

async fn upgrade(repo: &Path) {
    let (store, repository) = open_repo(repo).await;
    ensure_upgraded(&store, repository.read_keys(), &mut Entropy::os())
        .await
        .unwrap();
}

async fn provenance(repo: &Path) -> ProvenanceSet {
    let (store, repository) = open_repo(repo).await;
    let set = read_attestations(&store, repository.read_keys())
        .await
        .unwrap();
    ProvenanceSet::from_attested(&set, repository.read_keys())
}

fn restored_image(repo: &Path, spec: &str, target: &Path, file: &str) -> Vec<u8> {
    restore_image(repo, spec, target);
    fs::read(target.join(file)).unwrap()
}

fn files_backup(repo: &Path, tmp: &Path) -> PathBuf {
    let src = tmp.join("src");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("f"), b"just a file").unwrap();
    lode(repo).arg("backup").arg(&src).assert().success();
    src
}

#[tokio::test(flavor = "multi_thread")]
async fn image_backup_signs_its_source_in_an_upgraded_repo() {
    let (tmp, repo, image) = setup();
    upgrade(&repo).await;
    let id = snapshot_of(&backup_image_json(&repo, &image, Some("disk")));

    let set = provenance(&repo).await;
    let id_bytes = parse_hex32(&id).unwrap();
    assert_eq!(set.signer(&id_bytes), Some(Signer::Owner));
    assert_eq!(set.source_of(&id_bytes).as_str(), "image/disk");
    let record = set.get(&id_bytes).unwrap();
    let (key, _) = source::decode(record.source.as_ref()).unwrap();
    assert_eq!(key.kind(), SourceType::Image);

    let out = tmp.path().join("out");
    assert_eq!(
        restored_image(&repo, "latest@image/disk", &out, "disk.img"),
        fs::read(&image).unwrap()
    );
    stdout_of(lode(&repo).args(["snapshots", "--source-type", "image", "--json"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn image_latest_picks_its_own_instance() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    upgrade(&repo).await;
    let src = files_backup(&repo, tmp.path());
    let image = tmp.path().join("disk.raw");
    let other = tmp.path().join("other.raw");
    small_image(&image, 1);
    backup_image_json(&repo, &image, Some("disk"));
    small_image(&image, 2);
    backup_image_json(&repo, &image, Some("disk"));
    small_image(&other, 3);
    backup_image_json(&repo, &other, Some("other"));

    let disk_out = tmp.path().join("disk-out");
    assert_eq!(
        restored_image(&repo, "latest@image/disk", &disk_out, "disk.img"),
        fs::read(&image).unwrap()
    );
    let other_out = tmp.path().join("other-out");
    assert_eq!(
        restored_image(&repo, "latest@image/other", &other_out, "other.img"),
        fs::read(&other).unwrap()
    );
    let files_out = tmp.path().join("files-out");
    restore_image(&repo, "latest", &files_out);
    assert!(!files_out.join("disk.img").exists());
    assert_eq!(
        fs::read(restored_path(&files_out, &src).join("f")).unwrap(),
        b"just a file"
    );
}

#[test]
fn image_snapshot_is_not_files_latest() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let src = files_backup(&repo, tmp.path());
    let image = tmp.path().join("disk.raw");
    small_image(&image, 7);
    let id = snapshot_of(&backup_image_json(&repo, &image, Some("disk")));

    let files_out = tmp.path().join("files-out");
    restore_image(&repo, "latest", &files_out);
    assert!(!files_out.join("disk.img").exists());
    assert!(restored_path(&files_out, &src).join("f").exists());

    let denied = lode(&repo)
        .args(["restore", "latest@image/disk", "--target"])
        .arg(tmp.path().join("denied"))
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&denied.get_output().stderr).into_owned();
    assert!(
        stderr.contains("No snapshot in this repository is signed"),
        "{stderr}"
    );

    let out = tmp.path().join("out");
    let assert = lode(&repo)
        .args(["restore", &id, "--target"])
        .arg(&out)
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(!stderr.contains("warning"), "{stderr}");
    assert_eq!(
        fs::read(out.join("disk.img")).unwrap(),
        fs::read(&image).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn image_unsigned_snapshot_warns_in_a_signed_repo() {
    let (tmp, repo, image) = setup();
    let id = snapshot_of(&backup_image_json(&repo, &image, Some("disk")));
    upgrade(&repo).await;

    let out = tmp.path().join("out");
    let assert = lode(&repo)
        .args(["restore", &id, "--target"])
        .arg(&out)
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("warning: nothing signed snapshot"),
        "{stderr}"
    );
    assert_eq!(
        fs::read(out.join("disk.img")).unwrap(),
        fs::read(&image).unwrap()
    );
}

#[test]
fn image_restore_path_gives_segments() {
    let (tmp, repo, image) = setup();
    let id = snapshot_of(&backup_image_json(&repo, &image, Some("disk")));

    let out = tmp.path().join("out");
    restore_image(&repo, &format!("{id}:/disks/disk"), &out);
    assert!(!out.join("disk.img").exists());
    let mut segments = Vec::new();
    let mut stack = vec![out];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                segments.push(path);
            }
        }
    }
    segments.sort();
    assert_eq!(segments.len(), 4);
    let joined: Vec<u8> = segments.iter().flat_map(|p| fs::read(p).unwrap()).collect();
    assert_eq!(joined, fs::read(&image).unwrap());
}

#[test]
fn image_restore_by_id_survives_unreadable_signatures() {
    use std::os::unix::fs::PermissionsExt;
    let (tmp, repo, image) = setup();
    let id = snapshot_of(&backup_image_json(&repo, &image, Some("disk")));
    let locks = repo.join("locks");
    fs::create_dir_all(&locks).unwrap();
    fs::set_permissions(&locks, fs::Permissions::from_mode(0o000)).unwrap();
    // shortcut: root ignores the mode bits, so there is nothing to test there.
    if fs::read_dir(&locks).is_ok() {
        fs::set_permissions(&locks, fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    let out = tmp.path().join("out");
    let assert = lode(&repo)
        .args(["restore", &id, "--target"])
        .arg(&out)
        .assert();
    fs::set_permissions(&locks, fs::Permissions::from_mode(0o755)).unwrap();
    let assert = assert.success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("could not read snapshot signatures"),
        "{stderr}"
    );
    assert_eq!(
        fs::read(out.join("disk.img")).unwrap(),
        fs::read(&image).unwrap()
    );
}
