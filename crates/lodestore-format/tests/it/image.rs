use std::fs;
use std::future::Future;
use std::io::Write as _;
#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use lodestore_format::Error;
use lodestore_format::crypto::{BLOB_TYPE_DATA, BlobId, Entropy};
use lodestore_format::fs::filter::PathMatcher;
use lodestore_format::fs::restore::{Overwrite, check_extents};
#[cfg(target_os = "linux")]
use lodestore_format::fs::walk::data_extents;
use lodestore_format::image::{
    BitmapBinding, ChunkRef, DISK_SIZE, FullReason, IMAGE_CHUNK_SIZE, ImageExtent, ImageMeta,
    ImageParent, ImageSource, MAX_IMAGE_NAME_LEN, MAX_IMAGE_SIZE, MAX_SEGMENTS, ParentChunks,
    ReadMode, SEGMENT_CHUNKS, disk_meta_key, load_image, merge_head, merge_segments, parse_layout,
    segment_name, segment_nodes, valid_image_name,
};
use lodestore_format::keys::KdfCost;
use lodestore_format::pack::CODEC_RAW;
use lodestore_format::pb;
use lodestore_format::reader::{Reader, RestoreOptions, RestoreProgress};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::source::{self, Meta, SourceKey, SourceType};
use lodestore_format::store::LocalFsStore;
use lodestore_format::tree::encode_tree;
use lodestore_format::writer::{BackupOptions, ImageOptions, ImageOutcome, Writer};
use proptest::prelude::*;
use prost::Message;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{Rng, SeedableRng};

const C: u64 = IMAGE_CHUNK_SIZE;
const MIB: u64 = 1 << 20;
const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

async fn init_repo(dir: &Path) -> LocalFsStore {
    let store = LocalFsStore::new(dir.to_path_buf());
    let mut entropy = Entropy::os();
    let options = InitOptions {
        kdf: LOW_KDF,
        ..Default::default()
    };
    PendingInit::generate(&mut entropy, options)
        .unwrap()
        .commit(&store, b"pw", &mut entropy)
        .await
        .unwrap();
    store
}

async fn writer(dir: &Path) -> Writer<LocalFsStore> {
    let store = LocalFsStore::new(dir.to_path_buf());
    let repo = Repository::open(&store, b"pw").await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    Writer::new(store, keys, repo.config()).await.unwrap()
}

async fn reader(dir: &Path) -> Reader<LocalFsStore> {
    let store = LocalFsStore::new(dir.to_path_buf());
    let repo = Repository::open(&store, b"pw").await.unwrap();
    Reader::open(store, repo.read_keys().duplicate(), repo.config()).unwrap()
}

fn random_bytes(len: usize, seed: u8) -> Vec<u8> {
    let mut rng = ChaCha20Rng::from_seed([seed; 32]);
    let mut out = vec![0u8; len];
    rng.fill_bytes(&mut out);
    out
}

#[derive(Clone, Copy, PartialEq)]
enum Lie {
    ShortExtents,
    EmptyExtent,
    ShortRead,
}

/// An image in memory; the byte ranges in `holes` are reported as holes.
struct MemSource {
    data: Vec<u8>,
    holes: Vec<(u64, u64)>,
    lie: Option<Lie>,
    reads: Vec<u64>,
}

impl MemSource {
    fn new(data: Vec<u8>, holes: Vec<(u64, u64)>) -> Self {
        MemSource {
            data,
            holes,
            lie: None,
            reads: Vec::new(),
        }
    }
}

impl ImageSource for MemSource {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }

    async fn extents(&mut self, offset: u64, len: u64) -> Result<Vec<ImageExtent>, Error> {
        let end = offset + len;
        let mut out = Vec::new();
        let mut pos = offset;
        while pos < end {
            let inside = self.holes.iter().find(|&&(s, l)| s <= pos && pos < s + l);
            let (run, zero) = match inside {
                Some(&(s, l)) => ((s + l).min(end) - pos, true),
                None => {
                    let next = self.holes.iter().map(|h| h.0).filter(|&s| s > pos).min();
                    (next.unwrap_or(end).min(end) - pos, false)
                }
            };
            out.push(ImageExtent {
                len: run,
                zero,
                dirty: false,
            });
            pos += run;
        }
        match self.lie {
            Some(Lie::ShortExtents) => {
                if let Some(last) = out.last_mut() {
                    last.len -= 1;
                }
            }
            Some(Lie::EmptyExtent) => out.insert(
                0,
                ImageExtent {
                    len: 0,
                    zero: false,
                    dirty: false,
                },
            ),
            _ => {}
        }
        Ok(out)
    }

    async fn read(&mut self, offset: u64, len: usize) -> Result<Vec<u8>, Error> {
        self.reads.push(offset);
        let start = usize::try_from(offset).unwrap();
        let mut out = self.data[start..start + len].to_vec();
        if self.lie == Some(Lie::ShortRead) {
            out.pop();
        }
        Ok(out)
    }
}

/// Chunks 0, 3 and 4 hold no data (3 is a hole, 4 is zero bytes the source
/// calls data); 1 and 2 are data, 5 is data in a 1 MiB tail.
fn holey_image() -> MemSource {
    let mut data = random_bytes((5 * C + MIB) as usize, 7);
    data[..C as usize].fill(0);
    data[3 * C as usize..5 * C as usize].fill(0);
    MemSource::new(data, vec![(0, C), (3 * C, C)])
}

// macOS filesystems may not keep a hole between writes, so only Linux asserts them.
#[cfg(target_os = "linux")]
fn file_data_extents(path: &Path, size: u64) -> Option<Vec<(u64, u64)>> {
    let fd: OwnedFd = fs::File::open(path).unwrap().into();
    data_extents(&fd, size)
}

fn file_node(name: &str, ids: &[BlobId], size: u64) -> pb::Node {
    pb::Node {
        name: name.as_bytes().to_vec(),
        r#type: pb::NodeType::File as i32,
        size,
        content: ids.iter().map(|id| id.to_vec()).collect(),
        ..Default::default()
    }
}

fn dir_node(name: &str, subtrees: &[BlobId]) -> pb::Node {
    pb::Node {
        name: name.as_bytes().to_vec(),
        r#type: pb::NodeType::Dir as i32,
        subtrees: subtrees.iter().map(|id| id.to_vec()).collect(),
        ..Default::default()
    }
}

#[tokio::test]
async fn image_roundtrip_with_holes_across_segments() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir(&repo_dir).unwrap();
    init_repo(&repo_dir).await;
    let mut source = holey_image();
    let original = source.data.clone();

    let mut w = writer(&repo_dir).await;
    let done = w
        .backup_image(
            "disk",
            &mut source,
            None,
            BackupOptions::default(),
            ImageOptions {
                segment_chunks: 2,
                ..Default::default()
            },
            &|_| {},
        )
        .await
        .unwrap();
    assert_eq!(source.reads, vec![C, 2 * C, 4 * C, 5 * C]);

    let mut r = reader(&repo_dir).await;
    let id = done.outcome.snapshot_id;
    let snapshot = r.snapshot(&id).await.unwrap();
    assert_eq!(snapshot.paths, vec![b"/disks/disk".to_vec()]);
    assert_eq!(snapshot.source, Some(done.source.clone()));
    let (key, meta) = source::decode(snapshot.source.as_ref()).unwrap();
    assert_eq!(key.as_str(), "image/disk");
    let parsed = ImageMeta::from_meta(&meta, "disk").unwrap();
    assert_eq!(parsed.read_mode, ReadMode::Full);
    assert_eq!(parsed.disk_size, original.len() as u64);

    let names: Vec<_> = r
        .list_dir(&id, b"/disks/disk")
        .await
        .unwrap()
        .into_iter()
        .map(|n| n.name)
        .collect();
    assert_eq!(
        names,
        vec![b"000000".to_vec(), b"000001".to_vec(), b"000002".to_vec()]
    );

    let out = tmp.path().join("out");
    let report = r
        .restore_image(&id, "disk", &out, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let restored = out.join("disk.img");
    assert!(
        fs::read(&restored).unwrap() == original,
        "the image differs"
    );
    #[cfg(target_os = "linux")]
    assert_eq!(
        file_data_extents(&restored, original.len() as u64),
        Some(vec![(C, 2 * C), (5 * C, MIB)])
    );
}

#[tokio::test]
async fn image_all_zero_writes_no_data_blob() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir(&repo_dir).unwrap();
    init_repo(&repo_dir).await;
    let mut source = MemSource::new(vec![0; 3 * C as usize], vec![(0, C)]);

    let mut w = writer(&repo_dir).await;
    let done = w
        .backup_image(
            "zeros",
            &mut source,
            None,
            BackupOptions::default(),
            ImageOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert_eq!(done.outcome.stats.bytes_added, 0);
    assert_eq!(done.outcome.stats.bytes_total, 3 * C);

    let mut r = reader(&repo_dir).await;
    let segments = r
        .list_dir(&done.outcome.snapshot_id, b"/disks/zeros")
        .await
        .unwrap();
    assert_eq!(segments.len(), 1);
    assert!(segments[0].content.is_empty());
    assert_eq!(
        segments[0].sparse_extents,
        vec![pb::Extent {
            offset: 0,
            length: 0
        }]
    );
}

#[tokio::test]
async fn image_empty_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir(&repo_dir).unwrap();
    init_repo(&repo_dir).await;
    let mut source = MemSource::new(Vec::new(), Vec::new());

    let mut w = writer(&repo_dir).await;
    let done = w
        .backup_image(
            "empty",
            &mut source,
            None,
            BackupOptions::default(),
            ImageOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();

    let mut r = reader(&repo_dir).await;
    let out = tmp.path().join("out");
    r.restore_image(
        &done.outcome.snapshot_id,
        "empty",
        &out,
        &RestoreOptions::default(),
        &|_| {},
    )
    .await
    .unwrap();
    assert_eq!(fs::metadata(out.join("empty.img")).unwrap().len(), 0);
}

#[tokio::test]
async fn image_backup_rejects_a_lying_source() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir(&repo_dir).unwrap();
    init_repo(&repo_dir).await;
    let mut w = writer(&repo_dir).await;

    for lie in [Lie::ShortExtents, Lie::EmptyExtent, Lie::ShortRead] {
        let mut source = MemSource::new(random_bytes(2 * C as usize, 3), Vec::new());
        source.lie = Some(lie);
        let err = w
            .backup_image(
                "liar",
                &mut source,
                None,
                BackupOptions::default(),
                ImageOptions::default(),
                &|_| {},
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)), "{err:?}");
        let listed = reader(&repo_dir).await.list_snapshots().await.unwrap();
        assert!(listed.entries.is_empty());
    }

    let mut honest = MemSource::new(random_bytes(C as usize, 3), Vec::new());
    for bad_name in ["", "a/b", ".", "..", "a\nb"] {
        let err = w
            .backup_image(
                bad_name,
                &mut honest,
                None,
                BackupOptions::default(),
                ImageOptions::default(),
                &|_| {},
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)), "{bad_name:?}");
    }
}

#[tokio::test]
async fn image_backup_fails_when_the_source_changes() {
    use std::os::unix::fs::FileExt;
    use std::time::Duration;

    use lodestore_format::fs::raw::FileSource;

    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir(&repo_dir).unwrap();
    init_repo(&repo_dir).await;
    let mut w = writer(&repo_dir).await;
    let image = tmp.path().join("disk.raw");

    let rewrite = |file: &fs::File| file.write_all_at(b"changed", C + 10).unwrap();
    let grow = |file: &fs::File| file.set_len(3 * C).unwrap();
    for change in [&rewrite as &dyn Fn(&fs::File), &grow] {
        fs::write(&image, random_bytes(2 * C as usize, 5)).unwrap();
        // past the coarse clock tick of the last write, so the change moves mtime
        std::thread::sleep(Duration::from_millis(50));
        let mut source = FileSource::open(&image).unwrap();
        change(&fs::OpenOptions::new().write(true).open(&image).unwrap());
        let err = w
            .backup_image(
                "disk",
                &mut source,
                None,
                BackupOptions::default(),
                ImageOptions::default(),
                &|_| {},
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::InvalidInput(m) if m.contains("changed while it was read")),
            "{err:?}"
        );
        let listed = reader(&repo_dir).await.list_snapshots().await.unwrap();
        assert!(listed.entries.is_empty());
    }

    fs::write(&image, random_bytes(2 * C as usize, 6)).unwrap();
    let mut source = FileSource::open(&image).unwrap();
    w.backup_image(
        "disk",
        &mut source,
        None,
        BackupOptions::default(),
        ImageOptions::default(),
        &|_| {},
    )
    .await
    .expect("an unchanged source is backed up");
}

#[test]
fn image_source_refuses_a_fifo_without_blocking() {
    use lodestore_format::fs::raw::FileSource;

    let tmp = tempfile::tempdir().unwrap();
    let fifo = tmp.path().join("fifo");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || tx.send(FileSource::open(&fifo).err()).unwrap());
    let err = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("opening a FIFO does not block")
        .expect("a FIFO is refused");
    assert!(matches!(err, Error::InvalidInput(_)), "{err:?}");
}

#[tokio::test]
async fn image_load_picks_the_named_disk_among_neighbours() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir(&repo_dir).unwrap();
    init_repo(&repo_dir).await;
    let mut w = writer(&repo_dir).await;

    let conf = w.put_blob_raw(BLOB_TYPE_DATA, CODEC_RAW, b"vm", 2).unwrap();
    let config = w
        .put_tree_raw(&encode_tree(vec![file_node("vm.conf", &[conf], 2)]))
        .unwrap();
    let holes = |size: u64| {
        let chunks = vec![ChunkRef::Zero; size.div_ceil(C) as usize];
        segment_nodes(size, &chunks, SEGMENT_CHUNKS).unwrap()
    };
    let mut disk_nodes = holes(8 * MIB);
    disk_nodes[0].mode = 0o4600;
    let disk = w.put_tree_raw(&encode_tree(disk_nodes)).unwrap();
    let mut broken = holes(4 * MIB);
    broken[0].name = b"x".to_vec();
    let other = w.put_tree_raw(&encode_tree(broken)).unwrap();
    let disks = w
        .put_tree_raw(&encode_tree(vec![
            dir_node("disk", &[disk]),
            dir_node("other", &[other]),
            dir_node("resized", &[disk]),
        ]))
        .unwrap();
    let root = w
        .put_tree_raw(&encode_tree(vec![
            dir_node("config", &[config]),
            dir_node("disks", &[disks]),
        ]))
        .unwrap();
    let mut meta = ImageMeta {
        read_mode: ReadMode::Full,
        disk_size: 8 * MIB,
    }
    .to_meta("disk");
    meta.insert(disk_meta_key("other", DISK_SIZE), (4 * MIB).to_string());
    meta.insert(disk_meta_key("resized", DISK_SIZE), (4 * MIB).to_string());
    let claim = source::encode(&SourceKey::parse("image/disk").unwrap(), &meta).unwrap();
    let id = w
        .commit_snapshot_raw_with(root, vec![b"/".to_vec()], claim)
        .await
        .unwrap();

    let mut r = reader(&repo_dir).await;
    let loaded = load_image(&mut r, &id, "disk").await.unwrap();
    assert_eq!(loaded.layout.size, 8 * MIB);
    assert_eq!(loaded.layout.chunk_table(), vec![ChunkRef::Zero; 2]);
    assert_eq!(loaded.file_name(), b"disk.img");
    assert_eq!(
        loaded.head().unwrap(),
        merge_head(&loaded.layout, b"disk.img")
    );
    assert!(matches!(
        load_image(&mut r, &id, "other").await,
        Err(Error::Corrupt { .. })
    ));
    let resized = load_image(&mut r, &id, "resized").await.unwrap_err();
    assert!(
        matches!(&resized, Error::Corrupt { reason, .. } if reason.contains("the snapshot says")),
        "{resized:?}"
    );
    assert!(matches!(
        load_image(&mut r, &id, "missing").await,
        Err(Error::Corrupt { .. })
    ));
    assert!(matches!(
        load_image(&mut r, &id, "a/b").await,
        Err(Error::InvalidInput(_))
    ));

    let mode_of = |dir: &str| {
        fs::metadata(tmp.path().join(dir).join("disk.img"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    };
    r.restore_image(
        &id,
        "disk",
        &tmp.path().join("owned"),
        &RestoreOptions::default(),
        &|_| {},
    )
    .await
    .unwrap();
    assert_eq!(mode_of("owned"), 0o4600);
    let skip_owners = RestoreOptions {
        skip_owners: true,
        ..Default::default()
    };
    r.restore_image(
        &id,
        "disk",
        &tmp.path().join("plain"),
        &skip_owners,
        &|_| {},
    )
    .await
    .unwrap();
    assert_eq!(mode_of("plain"), 0o600);
}

/// A data chunk, a hole of one chunk and a 1 MiB tail of data.
async fn small_image(repo_dir: &Path) -> (BlobId, Vec<u8>) {
    init_repo(repo_dir).await;
    let mut data = random_bytes(C as usize, 11);
    data.extend(vec![0u8; C as usize]);
    data.extend(random_bytes(MIB as usize, 12));
    let mut source = MemSource::new(data.clone(), vec![(C, C)]);
    let done = writer(repo_dir)
        .await
        .backup_image(
            "disk",
            &mut source,
            None,
            BackupOptions::default(),
            ImageOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    (done.outcome.snapshot_id, data)
}

type Events = Arc<Mutex<Vec<RestoreProgress>>>;

fn collector() -> (Events, impl Fn(&RestoreProgress)) {
    let events = Events::default();
    let sink = Arc::clone(&events);
    (events, move |e: &RestoreProgress| {
        sink.lock().unwrap().push(e.clone());
    })
}

fn last_files(events: &Events) -> u64 {
    events.lock().unwrap().last().unwrap().files_done
}

#[tokio::test]
async fn image_restore_counts_files_and_honours_options() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir(&repo_dir).unwrap();
    let (id, data) = small_image(&repo_dir).await;
    let mut r = reader(&repo_dir).await;
    let out = tmp.path().join("out");

    let (events, collect) = collector();
    let report = r
        .restore_image(&id, "disk", &out, &RestoreOptions::default(), &collect)
        .await
        .unwrap();
    assert_eq!((report.files, report.verified), (1, 0));
    assert_eq!(fs::read(out.join("disk.img")).unwrap(), data);
    let chunks: Vec<u64> = events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.files_done == 0)
        .map(|e| e.bytes_done)
        .collect();
    assert_eq!(chunks, vec![C, C + MIB]);
    assert_eq!(last_files(&events), 1);

    let never = RestoreOptions {
        overwrite: Overwrite::Never,
        ..Default::default()
    };
    let (events, collect) = collector();
    let report = r
        .restore_image(&id, "disk", &out, &never, &collect)
        .await
        .unwrap();
    assert_eq!((report.skipped, report.files), (1, 0));
    assert_eq!(last_files(&events), 1);

    let dry_never = RestoreOptions {
        dry_run: true,
        overwrite: Overwrite::Never,
        ..Default::default()
    };
    let (events, collect) = collector();
    let report = r
        .restore_image(&id, "disk", &out, &dry_never, &collect)
        .await
        .unwrap();
    assert_eq!((report.would_skip, report.would_write), (1, 0));
    assert_eq!(last_files(&events), 1);

    let dry = RestoreOptions {
        dry_run: true,
        ..Default::default()
    };
    let (events, collect) = collector();
    let report = r
        .restore_image(&id, "disk", &tmp.path().join("dry"), &dry, &collect)
        .await
        .unwrap();
    assert_eq!((report.would_write, report.would_skip), (1, 0));
    assert_eq!(last_files(&events), 1);

    let verify = RestoreOptions {
        verify: true,
        ..Default::default()
    };
    let report = r
        .restore_image(&id, "disk", &out, &verify, &|_| {})
        .await
        .unwrap();
    assert_eq!((report.files, report.verified), (1, 1));

    let with_path = RestoreOptions {
        path: b"/disks".to_vec(),
        ..Default::default()
    };
    let with_include = RestoreOptions {
        include: Some(PathMatcher::new(["/disks/*"]).unwrap()),
        ..Default::default()
    };
    for options in [with_path, with_include] {
        let err = r.restore_image(&id, "disk", &out, &options, &|_| {}).await;
        assert!(matches!(err, Err(Error::InvalidInput(_))), "{err:?}");
    }
}

#[tokio::test]
async fn image_restore_raw_fills_a_file_of_the_device_size() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir(&repo_dir).unwrap();
    let (id, data) = small_image(&repo_dir).await;
    let mut r = reader(&repo_dir).await;
    let size = data.len() as u64;
    assert_eq!(size, 2 * C + MIB);

    for out_size in [size, size + 4096] {
        let path = tmp.path().join(format!("dev-{out_size}"));
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        file.write_all(&vec![0xFF; out_size as usize]).unwrap();
        let (events, collect) = collector();
        let report = r
            .restore_image_raw(&id, "disk", &file, out_size, &collect)
            .await
            .unwrap();
        assert_eq!((report.files, report.bytes), (1, size));
        let written = fs::read(&path).unwrap();
        assert_eq!(written.len() as u64, out_size);
        assert!(written[..size as usize] == data[..]);
        assert!(written[size as usize..].iter().all(|&b| b == 0xFF));
        assert_eq!(events.lock().unwrap().last().unwrap().bytes_done, C + MIB);
    }

    let short = fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(tmp.path().join("short"))
        .unwrap();
    let err = r
        .restore_image_raw(&id, "disk", &short, size - 1, &|_| {})
        .await;
    assert!(matches!(err, Err(Error::InvalidInput(_))), "{err:?}");
}

fn image_cases() -> impl Strategy<Value = (u64, Vec<ChunkRef>, u64)> {
    (
        0u64..=13,
        0..C,
        prop::collection::vec(prop::option::of(any::<[u8; 32]>()), 14),
        1u64..=4,
    )
        .prop_map(|(full, tail, refs, per_segment)| {
            let size = full * C + tail;
            let count = usize::try_from(size.div_ceil(C)).unwrap();
            let chunks = refs
                .into_iter()
                .take(count)
                .map(|r| r.map_or(ChunkRef::Zero, ChunkRef::Data))
                .collect();
            (size, chunks, per_segment)
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn image_layout_round_trips((size, chunks, per_segment) in image_cases()) {
        let nodes = segment_nodes(size, &chunks, per_segment).unwrap();
        let layout = parse_layout(&nodes).unwrap();
        prop_assert_eq!(layout.size, size);
        prop_assert_eq!(layout.chunk_table(), chunks.clone());
        let again = segment_nodes(size, &chunks, per_segment).unwrap();
        prop_assert_eq!(
            nodes.iter().map(Message::encode_to_vec).collect::<Vec<_>>(),
            again.iter().map(Message::encode_to_vec).collect::<Vec<_>>()
        );
    }

    #[test]
    fn image_merge_matches_concatenation((size, chunks, per_segment) in image_cases()) {
        let layout = parse_layout(&segment_nodes(size, &chunks, per_segment).unwrap()).unwrap();
        let merged = merge_segments(&layout, b"x.img");
        let extents = check_extents(&merged).unwrap();
        prop_assert_eq!(merged.size, size);
        let ids: Vec<Vec<u8>> = chunks
            .iter()
            .filter_map(|c| match c {
                ChunkRef::Data(id) => Some(id.to_vec()),
                ChunkRef::Zero => None,
            })
            .collect();
        prop_assert_eq!(&merged.content, &ids);
        for (k, chunk) in chunks.iter().enumerate() {
            let from = k as u64 * C;
            let to = (from + C).min(size);
            let covered = extents.iter().any(|&(o, l)| o <= from && to <= o + l);
            let touched = extents.iter().any(|&(o, l)| l > 0 && o < to && from < o + l);
            prop_assert_eq!(covered, matches!(chunk, ChunkRef::Data(_)));
            prop_assert_eq!(touched, covered);
        }
        let mut head = merged.clone();
        head.content.clear();
        prop_assert_eq!(merge_head(&layout, b"x.img"), head);
    }
}

fn base_segments() -> Vec<pb::Node> {
    let chunks = [
        ChunkRef::Data([1; 32]),
        ChunkRef::Zero,
        ChunkRef::Data([2; 32]),
        ChunkRef::Data([3; 32]),
        ChunkRef::Data([4; 32]),
    ];
    segment_nodes(4 * C + MIB, &chunks, 2).unwrap()
}

fn extent(offset: u64, length: u64) -> pb::Extent {
    pb::Extent { offset, length }
}

#[test]
fn image_layout_rejects_hostile_segments() {
    type Edit = fn(&mut Vec<pb::Node>);
    let table: Vec<(&str, &str, Edit)> = vec![
        ("a skipped segment", "named", |n| {
            n.remove(1);
        }),
        ("name 1", "named", |n| n[0].name = b"1".to_vec()),
        ("name 0000001", "named", |n| n[0].name = b"0000001".to_vec()),
        ("name 00000a", "named", |n| n[0].name = b"00000a".to_vec()),
        ("a directory", "plain file", |n| {
            n[0].r#type = pb::NodeType::Dir as i32;
        }),
        ("a hardlink", "plain file", |n| n[0].hardlink_group = 7),
        ("extent past size", "sparse extents", |n| {
            n[2].sparse_extents = vec![extent(0, MIB + 1)];
        }),
        ("extents out of order", "sparse extents", |n| {
            n[1].sparse_extents = vec![extent(C, C), extent(0, C)];
        }),
        ("offset plus length overflows", "sparse extents", |n| {
            n[0].sparse_extents = vec![extent(u64::MAX - 1, 5)];
        }),
        ("adjacent extents", "adjacent extents are not merged", |n| {
            n[1].sparse_extents = vec![extent(0, C), extent(C, C)];
        }),
        ("extent starts inside a chunk", "chunk boundaries", |n| {
            n[0].sparse_extents = vec![extent(1, C)];
        }),
        ("extent ends inside a chunk", "chunk boundaries", |n| {
            n[0].sparse_extents = vec![extent(0, C - 1)];
        }),
        (
            "empty extent with chunk ids",
            "without data lists chunks",
            |n| {
                n[0].sparse_extents = vec![extent(0, 0)];
            },
        ),
        ("empty extent next to data", "empty extent", |n| {
            n[0].sparse_extents = vec![extent(0, C), extent(2 * C, 0)];
        }),
        ("no extents", "no extents", |n| n[0].sparse_extents.clear()),
        ("one id too many", "do not match", |n| {
            n[0].content.push(vec![9; 32]);
        }),
        ("one id too few", "do not match", |n| {
            n[1].content.pop();
        }),
        ("an id of 31 bytes", "not 32 bytes", |n| {
            n[0].content[0] = vec![1; 31];
        }),
        ("a short middle segment", "whole numbers", |n| n[1].size = C),
        ("a middle segment of 4 MiB + 1", "whole numbers", |n| {
            n[0].size = C + 1;
            n[1].size = C + 1;
        }),
        (
            "a last segment larger than the first",
            "larger than the others",
            |n| {
                n[2].size = 3 * C;
            },
        ),
        ("an empty last segment", "larger than the others", |n| {
            n[2].size = 0;
            n[2].sparse_extents = vec![extent(0, 0)];
            n[2].content.clear();
        }),
        (
            "a segment above the segment limit",
            "larger than a segment",
            |n| {
                n.truncate(1);
                n[0].size = (SEGMENT_CHUNKS + 1) * C;
            },
        ),
    ];
    for (label, expect, edit) in table {
        let mut nodes = base_segments();
        edit(&mut nodes);
        match parse_layout(&nodes) {
            Err(Error::Corrupt { reason, .. }) => {
                assert!(reason.contains(expect), "{label}: {reason}");
            }
            other => panic!("{label}: {other:?}"),
        }
    }

    let no_segments = parse_layout(&[]).unwrap_err().to_string();
    assert!(no_segments.contains("no segments"), "{no_segments}");
    let many: Vec<pb::Node> = (0..=MAX_SEGMENTS)
        .map(|i| pb::Node {
            name: segment_name(i),
            r#type: pb::NodeType::File as i32,
            sparse_extents: vec![extent(0, 0)],
            ..Default::default()
        })
        .collect();
    let too_many = parse_layout(&many).unwrap_err().to_string();
    assert!(too_many.contains("more than 64"), "{too_many}");
    assert!(parse_layout(&base_segments()).is_ok());
}

#[test]
fn image_layout_accepts_the_largest_image() {
    let nodes: Vec<pb::Node> = (0..MAX_SEGMENTS)
        .map(|i| pb::Node {
            name: segment_name(i),
            r#type: pb::NodeType::File as i32,
            size: SEGMENT_CHUNKS * C,
            sparse_extents: vec![extent(0, 0)],
            ..Default::default()
        })
        .collect();
    let layout = parse_layout(&nodes).unwrap();
    assert_eq!(layout.size, MAX_IMAGE_SIZE);
    let head = merge_head(&layout, b"x.img");
    assert_eq!(head.size, MAX_IMAGE_SIZE);
    assert_eq!(head.sparse_extents, vec![extent(0, 0)]);
}

#[test]
fn image_worst_segment_fits_a_tree_blob() {
    let chunks: Vec<ChunkRef> = (0..SEGMENT_CHUNKS)
        .map(|i| {
            let mut id = [0u8; 32];
            id[..8].copy_from_slice(&i.to_le_bytes());
            id[8] = 0xff;
            if i < SEGMENT_CHUNKS / 2 && i % 2 == 0 {
                ChunkRef::Zero
            } else {
                ChunkRef::Data(id)
            }
        })
        .collect();
    let nodes = segment_nodes(SEGMENT_CHUNKS * C, &chunks, SEGMENT_CHUNKS).unwrap();
    assert_eq!(nodes.len(), 1);
    let blob = encode_tree(nodes.clone());
    assert!(
        blob.len() < lodestore_format::pack::MAX_TREE_ULEN,
        "{}",
        blob.len()
    );
    assert_eq!(parse_layout(&nodes).unwrap().chunk_table(), chunks);
}

#[test]
fn image_source_keys() {
    let long = "a".repeat(200);
    for name in ["disk", "vm-101.raw", "a b", "ü", "100%", long.as_str()] {
        let key = SourceKey::image(name).unwrap();
        assert_eq!(key.kind(), SourceType::Image);
        assert!(key.as_str().starts_with("image/"));
        assert_eq!(SourceKey::parse(key.as_str()).unwrap(), key);
        assert_eq!(source::label(&key, &Meta::new()).as_deref(), Some(name));
    }
    assert_eq!(SourceKey::image("a b").unwrap().as_str(), "image/a%20b");
    let too_long = "a".repeat(201);
    for name in [
        "",
        too_long.as_str(),
        "a/b",
        ".",
        "..",
        "a\nb",
        "a\u{7f}",
        "a\0b",
    ] {
        assert!(!valid_image_name(name), "{name:?}");
        assert!(
            matches!(SourceKey::image(name), Err(Error::InvalidInput(_))),
            "{name:?}"
        );
    }
    // Every accepted name fits its source key: 84 two-byte characters encode to 504 bytes.
    let wide = "ü".repeat(84);
    assert!(valid_image_name(&wide));
    assert!(SourceKey::image(&wide).is_ok());
    let wider = "ü".repeat(85);
    assert!(!valid_image_name(&wider));
    assert!(SourceKey::image(&wider).is_err());
    let ascii = "a".repeat(MAX_IMAGE_NAME_LEN);
    assert!(valid_image_name(&ascii) && SourceKey::image(&ascii).is_ok());
    for text in [
        "image",
        "image/",
        "image/a/b",
        "image/a%2fb",
        "image/%2E%2E",
        "image/a%0Ab",
    ] {
        assert!(SourceKey::parse(text).is_err(), "{text}");
    }
    assert_eq!(SourceType::parse("image").unwrap(), SourceType::Image);
    assert_eq!(SourceType::ALL.len(), 7);
    assert_eq!(SourceType::ALL.last(), Some(&SourceType::Image));
    assert_eq!(SourceType::Image.as_str(), "image");
}

fn names() -> impl Strategy<Value = String> {
    prop::collection::vec(any::<char>(), 1..=40)
        .prop_map(|c| c.into_iter().collect::<String>())
        .prop_filter("a valid image name", |n| valid_image_name(n))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]


    #[test]
    fn image_name_rule_matches_the_source_key(chars in prop::collection::vec(any::<char>(), 1..=210)) {
        let name: String = chars.into_iter().collect();
        prop_assert_eq!(valid_image_name(&name), SourceKey::image(&name).is_ok(), "{:?}", name);
    }
    #[test]
    fn image_keys_are_injective(a in names(), b in names(), same in any::<bool>()) {
        let b = if same { a.clone() } else { b };
        let (ka, kb) = (SourceKey::image(&a).unwrap(), SourceKey::image(&b).unwrap());
        prop_assert_eq!(ka == kb, a == b);
        prop_assert_eq!(SourceKey::parse(ka.as_str()).unwrap(), ka);
    }

    #[test]
    fn image_meta_round_trips(size in 0..=MAX_IMAGE_SIZE, incremental in any::<bool>(), name in names()) {
        let read_mode = if incremental { ReadMode::Incremental } else { ReadMode::Full };
        let meta = ImageMeta { read_mode, disk_size: size };
        let written = meta.to_meta(&name);
        prop_assert_eq!(ImageMeta::from_meta(&written, &name).unwrap(), meta);
        prop_assert!(source::encode(&SourceKey::image(&name).unwrap(), &written).is_ok());
    }
}

#[test]
fn image_meta_round_trips_and_rejects() {
    let good = ImageMeta {
        read_mode: ReadMode::Full,
        disk_size: 20 * MIB,
    }
    .to_meta("disk");
    let size_key = disk_meta_key("disk", DISK_SIZE);
    assert_eq!(good.keys().map(String::as_str).collect::<Vec<_>>().len(), 4);
    assert_eq!(good.get(&size_key).map(String::as_str), Some("20971520"));
    assert!(source::encode(&SourceKey::image("disk").unwrap(), &good).is_ok());

    let with = |key: &str, value: &str| {
        let mut meta = good.clone();
        meta.insert(key.to_owned(), value.to_owned());
        meta
    };
    let without = |key: &str| {
        let mut meta = good.clone();
        meta.remove(key);
        meta
    };
    let too_big = (MAX_IMAGE_SIZE + 1).to_string();
    let rejected = [
        with("image_layout", "2"),
        with("image_layout", "x"),
        without("image_layout"),
        with("chunk_size", "1048576"),
        without("chunk_size"),
        with("read_mode", "bogus"),
        without("read_mode"),
        with(&size_key, "01"),
        with(&size_key, "+5"),
        with(&size_key, ""),
        with(&size_key, &too_big),
        with(&size_key, "18446744073709551616"),
        without(&size_key),
    ];
    for meta in rejected {
        assert!(
            matches!(
                ImageMeta::from_meta(&meta, "disk"),
                Err(Error::Corrupt { .. })
            ),
            "{meta:?}"
        );
    }
    let newer = ImageMeta::from_meta(&with("image_layout", "2"), "disk")
        .unwrap_err()
        .to_string();
    assert!(
        newer.contains("layout 2 is newer than this build reads"),
        "{newer}"
    );
    let malformed = ImageMeta::from_meta(&with("image_layout", "x"), "disk")
        .unwrap_err()
        .to_string();
    assert!(
        malformed.contains("missing or malformed") && !malformed.contains("newer"),
        "{malformed}"
    );
    assert!(ImageMeta::from_meta(&good, "other").is_err());

    let mut later = with("read_mode", "incremental");
    later.insert(disk_meta_key("disk", "bitmap_gen"), "3".into());
    later.insert("vm_uuid".into(), "x".into());
    let parsed = ImageMeta::from_meta(&later, "disk").unwrap();
    assert_eq!(parsed.read_mode, ReadMode::Incremental);
    assert_eq!(parsed.disk_size, 20 * MIB);
}

const ZERO_BLOCK: u64 = 64 * 1024;
const FIXED_TIME: pb::Timestamp = pb::Timestamp {
    seconds: 1_700_000_000,
    nanos: 0,
};

/// An image in memory with a dirty bitmap: the byte ranges in `dirty` are
/// reported dirty, and every 64 KiB block of zero bytes is reported as a hole.
struct DirtySource {
    data: Vec<u8>,
    dirty: Vec<(u64, u64)>,
    has_dirty: bool,
    bytes_read: u64,
    extent_calls: u32,
    read_calls: u32,
}

impl DirtySource {
    fn new(data: Vec<u8>, dirty: Vec<(u64, u64)>) -> Self {
        DirtySource {
            data,
            dirty,
            has_dirty: true,
            bytes_read: 0,
            extent_calls: 0,
            read_calls: 0,
        }
    }

    fn untouched(&self) -> bool {
        self.extent_calls == 0 && self.read_calls == 0
    }
}

impl ImageSource for DirtySource {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }

    async fn extents(&mut self, offset: u64, len: u64) -> Result<Vec<ImageExtent>, Error> {
        self.extent_calls += 1;
        let end = offset + len;
        let mut out = Vec::new();
        let mut pos = offset;
        while pos < end {
            let mut next = end.min((pos / ZERO_BLOCK + 1) * ZERO_BLOCK);
            for &(s, l) in &self.dirty {
                for edge in [s, s + l] {
                    if edge > pos {
                        next = next.min(edge);
                    }
                }
            }
            let piece = &self.data[pos as usize..next as usize];
            out.push(ImageExtent {
                len: next - pos,
                zero: piece.iter().all(|&b| b == 0),
                dirty: self.dirty.iter().any(|&(s, l)| s <= pos && pos < s + l),
            });
            pos = next;
        }
        Ok(out)
    }

    async fn read(&mut self, offset: u64, len: usize) -> Result<Vec<u8>, Error> {
        self.read_calls += 1;
        self.bytes_read += len as u64;
        let start = usize::try_from(offset).unwrap();
        Ok(self.data[start..start + len].to_vec())
    }

    fn has_dirty(&self) -> bool {
        self.has_dirty
    }
}

enum Failure {
    Error,
    Short,
}

struct FailingChunks {
    inner: Reader<LocalFsStore>,
    how: Failure,
}

impl ParentChunks for FailingChunks {
    fn chunk<'a>(
        &'a mut self,
        id: &'a BlobId,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, Error>> + Send + 'a>> {
        Box::pin(async move {
            match self.how {
                Failure::Error => Err(Error::InvalidInput("the parent is gone".into())),
                Failure::Short => {
                    let mut bytes = self.inner.chunk(id).await?;
                    bytes.pop();
                    Ok(bytes)
                }
            }
        })
    }
}

fn binding(generation: u64) -> BitmapBinding {
    BitmapBinding {
        name: format!("lode-test-g{generation}"),
        generation,
    }
}

fn fixed_time() -> BackupOptions<'static> {
    BackupOptions {
        time: Some(FIXED_TIME),
        ..BackupOptions::default()
    }
}

fn image_options(bitmap: Option<BitmapBinding>, full_reason: Option<FullReason>) -> ImageOptions {
    ImageOptions {
        segment_chunks: 2,
        bitmap,
        full_reason,
    }
}

async fn full_read(
    repo: &Path,
    source: &mut DirtySource,
    bitmap: Option<BitmapBinding>,
    full_reason: Option<FullReason>,
) -> ImageOutcome {
    let mut w = writer(repo).await;
    w.backup_image(
        "disk",
        source,
        None,
        fixed_time(),
        image_options(bitmap, full_reason),
        &|_| {},
    )
    .await
    .unwrap()
}

async fn parent_table(repo: &Path, parent: &BlobId) -> Vec<ChunkRef> {
    let mut r = reader(repo).await;
    load_image(&mut r, parent, "disk")
        .await
        .unwrap()
        .layout
        .chunk_table()
}

async fn incremental_with(
    repo: &Path,
    parent: BlobId,
    table: Vec<ChunkRef>,
    chunks: &mut dyn ParentChunks,
    source: &mut DirtySource,
    bitmap: Option<BitmapBinding>,
) -> Result<ImageOutcome, Error> {
    let mut w = writer(repo).await;
    w.backup_image(
        "disk",
        source,
        Some(ImageParent {
            snapshot_id: parent,
            table,
            chunks,
        }),
        fixed_time(),
        image_options(bitmap, None),
        &|_| {},
    )
    .await
}

async fn incremental(
    repo: &Path,
    parent: BlobId,
    source: &mut DirtySource,
    generation: u64,
) -> Result<ImageOutcome, Error> {
    let table = parent_table(repo, &parent).await;
    let mut r = reader(repo).await;
    incremental_with(
        repo,
        parent,
        table,
        &mut r,
        source,
        Some(binding(generation)),
    )
    .await
}

async fn fresh_repo(tmp: &Path) -> std::path::PathBuf {
    let repo = tmp.join("repo");
    fs::create_dir(&repo).unwrap();
    init_repo(&repo).await;
    repo
}

async fn restored(repo: &Path, id: &BlobId, out: &Path) -> Vec<u8> {
    let mut r = reader(repo).await;
    let report = r
        .restore_image(id, "disk", out, &RestoreOptions::default(), &|_| {})
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    fs::read(out.join("disk.img")).unwrap()
}

async fn snapshot_count(repo: &Path) -> usize {
    reader(repo)
        .await
        .list_snapshots()
        .await
        .unwrap()
        .entries
        .len()
}

fn overwrite(data: &mut [u8], at: u64, len: u64, seed: u8) {
    data[at as usize..(at + len) as usize].copy_from_slice(&random_bytes(len as usize, seed));
}

#[tokio::test]
async fn image_incremental_reuses_parent_chunks() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = fresh_repo(tmp.path()).await;
    let mut data = random_bytes((3 * C) as usize, 3);
    let parent = full_read(
        &repo,
        &mut DirtySource::new(data.clone(), Vec::new()),
        Some(binding(2)),
        None,
    )
    .await;

    let at = C + 2 * MIB;
    overwrite(&mut data, at, 4096, 4);
    let mut source = DirtySource::new(data.clone(), vec![(at, ZERO_BLOCK)]);
    let done = incremental(&repo, parent.outcome.snapshot_id, &mut source, 3)
        .await
        .unwrap();
    assert_eq!(done.outcome.stats.bytes_read, ZERO_BLOCK);
    assert_eq!(source.bytes_read, ZERO_BLOCK);
    assert_eq!(source.read_calls, 1);

    let r = reader(&repo).await;
    let snapshot = r.snapshot(&done.outcome.snapshot_id).await.unwrap();
    assert_eq!(snapshot.parent, parent.outcome.snapshot_id.to_vec());
    let (_, meta) = source::decode(snapshot.source.as_ref()).unwrap();
    assert_eq!(
        ImageMeta::from_meta(&meta, "disk").unwrap().read_mode,
        ReadMode::Incremental
    );
    let parent_refs = r
        .snapshot(&parent.outcome.snapshot_id)
        .await
        .unwrap()
        .index_refs;
    assert!(snapshot.index_refs.iter().any(|s| parent_refs.contains(s)));

    let full = full_read(
        &repo,
        &mut DirtySource::new(data.clone(), Vec::new()),
        None,
        None,
    )
    .await;
    assert_eq!(done.outcome.root_tree, full.outcome.root_tree);
    let out = tmp.path().join("out");
    assert!(restored(&repo, &done.outcome.snapshot_id, &out).await == data);
}

#[tokio::test]
async fn image_incremental_merges_the_short_last_chunk() {
    let tail_start = 2 * C;
    for tail_is_data in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let repo = fresh_repo(tmp.path()).await;
        let size = 2 * C + MIB + 512;
        let mut data = random_bytes(size as usize, 5);
        if !tail_is_data {
            data[tail_start as usize..].fill(0);
        }
        let parent = full_read(
            &repo,
            &mut DirtySource::new(data.clone(), Vec::new()),
            Some(binding(2)),
            None,
        )
        .await;

        let middle = tail_start + 512 * 1024;
        let last = size - 4096;
        overwrite(&mut data, middle, 4096, 6);
        overwrite(&mut data, last, 4096, 7);
        let mut source = DirtySource::new(data.clone(), vec![(middle, 4096), (last, 4096)]);
        let done = incremental(&repo, parent.outcome.snapshot_id, &mut source, 3)
            .await
            .unwrap();
        assert_eq!(source.bytes_read, 8192, "tail_is_data={tail_is_data}");
        assert_eq!(done.outcome.stats.bytes_read, 8192);

        let full = full_read(
            &repo,
            &mut DirtySource::new(data.clone(), Vec::new()),
            None,
            None,
        )
        .await;
        assert_eq!(done.outcome.root_tree, full.outcome.root_tree);
        let out = tmp.path().join("out");
        assert!(restored(&repo, &done.outcome.snapshot_id, &out).await == data);
    }
}

#[tokio::test]
async fn image_incremental_requires_parent_chunks_in_index() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = fresh_repo(tmp.path()).await;
    let data = random_bytes((2 * C) as usize, 8);
    let parent = full_read(
        &repo,
        &mut DirtySource::new(data.clone(), Vec::new()),
        Some(binding(2)),
        None,
    )
    .await;
    let mut table = parent_table(&repo, &parent.outcome.snapshot_id).await;
    table[1] = ChunkRef::Data([9; 32]);
    let before = snapshot_count(&repo).await;

    let mut source = DirtySource::new(data, Vec::new());
    let mut r = reader(&repo).await;
    let err = incremental_with(
        &repo,
        parent.outcome.snapshot_id,
        table,
        &mut r,
        &mut source,
        Some(binding(3)),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)), "{err:?}");
    assert_eq!(snapshot_count(&repo).await, before);
}

fn pack_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            pack_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

#[tokio::test]
async fn image_incremental_parent_chunk_failure_stops_the_run() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = fresh_repo(tmp.path()).await;
    let mut other = random_bytes((2 * C) as usize, 20);
    other.truncate((2 * C) as usize);
    let mut w = writer(&repo).await;
    w.backup_image(
        "other",
        &mut DirtySource::new(other, Vec::new()),
        None,
        fixed_time(),
        image_options(None, None),
        &|_| {},
    )
    .await
    .unwrap();
    let mut packs_before = Vec::new();
    pack_files(&repo.join("packs"), &mut packs_before);

    let mut data = random_bytes((2 * C) as usize, 21);
    let parent = full_read(
        &repo,
        &mut DirtySource::new(data.clone(), Vec::new()),
        Some(binding(2)),
        None,
    )
    .await;
    let mut packs_after = Vec::new();
    pack_files(&repo.join("packs"), &mut packs_after);
    let parent_packs: Vec<_> = packs_after
        .iter()
        .filter(|p| !packs_before.contains(p))
        .collect();
    assert!(!parent_packs.is_empty());
    let other_pack = packs_before[0].clone();

    overwrite(&mut data, 4096, 4096, 22);
    let table = parent_table(&repo, &parent.outcome.snapshot_id).await;
    let before = snapshot_count(&repo).await;

    for how in [Failure::Error, Failure::Short] {
        let short = matches!(how, Failure::Short);
        let mut chunks = FailingChunks {
            inner: reader(&repo).await,
            how,
        };
        let mut source = DirtySource::new(data.clone(), vec![(4096, 4096)]);
        let err = incremental_with(
            &repo,
            parent.outcome.snapshot_id,
            table.clone(),
            &mut chunks,
            &mut source,
            Some(binding(3)),
        )
        .await
        .unwrap_err();
        if short {
            assert!(matches!(err, Error::Corrupt { .. }), "{err:?}");
        }
        assert_eq!(snapshot_count(&repo).await, before);
    }

    let foreign = fs::read(&other_pack).unwrap();
    for pack in parent_packs {
        fs::write(pack, &foreign).unwrap();
    }
    let mut source = DirtySource::new(data, vec![(4096, 4096)]);
    let mut r = reader(&repo).await;
    incremental_with(
        &repo,
        parent.outcome.snapshot_id,
        table,
        &mut r,
        &mut source,
        Some(binding(3)),
    )
    .await
    .unwrap_err();
    assert_eq!(snapshot_count(&repo).await, before);
}

#[tokio::test]
async fn image_incremental_rejects_a_mismatched_table() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = fresh_repo(tmp.path()).await;
    let data = random_bytes((2 * C) as usize, 9);
    let parent = full_read(
        &repo,
        &mut DirtySource::new(data.clone(), Vec::new()),
        Some(binding(2)),
        None,
    )
    .await;
    let table = parent_table(&repo, &parent.outcome.snapshot_id).await;
    let mut longer = table.clone();
    longer.push(ChunkRef::Zero);
    let shorter = table[..1].to_vec();
    for bad in [longer, shorter] {
        let mut source = DirtySource::new(data.clone(), Vec::new());
        let mut r = reader(&repo).await;
        let err = incremental_with(
            &repo,
            parent.outcome.snapshot_id,
            bad,
            &mut r,
            &mut source,
            Some(binding(3)),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)), "{err:?}");
        assert!(source.untouched());
    }
}

#[tokio::test]
async fn image_incremental_requires_a_dirty_source() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = fresh_repo(tmp.path()).await;
    let data = random_bytes(C as usize, 10);
    let parent = full_read(
        &repo,
        &mut DirtySource::new(data.clone(), Vec::new()),
        Some(binding(2)),
        None,
    )
    .await;
    let table = parent_table(&repo, &parent.outcome.snapshot_id).await;

    let mut blind = DirtySource::new(data.clone(), Vec::new());
    blind.has_dirty = false;
    let mut r = reader(&repo).await;
    let err = incremental_with(
        &repo,
        parent.outcome.snapshot_id,
        table.clone(),
        &mut r,
        &mut blind,
        Some(binding(3)),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)), "{err:?}");
    assert!(blind.untouched());

    let mut source = DirtySource::new(data, Vec::new());
    let err = incremental_with(
        &repo,
        parent.outcome.snapshot_id,
        table,
        &mut r,
        &mut source,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)), "{err:?}");
    assert!(source.untouched());
}

async fn meta_of(repo: &Path, done: &ImageOutcome) -> (Vec<u8>, Meta) {
    let r = reader(repo).await;
    let snapshot = r.snapshot(&done.outcome.snapshot_id).await.unwrap();
    let (_, meta) = source::decode(snapshot.source.as_ref()).unwrap();
    (snapshot.parent, meta)
}

#[tokio::test]
async fn image_incremental_meta_records_the_binding() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = fresh_repo(tmp.path()).await;
    let data = random_bytes(C as usize, 11);
    let plain = full_read(
        &repo,
        &mut DirtySource::new(data.clone(), Vec::new()),
        None,
        None,
    )
    .await;
    let (parent, meta) = meta_of(&repo, &plain).await;
    assert!(parent.is_empty());
    assert_eq!(BitmapBinding::from_meta(&meta, "disk").unwrap(), None);
    assert_eq!(FullReason::from_meta(&meta, "disk").unwrap(), None);
    assert_eq!(
        ImageMeta::from_meta(&meta, "disk").unwrap().read_mode,
        ReadMode::Full
    );

    let first = full_read(
        &repo,
        &mut DirtySource::new(data.clone(), Vec::new()),
        Some(binding(2)),
        Some(FullReason::NoState),
    )
    .await;
    let (parent, meta) = meta_of(&repo, &first).await;
    assert!(parent.is_empty());
    assert_eq!(
        BitmapBinding::from_meta(&meta, "disk").unwrap(),
        Some(binding(2))
    );
    assert_eq!(
        FullReason::from_meta(&meta, "disk").unwrap(),
        Some(FullReason::NoState)
    );
    assert_eq!(
        ImageMeta::from_meta(&meta, "disk").unwrap().read_mode,
        ReadMode::Full
    );

    let mut source = DirtySource::new(data, Vec::new());
    let next = incremental(&repo, first.outcome.snapshot_id, &mut source, 3)
        .await
        .unwrap();
    let (parent, meta) = meta_of(&repo, &next).await;
    assert_eq!(parent, first.outcome.snapshot_id.to_vec());
    assert_eq!(
        BitmapBinding::from_meta(&meta, "disk").unwrap(),
        Some(binding(3))
    );
    assert_eq!(FullReason::from_meta(&meta, "disk").unwrap(), None);
    assert_eq!(
        ImageMeta::from_meta(&meta, "disk").unwrap().read_mode,
        ReadMode::Incremental
    );
    assert_eq!(source.bytes_read, 0);
}
