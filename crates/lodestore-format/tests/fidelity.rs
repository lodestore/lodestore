//! Metadata fidelity: what a restore reproduces besides the bytes. Linux only;
//! on a macOS host run it through `scripts/test-linux.sh` (add `--root` for the
//! tests that need root).
#![cfg(target_os = "linux")]

use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use lodestore_format::crypto::{BLOB_TYPE_TREE, Entropy};
use lodestore_format::keys::KdfCost;
use lodestore_format::pb;
use lodestore_format::reader::{Reader, RestoreOptions, RestoreReport};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::store::LocalFsStore;
use lodestore_format::tree::decode_tree;
use lodestore_format::writer::{BackupOptions, BackupOutcome, Writer};
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{Rng, SeedableRng};
use rustix::fs::{AtFlags, CWD, FileType, Mode, Timespec, Timestamps, mknodat, utimensat};

const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

struct Rt {
    store: LocalFsStore,
    outcome: BackupOutcome,
    report: RestoreReport,
    target: PathBuf,
}

impl Rt {
    /// Nothing below `src` went unapplied. The directories above it exist only
    /// to hold the path, so their owner (root's, for a non-root restore) is
    /// reported but not judged here.
    fn assert_clean(&self, src: &Path) {
        let below = src.canonicalize().unwrap();
        let below = below.strip_prefix("/").unwrap().as_os_str().as_bytes();
        let bad: Vec<_> = self
            .report
            .unapplied
            .iter()
            .filter(|u| u.path.starts_with(below))
            .collect();
        assert!(bad.is_empty(), "{bad:?}");
    }

    /// Where a backed-up path ends up below the restore target.
    fn at(&self, src: &Path) -> PathBuf {
        self.target
            .join(src.canonicalize().unwrap().strip_prefix("/").unwrap())
    }
}

async fn open_reader(store: &LocalFsStore) -> Reader<&LocalFsStore> {
    let repo = Repository::open(store, b"pw").await.unwrap();
    Reader::open(store, repo.read_keys().duplicate(), repo.config()).unwrap()
}

/// Backs up `paths` into a fresh repository below `tmp`.
async fn backup(tmp: &Path, paths: &[PathBuf]) -> (LocalFsStore, BackupOutcome) {
    let repo_dir = tmp.join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    let store = LocalFsStore::new(repo_dir);
    let mut entropy = Entropy::os();
    PendingInit::generate(
        &mut entropy,
        InitOptions {
            kdf: LOW_KDF,
            ..Default::default()
        },
    )
    .unwrap()
    .commit(&store, b"pw", &mut entropy)
    .await
    .unwrap();

    let repo = Repository::open(&store, b"pw").await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    let mut w = Writer::new(&store, keys, repo.config()).await.unwrap();
    let outcome = w
        .backup(paths, BackupOptions::default(), &|_| {})
        .await
        .unwrap();
    assert!(!outcome.incomplete, "backup reported errors");
    (store, outcome)
}

async fn restore(
    store: &LocalFsStore,
    outcome: &BackupOutcome,
    target: &Path,
) -> lodestore_format::Result<RestoreReport> {
    let mut r = open_reader(store).await;
    r.restore(
        &outcome.snapshot_id,
        target,
        &RestoreOptions::default(),
        &|_| {},
    )
    .await
}

/// Backs up `paths` and restores the snapshot into `tmp/out`.
async fn backup_restore(tmp: &Path, paths: &[PathBuf]) -> Rt {
    let (store, outcome) = backup(tmp, paths).await;
    let target = tmp.join("out");
    let report = restore(&store, &outcome, &target).await.unwrap();
    Rt {
        store,
        outcome,
        report,
        target,
    }
}

fn own_uid() -> u32 {
    fs::metadata("/proc/self").unwrap().uid()
}

fn mtime(p: &Path) -> (i64, i64) {
    let m = fs::symlink_metadata(p).unwrap();
    (m.mtime(), m.mtime_nsec())
}

fn atime(p: &Path) -> (i64, i64) {
    let m = fs::symlink_metadata(p).unwrap();
    (m.atime(), m.atime_nsec())
}

fn set_times(p: &Path, a: (i64, i64), m: (i64, i64)) {
    let ts = |(s, n): (i64, i64)| Timespec {
        tv_sec: s,
        tv_nsec: n as _,
    };
    utimensat(
        CWD,
        p,
        &Timestamps {
            last_access: ts(a),
            last_modification: ts(m),
        },
        AtFlags::SYMLINK_NOFOLLOW,
    )
    .unwrap();
}

fn getx(p: &Path, name: &str) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; 4096];
    match rustix::fs::lgetxattr(p, name, &mut buf[..]) {
        Ok(n) => {
            buf.truncate(n);
            Some(buf)
        }
        Err(rustix::io::Errno::NODATA) => None,
        Err(e) => panic!("getxattr {name} on {}: {e}", p.display()),
    }
}

fn setx(p: &Path, name: &str, value: &[u8]) -> rustix::io::Result<()> {
    rustix::fs::lsetxattr(p, name, value, rustix::fs::XattrFlags::empty())
}

#[tokio::test]
async fn fidelity_modes_times_ns() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(src.join("sticky")).unwrap();
    let files = [
        (
            "a640",
            0o640,
            (1_500_000_001, 111_111_111),
            (1_600_000_001, 222_222_222),
        ),
        (
            "b755",
            0o755,
            (1_500_000_002, 333_333_333),
            (1_600_000_002, 444_444_444),
        ),
        (
            "c4755",
            0o4755,
            (1_500_000_003, 555_555_555),
            (1_600_000_003, 666_666_666),
        ),
        (
            "d2755",
            0o2755,
            (1_500_000_004, 1),
            (1_600_000_004, 999_999_999),
        ),
    ];
    for (name, mode, a, m) in files {
        let p = src.join(name);
        fs::write(&p, name.as_bytes()).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
        set_times(&p, a, m);
    }
    fs::set_permissions(src.join("sticky"), fs::Permissions::from_mode(0o1777)).unwrap();
    set_times(
        &src.join("sticky"),
        (1_500_000_010, 123_456_789),
        (1_600_000_010, 987_654_321),
    );
    set_times(&src, (1_500_000_020, 5), (1_600_000_020, 6));

    let rt = backup_restore(tmp.path(), std::slice::from_ref(&src)).await;

    // Only stat the restored entries: reading a directory would move its atime.
    for (name, mode, a, m) in files {
        let p = rt.at(&src).join(name);
        assert_eq!(fs::metadata(&p).unwrap().mode() & 0o7777, mode, "{name}");
        assert_eq!(atime(&p), a, "atime of {name}");
        assert_eq!(mtime(&p), m, "mtime of {name}");
    }
    let sticky = rt.at(&src).join("sticky");
    assert_eq!(fs::metadata(&sticky).unwrap().mode() & 0o7777, 0o1777);
    assert_eq!(atime(&sticky), (1_500_000_010, 123_456_789));
    assert_eq!(mtime(&sticky), (1_600_000_010, 987_654_321));
    assert_eq!(atime(&rt.at(&src)), (1_500_000_020, 5));
    assert_eq!(mtime(&rt.at(&src)), (1_600_000_020, 6));
    rt.assert_clean(&src);
}

#[tokio::test]
async fn fidelity_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::write(src.join("sub/file"), b"x").unwrap();
    let odd = OsStr::from_bytes(b"target-\xff\xfe-odd");
    let outside = tmp.path().join("outside");
    let links: Vec<(&str, PathBuf)> = vec![
        ("abs", PathBuf::from("/etc/passwd")),
        ("rel", PathBuf::from("sub/file")),
        ("dangling", PathBuf::from("no/such/place")),
        ("odd", PathBuf::from(odd)),
        ("escape", outside.clone()),
    ];
    for (name, target) in &links {
        symlink(target, src.join(name)).unwrap();
    }
    set_times(&src.join("rel"), (1_500_000_000, 7), (1_600_000_000, 8));

    let rt = backup_restore(tmp.path(), std::slice::from_ref(&src)).await;

    for (name, target) in &links {
        let p = rt.at(&src).join(name);
        assert!(fs::symlink_metadata(&p).unwrap().is_symlink(), "{name}");
        assert_eq!(
            fs::read_link(&p).unwrap().as_os_str().as_bytes(),
            target.as_os_str().as_bytes(),
            "target of {name}"
        );
    }
    assert_eq!(mtime(&rt.at(&src).join("rel")), (1_600_000_000, 8));
    assert!(
        !outside.exists(),
        "restore followed a symlink out of the target"
    );

    // A symlink already sitting where a directory belongs is replaced by the
    // directory and never written through.
    let tmp2 = tempfile::tempdir().unwrap();
    let src2 = tmp2.path().join("src");
    fs::create_dir_all(src2.join("d")).unwrap();
    fs::write(src2.join("d/payload"), b"payload").unwrap();
    let trap = tmp2.path().join("trap");
    fs::create_dir(&trap).unwrap();
    let target = tmp2.path().join("out");
    let planted = target.join(src2.canonicalize().unwrap().strip_prefix("/").unwrap());
    fs::create_dir_all(&planted).unwrap();
    symlink(&trap, planted.join("d")).unwrap();

    let (store, outcome) = backup(tmp2.path(), std::slice::from_ref(&src2)).await;
    restore(&store, &outcome, &target).await.unwrap();
    assert_eq!(
        fs::read_dir(&trap).unwrap().count(),
        0,
        "wrote through a symlink"
    );
    let replaced = fs::symlink_metadata(planted.join("d")).unwrap();
    assert!(replaced.is_dir() && !replaced.is_symlink());
    assert_eq!(fs::read(planted.join("d/payload")).unwrap(), b"payload");
}

#[tokio::test]
async fn fidelity_hardlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    for d in ["a", "b", "c/d"] {
        fs::create_dir_all(src.join(d)).unwrap();
    }
    fs::write(src.join("a/f1"), b"shared content").unwrap();
    fs::hard_link(src.join("a/f1"), src.join("b/f2")).unwrap();
    fs::hard_link(src.join("a/f1"), src.join("c/d/f3")).unwrap();
    fs::write(src.join("p1"), b"pair").unwrap();
    fs::hard_link(src.join("p1"), src.join("p2")).unwrap();
    fs::write(src.join("alone"), b"alone").unwrap();

    let rt = backup_restore(tmp.path(), std::slice::from_ref(&src)).await;

    let out = rt.at(&src);
    let group: Vec<_> = ["a/f1", "b/f2", "c/d/f3"]
        .iter()
        .map(|n| fs::metadata(out.join(n)).unwrap())
        .collect();
    assert!(group.iter().all(|m| m.ino() == group[0].ino()));
    assert!(group.iter().all(|m| m.nlink() == 3));
    let pair = [
        fs::metadata(out.join("p1")).unwrap(),
        fs::metadata(out.join("p2")).unwrap(),
    ];
    assert_eq!(pair[0].ino(), pair[1].ino());
    assert_eq!(pair[0].nlink(), 2);
    assert_ne!(pair[0].ino(), group[0].ino());
    assert_eq!(fs::metadata(out.join("alone")).unwrap().nlink(), 1);
    assert_eq!(fs::read(out.join("c/d/f3")).unwrap(), b"shared content");
    rt.assert_clean(&src);
}

#[tokio::test]
async fn fidelity_fifo() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    let fifo = src.join("pipe");
    mknodat(CWD, &fifo, FileType::Fifo, Mode::from_raw_mode(0o640), 0).unwrap();
    fs::set_permissions(&fifo, fs::Permissions::from_mode(0o640)).unwrap();
    set_times(&fifo, (1_500_000_000, 1), (1_600_000_000, 2));

    let rt = backup_restore(tmp.path(), std::slice::from_ref(&src)).await;

    let restored = rt.at(&fifo);
    let m = fs::symlink_metadata(&restored).unwrap();
    assert_eq!(FileType::from_raw_mode(m.mode()), FileType::Fifo);
    assert_eq!(m.mode() & 0o7777, 0o640);
    assert_eq!(mtime(&restored), (1_600_000_000, 2));
    rt.assert_clean(&src);
}

#[tokio::test]
async fn fidelity_user_xattrs() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(src.join("dir")).unwrap();
    fs::write(src.join("file"), b"data").unwrap();
    let binary: Vec<u8> = vec![0, 1, 0xff, 0xfe, 0, 42];
    let probe = setx(&src.join("file"), "user.lode.binary", &binary);
    assert!(
        probe.is_ok(),
        "user xattrs are not supported on {} ({probe:?}); run on a filesystem that has them (RESEARCH A8)",
        tmp.path().display()
    );
    setx(&src.join("file"), "user.lode.text", b"hello").unwrap();
    setx(&src.join("dir"), "user.lode.dir", &binary).unwrap();

    let rt = backup_restore(tmp.path(), std::slice::from_ref(&src)).await;

    let out = rt.at(&src);
    assert_eq!(
        getx(&out.join("file"), "user.lode.binary"),
        Some(binary.clone())
    );
    assert_eq!(
        getx(&out.join("file"), "user.lode.text"),
        Some(b"hello".to_vec())
    );
    assert_eq!(getx(&out.join("dir"), "user.lode.dir"), Some(binary));
    rt.assert_clean(&src);
}

#[tokio::test]
async fn fidelity_nonroot_owner_summary() {
    if own_uid() == 0 {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let file = ["/etc/hostname", "/etc/passwd"]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .unwrap();
    let rt = backup_restore(tmp.path(), std::slice::from_ref(&file)).await;

    let rel = file.strip_prefix("/").unwrap().as_os_str().as_bytes();
    assert!(
        rt.report
            .unapplied
            .iter()
            .any(|u| u.path == rel && u.what == "owner"),
        "{:?}",
        rt.report.unapplied
    );
    assert!(rt.at(&file).exists());
}

/// The manifest node at `path`, found by walking the snapshot's trees.
async fn find_node(rt: &Rt, path: &Path) -> pb::Node {
    let mut r = open_reader(&rt.store).await;
    let snap = r.list_snapshots().await.unwrap().entries.remove(0);
    let mut ids = vec![snap.snapshot.root_tree.clone()];
    let comps: Vec<_> = path
        .canonicalize()
        .unwrap()
        .components()
        .skip(1)
        .map(|c| c.as_os_str().as_bytes().to_vec())
        .collect();
    let mut node = None;
    for comp in comps {
        let mut found = None;
        for id in &ids {
            let bytes = r
                .read_blob(id.as_slice().try_into().unwrap(), BLOB_TYPE_TREE)
                .await
                .unwrap();
            found = decode_tree(&bytes, None)
                .unwrap()
                .into_iter()
                .find(|n| n.name == comp)
                .or(found);
        }
        let n = found.expect("path component in snapshot");
        ids = n.subtrees.clone();
        node = Some(n);
    }
    node.unwrap()
}

#[tokio::test]
async fn fidelity_sparse_extents() {
    const MIB: u64 = 1 << 20;
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    let sparse = src.join("sparse");

    let mut rng = ChaCha20Rng::from_seed([9; 32]);
    let mut block = vec![0u8; MIB as usize];
    rng.fill_bytes(&mut block);
    let mut f = fs::File::create(&sparse).unwrap();
    f.write_all(&block).unwrap();
    f.seek(SeekFrom::Start(101 * MIB)).unwrap();
    f.write_all(&block).unwrap();
    f.sync_all().unwrap();
    drop(f);
    let logical = 102 * MIB;
    assert_eq!(fs::metadata(&sparse).unwrap().len(), logical);

    let rt = backup_restore(tmp.path(), std::slice::from_ref(&src)).await;

    let node = find_node(&rt, &sparse).await;
    assert_eq!(node.sparse_extents.len(), 2, "{:?}", node.sparse_extents);
    assert_eq!(node.size, logical);
    assert!(
        rt.outcome.stats.bytes_read < 4 * MIB,
        "read {} bytes",
        rt.outcome.stats.bytes_read
    );

    let restored = rt.at(&sparse);
    let m = fs::metadata(&restored).unwrap();
    assert_eq!(m.len(), logical);
    assert!(
        m.blocks() * 512 < logical / 10,
        "{} bytes allocated for a {logical} byte file",
        m.blocks() * 512
    );
    let (mut a, mut b) = (
        fs::File::open(&sparse).unwrap(),
        fs::File::open(&restored).unwrap(),
    );
    let (mut ba, mut bb) = (vec![0u8; MIB as usize], vec![0u8; MIB as usize]);
    for i in 0..102 {
        a.read_exact(&mut ba).unwrap();
        b.read_exact(&mut bb).unwrap();
        assert!(ba == bb, "megabyte {i} differs");
    }
}

#[tokio::test]
async fn fidelity_sparse_all_hole() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    fs::File::create(src.join("hole"))
        .unwrap()
        .set_len(10 << 20)
        .unwrap();

    let rt = backup_restore(tmp.path(), std::slice::from_ref(&src)).await;

    let m = fs::metadata(rt.at(&src).join("hole")).unwrap();
    assert_eq!(m.len(), 10 << 20);
    assert_eq!(m.blocks(), 0);
}

fn require_root() {
    assert_eq!(own_uid(), 0, "run through scripts/test-linux.sh --root");
}

#[tokio::test]
#[ignore = "requires root; run scripts/test-linux.sh --root"]
async fn fidelity_root_owner_and_setuid() {
    require_root();
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    let file = src.join("suid");
    fs::write(&file, b"#!/bin/sh\n").unwrap();
    std::os::unix::fs::chown(&file, Some(1234), Some(1234)).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o4755)).unwrap();

    let rt = backup_restore(tmp.path(), std::slice::from_ref(&src)).await;

    let m = fs::metadata(rt.at(&file)).unwrap();
    assert_eq!((m.uid(), m.gid()), (1234, 1234));
    assert_eq!(m.mode() & 0o7777, 0o4755);
    rt.assert_clean(&src);
}

#[tokio::test]
#[ignore = "requires root; run scripts/test-linux.sh --root"]
async fn fidelity_root_capability_xattr() {
    require_root();
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    let file = src.join("bin");
    fs::write(&file, b"elf").unwrap();
    // VFS_CAP_REVISION_2 with the effective bit, CAP_NET_BIND_SERVICE permitted.
    let mut cap = vec![0x01, 0x00, 0x00, 0x02];
    cap.extend_from_slice(&(1u32 << 10).to_le_bytes());
    cap.extend_from_slice(&[0; 12]);
    setx(&file, "security.capability", &cap).unwrap();

    let rt = backup_restore(tmp.path(), std::slice::from_ref(&src)).await;

    assert_eq!(getx(&rt.at(&file), "security.capability"), Some(cap));
    rt.assert_clean(&src);
}

#[tokio::test]
#[ignore = "requires root; run scripts/test-linux.sh --root"]
async fn fidelity_root_posix_acl() {
    require_root();
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(src.join("dir")).unwrap();
    fs::write(src.join("file"), b"acl").unwrap();
    let setfacl = |args: &[&str], path: &Path| {
        let status = Command::new("setfacl")
            .args(args)
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success(), "setfacl {args:?}");
    };
    setfacl(&["-m", "u:1234:r"], &src.join("file"));
    setfacl(&["-d", "-m", "u:1234:rx"], &src.join("dir"));
    let access = getx(&src.join("file"), "system.posix_acl_access").expect("access ACL");
    let default = getx(&src.join("dir"), "system.posix_acl_default").expect("default ACL");

    let rt = backup_restore(tmp.path(), std::slice::from_ref(&src)).await;

    let out = rt.at(&src);
    assert_eq!(
        getx(&out.join("file"), "system.posix_acl_access"),
        Some(access)
    );
    assert_eq!(
        getx(&out.join("dir"), "system.posix_acl_default"),
        Some(default)
    );
    rt.assert_clean(&src);
}

#[tokio::test]
#[ignore = "requires root; run scripts/test-linux.sh --root"]
async fn fidelity_root_devices() {
    require_root();
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    let dev = src.join("null");
    mknodat(
        CWD,
        &dev,
        FileType::CharacterDevice,
        Mode::from_raw_mode(0o600),
        rustix::fs::makedev(1, 3),
    )
    .unwrap();

    let rt = backup_restore(tmp.path(), std::slice::from_ref(&src)).await;

    let (a, b) = (
        fs::symlink_metadata(&dev).unwrap(),
        fs::symlink_metadata(rt.at(&dev)).unwrap(),
    );
    assert_eq!(FileType::from_raw_mode(b.mode()), FileType::CharacterDevice);
    assert_eq!(a.rdev(), b.rdev());
    rt.assert_clean(&src);
}

#[tokio::test]
#[ignore = "requires root; run scripts/test-linux.sh --root"]
async fn fidelity_root_trusted_xattr() {
    require_root();
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    let file = src.join("f");
    fs::write(&file, b"t").unwrap();
    let probe = setx(&file, "trusted.lode", b"secret\0value");
    assert!(
        probe.is_ok(),
        "trusted.* needs CAP_SYS_ADMIN ({probe:?}); scripts/test-linux.sh --root adds it"
    );

    let rt = backup_restore(tmp.path(), std::slice::from_ref(&src)).await;

    assert_eq!(
        getx(&rt.at(&file), "trusted.lode"),
        Some(b"secret\0value".to_vec())
    );
    rt.assert_clean(&src);
}
