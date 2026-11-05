#![allow(dead_code)]

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use assert_cmd::Command;

pub const PASSWORD: &str = "correct horse";

/// `lode -r <repo> ...` with the test password in the environment.
pub fn lode(repo: &Path) -> Command {
    let mut cmd = Command::cargo_bin("lode").unwrap();
    cmd.env("LODE_PASSWORD", PASSWORD)
        .env("XDG_CACHE_HOME", cache_dir_of(repo))
        .env_remove("LODE_PASSWORD_FILE")
        .arg("-r")
        .arg(repo);
    cmd
}

/// Where `lode` keeps its cache while running against `repo`.
pub fn cache_dir_of(repo: &Path) -> PathBuf {
    let mut name = repo.as_os_str().to_owned();
    name.push(".cache");
    PathBuf::from(name)
}

/// Path of the recovery key file that `init_repo` writes next to `repo`.
pub fn key_file_of(repo: &Path) -> PathBuf {
    let mut name = repo.as_os_str().to_owned();
    name.push(".recovery-key");
    PathBuf::from(name)
}

/// `lode repo init` with the key saved beside the repository; there is no
/// terminal in tests, so the init gate needs `--recovery-key-file`.
pub fn init_repo(repo: &Path) -> PathBuf {
    let key_file = key_file_of(repo);
    lode(repo)
        .args(["repo", "init", "--recovery-key-file"])
        .arg(&key_file)
        .assert()
        .success();
    key_file
}

pub fn stdout_of(cmd: &mut Command) -> String {
    let out = cmd.assert().success().get_output().stdout.clone();
    String::from_utf8(out).unwrap()
}

/// Deterministic incompressible bytes (splitmix64).
pub fn pseudo_random(len: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        out.extend_from_slice(&(z ^ (z >> 31)).to_le_bytes());
    }
    out.truncate(len);
    out
}

fn write_file(path: &Path, data: &[u8], mode: u32, mtime: u64) {
    fs::write(path, data).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::new(mtime, 123_456_789))
        .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn set_dir_mtime(path: &Path, secs: u64) {
    fs::File::open(path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::new(secs, 987_654_321))
        .unwrap();
}

/// Nested directories, a 3 MiB random file, a 5 MiB repeating file, an empty
/// directory, odd modes and (on Linux) a name that is not UTF-8.
pub fn make_tree(root: &Path) {
    fs::create_dir_all(root.join("a/b/c")).unwrap();
    fs::create_dir(root.join("empty")).unwrap();
    write_file(&root.join("top.txt"), b"hello", 0o644, 1_600_000_000);
    write_file(
        &root.join("a/script.sh"),
        b"#!/bin/sh\n",
        0o751,
        1_600_000_100,
    );
    write_file(
        &root.join("a/b/random.bin"),
        &pseudo_random(3 << 20),
        0o600,
        1_600_000_200,
    );
    let repeated: Vec<u8> = b"lodestore "
        .iter()
        .copied()
        .cycle()
        .take(5 << 20)
        .collect();
    write_file(
        &root.join("a/b/c/repeat.bin"),
        &repeated,
        0o640,
        1_600_000_300,
    );
    write_file(
        &root.join("a/b/c/ro.txt"),
        b"read only",
        0o400,
        1_600_000_400,
    );
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::OsStr::from_bytes(b"not-utf8-\xff\xfe");
        write_file(
            &root.join("a").join(name),
            b"odd name",
            0o644,
            1_600_000_500,
        );
    }
    for (dir, t) in [
        ("a/b/c", 1_600_001_000),
        ("a/b", 1_600_001_100),
        ("a", 1_600_001_200),
        ("empty", 1_600_001_300),
        ("", 1_600_001_400),
    ] {
        set_dir_mtime(&root.join(dir), t);
    }
}

/// Where a backed-up path ends up below a restore target.
pub fn restored_path(target: &Path, src: &Path) -> PathBuf {
    let canonical = src.canonicalize().unwrap();
    target.join(canonical.strip_prefix("/").unwrap())
}

/// Same names, types, bytes, permission bits and mtimes, recursively.
pub fn assert_trees_equal(a: &Path, b: &Path) {
    let (ma, mb) = (
        fs::symlink_metadata(a).unwrap(),
        fs::symlink_metadata(b).unwrap(),
    );
    assert_eq!(ma.file_type(), mb.file_type(), "{}", a.display());
    assert_eq!(
        ma.permissions().mode() & 0o7777,
        mb.permissions().mode() & 0o7777,
        "mode of {}",
        a.display()
    );
    assert_eq!(
        ma.modified().unwrap(),
        mb.modified().unwrap(),
        "mtime of {}",
        a.display()
    );
    if ma.is_dir() {
        let names = |p: &Path| {
            let mut v: Vec<OsString> = fs::read_dir(p)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            v.sort();
            v
        };
        let names_a = names(a);
        assert_eq!(names_a, names(b), "entries of {}", a.display());
        for n in names_a {
            assert_trees_equal(&a.join(&n), &b.join(&n));
        }
    } else {
        assert_eq!(
            fs::read(a).unwrap(),
            fs::read(b).unwrap(),
            "{}",
            a.display()
        );
    }
}

pub fn count_files(dir: &Path) -> usize {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .map(|p| if p.is_dir() { count_files(&p) } else { 1 })
        .sum()
}
