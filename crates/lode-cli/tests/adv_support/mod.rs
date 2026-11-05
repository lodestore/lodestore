#![allow(dead_code)]

//! The committed repository that was upgraded for server signatures: password
//! `common::PASSWORD`, a cheap key derivation, one index shard planted before
//! the upgrade that names `lying_content()` in a pack that holds no such blob,
//! and the attestations that sign it. Only std, so the scenarios compile
//! against the first release too.

use std::fs;
use std::path::{Path, PathBuf};

pub fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/adv_support/upgraded-repo")
}

/// Copies the fixture to `dst/repo` and returns that path.
pub fn upgraded_repo(dst: &Path) -> PathBuf {
    let repo = dst.join("repo");
    copy_dir(&fixture_dir(), &repo);
    for name in ["keys", "index", "packs", "locks", "snapshots"] {
        fs::create_dir_all(repo.join(name)).unwrap();
    }
    repo
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), dest).unwrap();
        }
    }
}

/// 4096 bytes, below the smallest chunk, so a file with this content is one blob.
pub fn lying_content() -> Vec<u8> {
    (0..4096u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect()
}
