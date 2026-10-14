mod common;

use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use common::{
    assert_trees_equal, count_files, init_repo, lode, make_tree, restored_path, stdout_of,
};

fn snapshot_id(saved: &str) -> String {
    saved
        .strip_prefix("snapshot ")
        .and_then(|s| s.split_whitespace().next())
        .expect("backup prints the snapshot id")
        .to_owned()
}

fn make_second_tree(root: &Path) {
    fs::create_dir_all(root.join("other")).unwrap();
    let data: Vec<u8> = b"second tree "
        .iter()
        .copied()
        .cycle()
        .take(2 << 20)
        .collect();
    fs::write(root.join("other/data.bin"), data).unwrap();
    fs::write(root.join("note.txt"), b"another snapshot").unwrap();
}

fn clear_dir(dir: &Path) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            fs::remove_dir_all(&path).unwrap();
        } else {
            fs::remove_file(&path).unwrap();
        }
    }
}

fn pack_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            pack_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn rebuild_json(repo: &Path) -> (Option<i32>, serde_json::Value) {
    let out = lode(repo)
        .args(["--json", "repo", "rebuild-index"])
        .assert()
        .get_output()
        .clone();
    let line = String::from_utf8(out.stdout).unwrap();
    (
        out.status.code(),
        serde_json::from_str(line.trim()).expect("one JSON line"),
    )
}

#[test]
fn rebuild_index_after_index_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src1, src2) = (
        tmp.path().join("repo"),
        tmp.path().join("src1"),
        tmp.path().join("src2"),
    );
    fs::create_dir(&src1).unwrap();
    fs::create_dir(&src2).unwrap();
    make_tree(&src1);
    make_second_tree(&src2);
    init_repo(&repo);

    let id1 = snapshot_id(&stdout_of(lode(&repo).arg("backup").arg(&src1)));
    let id2 = snapshot_id(&stdout_of(lode(&repo).arg("backup").arg(&src2)));

    clear_dir(&repo.join("index"));
    let failed = lode(&repo)
        .args(["restore", &id1[..8], "--target"])
        .arg(tmp.path().join("lost"))
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&failed.get_output().stderr).into_owned();
    assert!(stderr.contains("rebuild-index"), "{stderr}");

    lode(&repo)
        .args(["repo", "rebuild-index"])
        .assert()
        .success();
    assert!(count_files(&repo.join("index")) > 0);

    // Before the restores below: comparing trees reads the sources, which moves
    // their atime on Linux and would give the re-backup new tree blobs.
    let packs = count_files(&repo.join("packs"));
    lode(&repo).arg("backup").arg(&src1).assert().success();
    assert_eq!(count_files(&repo.join("packs")), packs);

    for (id, src, name) in [(&id1, &src1, "out1"), (&id2, &src2, "out2")] {
        let target = tmp.path().join(name);
        lode(&repo)
            .args(["restore", &id[..8], "--target"])
            .arg(&target)
            .assert()
            .success();
        assert_trees_equal(src, &restored_path(&target, src));
    }
}

#[test]
fn rebuild_index_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    fs::create_dir(&src).unwrap();
    make_tree(&src);
    init_repo(&repo);
    lode(&repo).arg("backup").arg(&src).assert().success();
    clear_dir(&repo.join("index"));

    let (code, first) = rebuild_json(&repo);
    assert_eq!(code, Some(0));
    assert_eq!(first["type"], "rebuild_index");
    assert!(first["shards_written"].as_u64().unwrap() > 0);
    assert_eq!(first["packs_scanned"], first["packs_indexed"]);

    let shards = count_files(&repo.join("index"));
    let (code, second) = rebuild_json(&repo);
    assert_eq!(code, Some(0));
    assert_eq!(second["shards_written"], 0);
    assert_eq!(second["packs_scanned"], 0);
    assert_eq!(count_files(&repo.join("index")), shards);
}

#[test]
fn rebuild_index_skips_corrupt_pack() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, src) = (tmp.path().join("repo"), tmp.path().join("src"));
    fs::create_dir(&src).unwrap();
    make_tree(&src);
    init_repo(&repo);
    lode(&repo).arg("backup").arg(&src).assert().success();
    clear_dir(&repo.join("index"));

    let mut packs = Vec::new();
    pack_files(&repo.join("packs"), &mut packs);
    packs.sort();
    assert!(packs.len() >= 2, "data and tree packs are separate");
    let victim = &packs[0];
    let mut file = fs::OpenOptions::new().write(true).open(victim).unwrap();
    file.seek(SeekFrom::End(-6)).unwrap();
    file.write_all(b"BROKEN").unwrap();
    drop(file);

    let (code, report) = rebuild_json(&repo);
    assert_eq!(code, Some(1));
    let skipped = report["packs_skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 1);
    let victim_key = victim
        .strip_prefix(&repo)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(skipped[0]["key"], victim_key.as_str());
    assert_eq!(
        report["packs_scanned"].as_u64().unwrap(),
        packs.len() as u64
    );
    assert_eq!(
        report["packs_indexed"].as_u64().unwrap(),
        packs.len() as u64 - 1
    );
}
