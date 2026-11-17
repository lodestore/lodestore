use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::{Cursor, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use lodestore_format::crypto::{BlobId, Entropy};
use lodestore_format::keys::KdfCost;
use lodestore_format::pack::pack_object_key;
use lodestore_format::pb;
use lodestore_format::reader::Reader;
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::selection::Selection;
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_server::download::archive::{ArchiveFormat, ArchiveReport, write_archive};
use tar::EntryType;
use tempfile::TempDir;
use tokio::runtime::Handle;

const PASSWORD: &[u8] = b"pw";
const MTIME: u64 = 1_600_000_000;
const MIB: usize = 1 << 20;

const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

fn write_file(path: &Path, data: &[u8], mode: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(MTIME))
        .unwrap();
}

struct Fx {
    tmp: TempDir,
    reader: Option<Reader<LocalFsStore>>,
    snapshot: BlobId,
    src: PathBuf,
}

impl Fx {
    async fn new(build: impl FnOnce(&Path)) -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        build(&src);

        let store = LocalFsStore::new(root.join("repo"));
        let mut entropy = Entropy::os();
        PendingInit::generate(
            &mut entropy,
            InitOptions {
                kdf: LOW_KDF,
                ..Default::default()
            },
        )
        .unwrap()
        .commit(&store, PASSWORD, &mut entropy)
        .await
        .unwrap();
        let repo = Repository::open(&store, PASSWORD).await.unwrap();
        let keys = repo.read_keys().write_keys(repo.config_hash());
        let mut writer = Writer::new(&store, keys, repo.config()).await.unwrap();
        let outcome = writer
            .backup(
                std::slice::from_ref(&src),
                BackupOptions::default(),
                &|_| {},
            )
            .await
            .unwrap();
        let reader = Reader::open(store, repo.read_keys().duplicate(), repo.config()).unwrap();
        Fx {
            tmp,
            reader: Some(reader),
            snapshot: outcome.snapshot_id,
            src,
        }
    }

    fn root(&self) -> String {
        self.src
            .display()
            .to_string()
            .trim_start_matches('/')
            .to_string()
    }

    fn select(&self, below: &str) -> Selection {
        let mut path = self.src.as_os_str().as_bytes().to_vec();
        if !below.is_empty() {
            path.push(b'/');
            path.extend_from_slice(below.as_bytes());
        }
        Selection::from_paths([path]).unwrap()
    }

    async fn archive(
        &mut self,
        selection: Selection,
        format: ArchiveFormat,
    ) -> (Vec<u8>, anyhow::Result<ArchiveReport>) {
        let mut reader = self.reader.take().unwrap();
        let snapshot = self.snapshot;
        let handle = Handle::current();
        let (reader, out, result) = tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            let result = write_archive(
                &handle,
                &mut reader,
                &snapshot,
                &selection,
                format,
                &mut out,
            );
            (reader, out, result)
        })
        .await
        .unwrap();
        self.reader = Some(reader);
        (out, result)
    }

    async fn nodes(&mut self, below: &str) -> Vec<pb::Node> {
        let mut path = self.src.as_os_str().as_bytes().to_vec();
        if !below.is_empty() {
            path.push(b'/');
            path.extend_from_slice(below.as_bytes());
        }
        let snapshot = self.snapshot;
        self.reader
            .as_mut()
            .unwrap()
            .list_dir(&snapshot, &path)
            .await
            .unwrap()
    }
}

struct Member {
    kind: EntryType,
    mode: u32,
    uid: u64,
    gid: u64,
    mtime: u64,
    size: u64,
    link: Option<Vec<u8>>,
    data: Vec<u8>,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pax: Vec<(String, Vec<u8>)>,
}

fn read_tar(source: impl Read) -> BTreeMap<Vec<u8>, Member> {
    let mut archive = tar::Archive::new(source);
    let mut out = BTreeMap::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let mut path = entry.path_bytes().into_owned();
        while path.last() == Some(&b'/') {
            path.pop();
        }
        let pax = entry
            .pax_extensions()
            .unwrap()
            .map(|extensions| {
                extensions
                    .map(|ext| {
                        let ext = ext.unwrap();
                        (ext.key().unwrap().to_string(), ext.value_bytes().to_vec())
                    })
                    .collect()
            })
            .unwrap_or_default();
        let header = entry.header().clone();
        let link = entry.link_name_bytes().map(|l| l.into_owned());
        let mut data = Vec::new();
        entry.read_to_end(&mut data).unwrap();
        out.insert(
            path,
            Member {
                kind: header.entry_type(),
                mode: header.mode().unwrap(),
                uid: header.uid().unwrap(),
                gid: header.gid().unwrap(),
                mtime: header.mtime().unwrap(),
                size: header.size().unwrap(),
                link,
                data,
                pax,
            },
        );
    }
    out
}

fn gunzip(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut out)
        .unwrap();
    out
}

fn tree(src: &Path) {
    write_file(&src.join("a.txt"), b"alpha", 0o640);
    write_file(&src.join("sub/b.bin"), &noise(3 * MIB, 7), 0o600);
    fs::create_dir_all(src.join("sub/empty")).unwrap();
    fs::set_permissions(src.join("sub/empty"), fs::Permissions::from_mode(0o750)).unwrap();
    std::os::unix::fs::symlink("a.txt", src.join("ln")).unwrap();
    write_file(&src.join("hl1"), b"shared inode", 0o644);
    fs::hard_link(src.join("hl1"), src.join("hl2")).unwrap();
    let made = std::process::Command::new("mkfifo")
        .arg(src.join("pipe"))
        .status()
        .unwrap();
    assert!(made.success());
}

fn check_tar(fx: &Fx, members: &BTreeMap<Vec<u8>, Member>) {
    let root = fx.root();
    let at = |below: &str| -> Vec<u8> {
        if below.is_empty() {
            root.clone().into_bytes()
        } else {
            format!("{root}/{below}").into_bytes()
        }
    };
    assert!(
        members.keys().all(|p| p.starts_with(root.as_bytes())),
        "no ancestors, relative paths: {:?}",
        members
            .keys()
            .map(|k| String::from_utf8_lossy(k))
            .collect::<Vec<_>>()
    );
    assert_eq!(members.len(), 9);

    let meta = fs::metadata(fx.src.join("a.txt")).unwrap();
    let a = &members[&at("a.txt")];
    assert_eq!(a.kind, EntryType::Regular);
    assert_eq!(a.data, b"alpha");
    assert_eq!(a.mode & 0o7777, 0o640);
    assert_eq!(a.mtime, MTIME);
    assert_eq!(
        (a.uid, a.gid),
        (u64::from(meta.uid()), u64::from(meta.gid()))
    );

    assert_eq!(members[&at("sub/b.bin")].data, noise(3 * MIB, 7));
    let empty = &members[&at("sub/empty")];
    assert_eq!(empty.kind, EntryType::Directory);
    assert_eq!(empty.mode & 0o7777, 0o750);
    assert_eq!(members[&at("")].kind, EntryType::Directory);
    assert_eq!(members[&at("sub")].kind, EntryType::Directory);

    let link = &members[&at("ln")];
    assert_eq!(link.kind, EntryType::Symlink);
    assert_eq!(link.link.as_deref(), Some(&b"a.txt"[..]));

    let first = &members[&at("hl1")];
    assert_eq!(first.kind, EntryType::Regular);
    assert_eq!(first.data, b"shared inode");
    let second = &members[&at("hl2")];
    assert_eq!(second.kind, EntryType::Link);
    assert_eq!(second.link.as_deref(), Some(at("hl1").as_slice()));
    assert_eq!(second.size, 0);

    let pipe = &members[&at("pipe")];
    assert_eq!(pipe.kind, EntryType::Fifo);
    assert_eq!(pipe.size, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_archive_tar_gz_roundtrip() {
    let mut fx = Fx::new(tree).await;
    let (bytes, report) = fx.archive(fx.select(""), ArchiveFormat::TarGz).await;
    let report = report.unwrap();
    assert_eq!(
        report,
        ArchiveReport {
            files: 4,
            dirs: 3,
            skipped: 0
        }
    );
    check_tar(&fx, &read_tar(Cursor::new(gunzip(&bytes))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_archive_tar_zst_roundtrip() {
    let mut fx = Fx::new(tree).await;
    let (bytes, report) = fx.archive(fx.select(""), ArchiveFormat::TarZst).await;
    report.unwrap();
    let decoder = zstd::stream::read::Decoder::new(Cursor::new(bytes)).unwrap();
    check_tar(&fx, &read_tar(decoder));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_archive_zip_roundtrip() {
    let mut fx = Fx::new(tree).await;
    let (bytes, report) = fx.archive(fx.select(""), ArchiveFormat::Zip).await;
    assert_eq!(
        report.unwrap(),
        ArchiveReport {
            files: 4,
            dirs: 3,
            skipped: 2
        }
    );
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    let root = fx.root();
    let mut names: Vec<String> = zip.file_names().map(|n| n.unwrap().into_owned()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            format!("{root}/"),
            format!("{root}/a.txt"),
            format!("{root}/hl1"),
            format!("{root}/hl2"),
            format!("{root}/sub/"),
            format!("{root}/sub/b.bin"),
            format!("{root}/sub/empty/"),
        ]
    );
    let mut text = String::new();
    let mut file = zip.by_name(&format!("{root}/a.txt")).unwrap();
    file.read_to_string(&mut text).unwrap();
    assert_eq!(text, "alpha");
    let time = file.last_modified().unwrap();
    assert_eq!(
        (
            time.year(),
            time.month(),
            time.day(),
            time.hour(),
            time.minute(),
            time.second()
        ),
        (2020, 9, 13, 12, 26, 40)
    );
    drop(file);
    let mut big = Vec::new();
    zip.by_name(&format!("{root}/sub/b.bin"))
        .unwrap()
        .read_to_end(&mut big)
        .unwrap();
    assert_eq!(big, noise(3 * MIB, 7));
    let mut shared = String::new();
    zip.by_name(&format!("{root}/hl2"))
        .unwrap()
        .read_to_string(&mut shared)
        .unwrap();
    assert_eq!(shared, "shared inode");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_archive_sparse_dense() {
    let mut fx = Fx::new(|src| {
        let file = fs::File::create(src.join("sparse.bin")).unwrap();
        file.set_len(8 * MIB as u64).unwrap();
        file.write_all_at(&noise(64 << 10, 3), 4 * MIB as u64)
            .unwrap();
    })
    .await;
    #[cfg(target_os = "linux")]
    assert!(
        !fx.nodes("").await[0].sparse_extents.is_empty(),
        "the snapshot should hold the file as extents"
    );

    let (bytes, report) = fx
        .archive(fx.select("sparse.bin"), ArchiveFormat::TarGz)
        .await;
    report.unwrap();
    let members = read_tar(Cursor::new(gunzip(&bytes)));
    let member = &members[format!("{}/sparse.bin", fx.root()).as_bytes()];
    assert_eq!(member.kind, EntryType::Regular);
    assert_eq!(member.size, 8 * MIB as u64);
    assert_eq!(member.data, fs::read(fx.src.join("sparse.bin")).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_archive_non_utf8_names() {
    let odd = OsStr::from_bytes(b"odd-\xff.txt");
    let probe = tempfile::tempdir().unwrap();
    if fs::write(probe.path().join(odd), b"x").is_err() {
        eprintln!("skipped: this filesystem refuses non-UTF-8 names");
        return;
    }
    let mut fx = Fx::new(|src| write_file(&src.join(odd), b"odd one", 0o644)).await;

    let (bytes, report) = fx.archive(fx.select(""), ArchiveFormat::TarGz).await;
    report.unwrap();
    let members = read_tar(Cursor::new(gunzip(&bytes)));
    let mut expected = format!("{}/", fx.root()).into_bytes();
    expected.extend_from_slice(b"odd-\xff.txt");
    assert_eq!(members[&expected].data, b"odd one");

    let (bytes, report) = fx.archive(fx.select(""), ArchiveFormat::Zip).await;
    report.unwrap();
    let zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    let lossy = format!("{}/odd-\u{fffd}.txt", fx.root());
    assert!(zip.file_names().any(|n| n.unwrap() == lossy));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_archive_selection_only() {
    let mut fx = Fx::new(tree).await;
    let (bytes, report) = fx
        .archive(fx.select("sub/b.bin"), ArchiveFormat::TarGz)
        .await;
    assert_eq!(
        report.unwrap(),
        ArchiveReport {
            files: 1,
            dirs: 0,
            skipped: 0
        }
    );
    let members = read_tar(Cursor::new(gunzip(&bytes)));
    assert_eq!(
        members.keys().cloned().collect::<Vec<_>>(),
        [format!("{}/sub/b.bin", fx.root()).into_bytes()]
    );
    assert!(!members.keys().next().unwrap().starts_with(b"/"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_archive_error_does_not_finish() {
    let mut fx = Fx::new(|src| {
        write_file(&src.join("a-good.bin"), &noise(2 * MIB, 11), 0o644);
        write_file(&src.join("z-bad.bin"), &noise(MIB, 12), 0o644);
    })
    .await;
    let nodes = fx.nodes("").await;
    let bad = nodes.iter().find(|n| n.name == b"z-bad.bin").unwrap();
    let snapshot_key = fx
        .reader
        .as_mut()
        .unwrap()
        .chunk_locations(bad)
        .await
        .unwrap()[0];
    let pack = fx
        .tmp
        .path()
        .canonicalize()
        .unwrap()
        .join("repo")
        .join(pack_object_key(&snapshot_key.pack_id));
    let mut pack_bytes = fs::read(&pack).unwrap();
    let hit = snapshot_key.offset as usize + snapshot_key.enc_len as usize / 2;
    pack_bytes[hit] ^= 0xff;
    fs::write(&pack, pack_bytes).unwrap();

    let (gz, report) = fx.archive(fx.select(""), ArchiveFormat::TarGz).await;
    assert!(report.is_err());
    assert!(gz.len() > MIB, "the good file went out before the failure");
    let mut sink = Vec::new();
    assert!(
        flate2::read::GzDecoder::new(&gz[..])
            .read_to_end(&mut sink)
            .is_err(),
        "a cut-short gzip must not look whole"
    );

    let (zst, report) = fx.archive(fx.select(""), ArchiveFormat::TarZst).await;
    assert!(report.is_err());
    let mut sink = Vec::new();
    assert!(
        zstd::stream::read::Decoder::new(&zst[..])
            .unwrap()
            .read_to_end(&mut sink)
            .is_err()
    );

    let (zipped, report) = fx.archive(fx.select(""), ArchiveFormat::Zip).await;
    assert!(report.is_err());
    assert!(zip::ZipArchive::new(Cursor::new(zipped)).is_err());
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_archive_xattrs_in_pax() {
    let probe = tempfile::tempdir().unwrap();
    let file = probe.path().join("probe");
    fs::write(&file, b"x").unwrap();
    if rustix::fs::lsetxattr(&file, "user.k", b"v", rustix::fs::XattrFlags::empty()).is_err() {
        eprintln!("skipped: user xattrs are not supported here");
        return;
    }
    let mut fx = Fx::new(|src| {
        write_file(&src.join("tagged"), b"data", 0o644);
        rustix::fs::lsetxattr(
            src.join("tagged"),
            "user.k",
            b"v",
            rustix::fs::XattrFlags::empty(),
        )
        .unwrap();
    })
    .await;
    let (bytes, report) = fx.archive(fx.select("tagged"), ArchiveFormat::TarGz).await;
    report.unwrap();
    let members = read_tar(Cursor::new(gunzip(&bytes)));
    let member = &members[format!("{}/tagged", fx.root()).as_bytes()];
    assert!(
        member
            .pax
            .contains(&("SCHILY.xattr.user.k".to_string(), b"v".to_vec()))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "slow; run with --run-ignored only"]
async fn download_archive_zip64_large() {
    const SIZE: u64 = 4_100 * MIB as u64;
    let mut fx = Fx::new(|src| {
        let file = fs::File::create(src.join("huge.bin")).unwrap();
        file.set_len(SIZE).unwrap();
        file.write_all_at(&noise(MIB, 5), SIZE - MIB as u64)
            .unwrap();
    })
    .await;
    let (bytes, report) = fx.archive(fx.select("huge.bin"), ArchiveFormat::Zip).await;
    report.unwrap();
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    let file = zip.by_name(&format!("{}/huge.bin", fx.root())).unwrap();
    assert_eq!(file.size(), SIZE);
}
