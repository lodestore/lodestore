use crate::common::{init_repo, lode, pseudo_random, stdout_of};

use std::fs;
use std::path::{Path, PathBuf};

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
