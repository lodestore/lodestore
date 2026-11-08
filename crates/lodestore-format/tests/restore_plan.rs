//! Restoring from a plan: what the server walks and what the agent writes,
//! with nothing but a pack key and `k_id` between them.

use std::fs;
use std::ops::Range;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use lodestore_format::crypto::{BLOB_TYPE_DATA, BlobId, Entropy};
use lodestore_format::error::Error;
use lodestore_format::fs::restore::{
    Overwrite, RestoreFailure, RestoreSettings, RestoreStats, Restorer,
};
use lodestore_format::index::Index;
use lodestore_format::keys::KdfCost;
use lodestore_format::pack::pack_object_key;
use lodestore_format::pb;
use lodestore_format::reader::{Reader, open_chunk};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::selection::Selection;
use lodestore_format::store::{BlobError, BlobStore, ListEntry, LocalFsStore};
use lodestore_format::tree::{PlanStep, SelectionWalk};
use lodestore_format::writer::{BackupOptions, Writer};
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{Rng, SeedableRng};

const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

const ODD_NAME: &str = "!keep [1] #x";

type Read = (String, Range<u64>);

/// A `LocalFsStore` that remembers which ranges of which objects were read.
#[derive(Clone)]
struct Counting {
    inner: Arc<LocalFsStore>,
    reads: Arc<Mutex<Vec<Read>>>,
}

impl Counting {
    fn new(inner: LocalFsStore) -> Self {
        Counting {
            inner: Arc::new(inner),
            reads: Arc::default(),
        }
    }

    fn ranges(&self) -> Vec<Read> {
        self.reads.lock().unwrap().clone()
    }
}

impl BlobStore for Counting {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<(), BlobError> {
        self.inner.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        self.reads
            .lock()
            .unwrap()
            .push((key.to_owned(), 0..u64::MAX));
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.reads
            .lock()
            .unwrap()
            .push((key.to_owned(), range.clone()));
        self.inner.get_range(key, range).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        self.inner.list(prefix).await
    }
}

fn write_file(path: &Path, data: &[u8], mode: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn random(len: usize, seed: u8) -> Vec<u8> {
    let mut rng = ChaCha20Rng::from_seed([seed; 32]);
    let mut out = vec![0u8; len];
    rng.fill_bytes(&mut out);
    out
}

struct Fx {
    _tmp: tempfile::TempDir,
    repo_dir: PathBuf,
    /// Canonical path of the backed-up tree.
    src: PathBuf,
    out: PathBuf,
    snapshot: BlobId,
}

impl Fx {
    async fn new() -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let repo_dir = root.join("repo");
        fs::create_dir(&repo_dir).unwrap();
        let src = root.join("src");
        write_file(&src.join("a/x"), b"content of a/x", 0o640);
        write_file(&src.join("a/y"), b"content of a/y", 0o644);
        write_file(&src.join("b/z"), b"content of b/z", 0o644);
        write_file(&src.join("c/deep/w"), &random(3 << 20, 5), 0o600);
        write_file(&src.join("odd").join(ODD_NAME), b"odd one", 0o755);

        let store = LocalFsStore::new(repo_dir.clone());
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
        let repo = Repository::open(&store, b"pw").await.unwrap();
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
        Fx {
            out: root.join("out"),
            _tmp: tmp,
            repo_dir,
            src,
            snapshot: outcome.snapshot_id,
        }
    }

    fn store(&self) -> LocalFsStore {
        LocalFsStore::new(self.repo_dir.clone())
    }

    async fn reader<S: BlobStore>(&self, store: S) -> Reader<S> {
        let local = self.store();
        let repo = Repository::open(&local, b"pw").await.unwrap();
        Reader::open(store, repo.read_keys().duplicate(), repo.config()).unwrap()
    }

    /// Absolute snapshot path of `rel` below the backed-up tree.
    fn abs(&self, rel: &str) -> Vec<u8> {
        let mut p = self.src.as_os_str().as_encoded_bytes().to_vec();
        if !rel.is_empty() {
            p.push(b'/');
            p.extend_from_slice(rel.as_bytes());
        }
        p
    }

    fn selection(&self, rels: &[&str]) -> Selection {
        Selection::from_paths(rels.iter().map(|r| self.abs(r))).unwrap()
    }

    /// Where a backed-up path lands below the target.
    fn restored(&self, target: &Path, rel: &str) -> PathBuf {
        let mut p = target.join(self.src.strip_prefix("/").unwrap());
        if !rel.is_empty() {
            p.push(rel);
        }
        p
    }

    fn settings<S: BlobStore>(&self, reader: &Reader<S>) -> RestoreSettings {
        RestoreSettings {
            overwrite: Overwrite::IfChanged,
            verify: true,
            dry_run: false,
            k_id: zeroize::Zeroizing::new(*reader.keys().k_id()),
            seed: reader.keys().cdc_seed(),
            restore_owners: true,
            keep_privileges: true,
        }
    }
}

struct Outcome {
    stats: RestoreStats,
    failures: Vec<RestoreFailure>,
}

fn dir_node(name: &str, mode: u32, mtime: i64) -> pb::Node {
    pb::Node {
        name: name.as_bytes().to_vec(),
        r#type: pb::NodeType::Dir as i32,
        mode,
        uid: rustix::process::getuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
        mtime: Some(pb::Timestamp {
            seconds: mtime,
            nanos: 0,
        }),
        ..Default::default()
    }
}

fn file_node(name: &str, data: &[u8], uid: u32) -> pb::Node {
    pb::Node {
        name: name.as_bytes().to_vec(),
        r#type: pb::NodeType::File as i32,
        mode: 0o644,
        uid,
        gid: rustix::process::getgid().as_raw(),
        size: data.len() as u64,
        ..Default::default()
    }
}

fn plain_settings(overwrite: Overwrite, restore_owners: bool) -> RestoreSettings {
    RestoreSettings {
        overwrite,
        verify: false,
        dry_run: false,
        k_id: zeroize::Zeroizing::new([0; 32]),
        seed: 0,
        restore_owners,
        keep_privileges: true,
    }
}

fn put(restorer: &mut Restorer, node: &pb::Node, data: &[u8]) {
    let mut sink = restorer.begin_file(node).unwrap().unwrap();
    sink.write(data).unwrap();
    restorer.finish_file(sink, node).unwrap();
}

fn set_dir_times(path: &Path, secs: u64) {
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
    let dir = fs::File::open(path).unwrap();
    dir.set_times(fs::FileTimes::new().set_accessed(t).set_modified(t))
        .unwrap();
}

fn mtime_secs(path: &Path) -> u64 {
    fs::metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Fetches and opens one chunk with the plan's facts alone.
async fn fetch_chunk<S: BlobStore>(
    reader: &mut Reader<S>,
    id: &BlobId,
    loc: &lodestore_format::index::BlobLocation,
    k_id: &[u8; 32],
) -> Result<Vec<u8>, Error> {
    let pack_key = reader.pack_key(&loc.pack_id).await?;
    let start = u64::from(loc.offset);
    let frame = reader
        .store()
        .get_range(
            &pack_object_key(&loc.pack_id),
            start..start + u64::from(loc.enc_len),
        )
        .await?;
    open_chunk(
        &pack_key,
        reader.keys().repo_id(),
        BLOB_TYPE_DATA,
        id,
        &frame,
        loc.ulen,
        k_id,
    )
}

/// What the agent does with a plan: no index, no trees, no `Reader::restore`.
async fn agent_restore<S: BlobStore>(
    reader: &mut Reader<S>,
    snapshot: &BlobId,
    selection: &Selection,
    target: &Path,
    settings: RestoreSettings,
) -> Outcome {
    let mut walk = SelectionWalk::start(reader, snapshot, selection)
        .await
        .unwrap();
    let k_id = settings.k_id.clone();
    let mut restorer = Restorer::open(target, settings).unwrap();
    while let Some(step) = walk.next(reader).await.unwrap() {
        let node = match step {
            PlanStep::Enter {
                node,
                ancestor: true,
            } => {
                restorer.enter_ancestor(&node).unwrap();
                continue;
            }
            PlanStep::Enter { node, .. } => {
                restorer.enter_dir(&node).unwrap();
                continue;
            }
            PlanStep::Leave => {
                restorer.leave_dir();
                continue;
            }
            PlanStep::Entry(node) => node,
        };
        if node.r#type != pb::NodeType::File as i32 {
            restorer.create_other(&node).unwrap();
            continue;
        }
        if restorer.link_if_seen(&node).unwrap() {
            continue;
        }
        let Some(mut sink) = restorer.begin_file(&node).unwrap() else {
            continue;
        };
        let locations = reader.chunk_locations(&node).await.unwrap();
        for (raw, loc) in node.content.iter().zip(&locations) {
            let id: BlobId = raw.as_slice().try_into().unwrap();
            let plain = fetch_chunk(reader, &id, loc, &k_id).await.unwrap();
            sink.write(&plain).unwrap();
        }
        restorer.finish_file(sink, &node).unwrap();
    }
    Outcome {
        stats: restorer.take_stats(),
        failures: restorer.take_failures(),
    }
}

fn same_file(a: &Path, b: &Path) {
    assert_eq!(
        fs::read(a).unwrap(),
        fs::read(b).unwrap(),
        "{}",
        b.display()
    );
    assert_eq!(
        fs::metadata(a).unwrap().permissions().mode() & 0o7777,
        fs::metadata(b).unwrap().permissions().mode() & 0o7777,
        "{}",
        b.display()
    );
}

#[tokio::test]
async fn restore_plan_rebuilds_selected_files() {
    let fx = Fx::new().await;
    let mut reader = fx.reader(fx.store()).await;
    let selection = fx.selection(&["a/x", "c", &format!("odd/{ODD_NAME}")]);
    let settings = fx.settings(&reader);

    let out = agent_restore(&mut reader, &fx.snapshot, &selection, &fx.out, settings).await;
    assert!(out.failures.is_empty(), "{:?}", out.failures);
    assert_eq!(out.stats.restored, 3);

    for rel in ["a/x", "c/deep/w", &format!("odd/{ODD_NAME}")] {
        same_file(&fx.src.join(rel), &fx.restored(&fx.out, rel));
    }
    assert!(!fx.restored(&fx.out, "b").exists());
    assert!(!fx.restored(&fx.out, "a/y").exists());
}

#[tokio::test]
async fn restore_plan_skips_unselected_subtrees() {
    let fx = Fx::new().await;
    let mut plain = fx.reader(fx.store()).await;
    let top = plain.list_dir(&fx.snapshot, &fx.abs("")).await.unwrap();
    let tree_ids = |name: &str| -> Vec<BlobId> {
        let node = top.iter().find(|n| n.name == name.as_bytes()).unwrap();
        node.subtrees
            .iter()
            .map(|id| id.as_slice().try_into().unwrap())
            .collect()
    };

    let local = fx.store();
    let repo = Repository::open(&local, b"pw").await.unwrap();
    let mut index = Index::default();
    index
        .load_all(&local, repo.read_keys().k_idx(), repo.read_keys().repo_id())
        .await
        .unwrap();
    let frames = |name: &str| -> Vec<Read> {
        tree_ids(name)
            .iter()
            .map(|id| {
                let loc = index
                    .get(id, lodestore_format::crypto::BLOB_TYPE_TREE)
                    .unwrap();
                let start = u64::from(loc.offset);
                (
                    pack_object_key(&loc.pack_id),
                    start..start + u64::from(loc.enc_len),
                )
            })
            .collect()
    };
    let unwanted: Vec<_> = frames("b").into_iter().chain(frames("c")).collect();
    let wanted = frames("a");

    let counting = Counting::new(fx.store());
    let mut reader = fx.reader(counting.clone()).await;
    let selection = fx.selection(&["a/x"]);
    let settings = fx.settings(&reader);
    let out = agent_restore(&mut reader, &fx.snapshot, &selection, &fx.out, settings).await;
    assert_eq!(out.stats.restored, 1);

    let reads = counting.ranges();
    for frame in &unwanted {
        assert!(!reads.contains(frame), "read the tree of an unselected dir");
    }
    for frame in &wanted {
        assert!(reads.contains(frame), "never read the tree of a");
    }
}

#[tokio::test]
async fn restore_plan_open_chunk_rejects_wrong_id() {
    let fx = Fx::new().await;
    let mut reader = fx.reader(fx.store()).await;
    let k_id = *reader.keys().k_id();
    let repo_id = *reader.keys().repo_id();
    let entries = reader.list_dir(&fx.snapshot, &fx.abs("a")).await.unwrap();
    let node = entries.iter().find(|n| n.name == b"x").unwrap();
    let id: BlobId = node.content[0].as_slice().try_into().unwrap();
    let loc = reader.chunk_locations(node).await.unwrap().remove(0);

    let plain = fetch_chunk(&mut reader, &id, &loc, &k_id).await.unwrap();
    assert_eq!(plain, b"content of a/x");

    let pack_key = reader.pack_key(&loc.pack_id).await.unwrap();
    let start = u64::from(loc.offset);
    let frame = reader
        .store()
        .get_range(
            &pack_object_key(&loc.pack_id),
            start..start + u64::from(loc.enc_len),
        )
        .await
        .unwrap();
    let open = |id: &BlobId, frame: &[u8], k_id: &[u8; 32]| {
        open_chunk(
            &pack_key,
            &repo_id,
            BLOB_TYPE_DATA,
            id,
            frame,
            loc.ulen,
            k_id,
        )
    };

    let mut other = id;
    other[0] ^= 1;
    assert!(matches!(
        open(&other, &frame, &k_id),
        Err(Error::Corrupt { .. })
    ));
    assert!(open(&id, &frame[..frame.len() - 1], &k_id).is_err());
    assert!(open(&id, &[], &k_id).is_err());
    let mut flipped = frame.to_vec();
    flipped[frame.len() / 2] ^= 1;
    assert!(open(&id, &flipped, &k_id).is_err());
    let mut wrong_key = k_id;
    wrong_key[0] ^= 1;
    assert!(matches!(
        open(&id, &frame, &wrong_key),
        Err(Error::Corrupt { .. })
    ));
    assert!(
        open_chunk(
            &pack_key,
            &[0; 16],
            BLOB_TYPE_DATA,
            &id,
            &frame,
            loc.ulen,
            &k_id
        )
        .is_err()
    );
    assert!(
        open_chunk(
            &pack_key,
            &repo_id,
            BLOB_TYPE_DATA,
            &id,
            &frame,
            loc.ulen + 1,
            &k_id
        )
        .is_err()
    );
}

#[test]
fn selection_rejects_dot_dot() {
    for bad in [&b"/a/../b"[..], b"a/b", b"/a/./b", b"/a/b\0", b"/.."] {
        let err = Selection::from_paths([bad.to_vec()]).unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)), "{bad:?}: {err:?}");
    }
    let deep = format!("/{}", "a/".repeat(5000)).into_bytes();
    assert!(matches!(
        Selection::from_paths([deep]),
        Err(Error::InvalidInput(_))
    ));
    assert!(Selection::from_paths([b"/a/b/".to_vec(), b"//c".to_vec()]).is_ok());
}

fn me() -> u32 {
    rustix::process::getuid().as_raw()
}

#[test]
fn restore_plan_ancestors_keep_existing_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("t");
    fs::create_dir_all(target.join("p")).unwrap();
    fs::set_permissions(target.join("p"), fs::Permissions::from_mode(0o751)).unwrap();
    set_dir_times(&target.join("p"), 1_000_000_000);

    let mut restorer = Restorer::open(&target, plain_settings(Overwrite::IfChanged, true)).unwrap();
    restorer
        .enter_ancestor(&dir_node("p", 0o700, 1_700_000_000))
        .unwrap();
    put(&mut restorer, &file_node("f", b"abc", me()), b"abc");
    restorer
        .enter_ancestor(&dir_node("q", 0o750, 1_700_000_000))
        .unwrap();
    put(&mut restorer, &file_node("g", b"abc", me()), b"abc");
    restorer.leave_dir();
    restorer.leave_dir();
    assert!(restorer.take_failures().is_empty());

    let p = target.join("p");
    assert_eq!(
        fs::metadata(&p).unwrap().permissions().mode() & 0o7777,
        0o751
    );
    assert_eq!(mtime_secs(&p), 1_000_000_000);
    assert_eq!(fs::read(p.join("f")).unwrap(), b"abc");

    let q = p.join("q");
    assert_eq!(
        fs::metadata(&q).unwrap().permissions().mode() & 0o7777,
        0o750
    );
    assert_eq!(mtime_secs(&q), 1_700_000_000);
    assert_eq!(fs::read(q.join("g")).unwrap(), b"abc");
}

#[test]
fn restore_plan_unchanged_foreign_ancestor_gets_no_time_warning() {
    if me() == 0 {
        return;
    }
    let mut restorer = Restorer::open(Path::new("/"), plain_settings(Overwrite::Never, false))
        .expect("open / read-only");
    restorer
        .enter_ancestor(&dir_node("usr", 0o755, 1_700_000_000))
        .unwrap();
    restorer.leave_dir();
    assert!(restorer.take_failures().is_empty());
    let unapplied = restorer.take_unapplied();
    assert!(unapplied.is_empty(), "{unapplied:?}");
}

#[test]
fn restore_plan_ancestor_symlink_is_not_followed() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("t");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&target).unwrap();
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, target.join("link")).unwrap();
    fs::write(target.join("plain"), b"in the way").unwrap();

    let mut restorer = Restorer::open(&target, plain_settings(Overwrite::Always, true)).unwrap();
    for name in ["link", "plain"] {
        restorer
            .enter_ancestor(&dir_node(name, 0o755, 1_700_000_000))
            .unwrap();
        assert!(
            restorer
                .begin_file(&file_node("f", b"abc", me()))
                .unwrap()
                .is_none()
        );
        restorer.leave_dir();
    }

    let failed: Vec<_> = restorer
        .take_failures()
        .into_iter()
        .map(|f| f.path)
        .collect();
    assert_eq!(failed, [b"link".to_vec(), b"plain".to_vec()]);
    assert!(
        fs::symlink_metadata(target.join("link"))
            .unwrap()
            .is_symlink()
    );
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    assert_eq!(fs::read(target.join("plain")).unwrap(), b"in the way");
}

#[tokio::test]
async fn restore_plan_counts_created_overwritten_skipped() {
    let fx = Fx::new().await;
    let mut reader = fx.reader(fx.store()).await;
    let selection = fx.selection(&["a"]);
    let base = fx.settings(&reader);
    let with = |overwrite| RestoreSettings {
        overwrite,
        ..base.clone()
    };
    let counts = |s: &RestoreStats| (s.created, s.overwritten, s.skipped, s.restored);
    let x = fx.restored(&fx.out, "a/x");
    let y = fx.restored(&fx.out, "a/y");

    let out = agent_restore(
        &mut reader,
        &fx.snapshot,
        &selection,
        &fx.out,
        with(Overwrite::IfChanged),
    )
    .await;
    assert_eq!(counts(&out.stats), (2, 0, 0, 2));

    fs::write(&x, b"changed").unwrap();
    let out = agent_restore(
        &mut reader,
        &fx.snapshot,
        &selection,
        &fx.out,
        with(Overwrite::IfChanged),
    )
    .await;
    assert_eq!(counts(&out.stats), (0, 1, 1, 1));
    assert_eq!(fs::read(&x).unwrap(), b"content of a/x");

    fs::write(&x, b"changed").unwrap();
    fs::remove_file(&y).unwrap();
    let out = agent_restore(
        &mut reader,
        &fx.snapshot,
        &selection,
        &fx.out,
        with(Overwrite::Never),
    )
    .await;
    assert_eq!(counts(&out.stats), (1, 0, 1, 1));
    assert_eq!(fs::read(&x).unwrap(), b"changed");

    let out = agent_restore(
        &mut reader,
        &fx.snapshot,
        &selection,
        &fx.out,
        with(Overwrite::Always),
    )
    .await;
    assert_eq!(counts(&out.stats), (0, 2, 0, 2));
    assert_eq!(fs::read(&x).unwrap(), b"content of a/x");
}

#[test]
fn restore_plan_owners_off_skips_chown() {
    if rustix::process::getuid().is_root() {
        return;
    }
    for owners in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let mut restorer = Restorer::open(
            &tmp.path().join("t"),
            plain_settings(Overwrite::Always, owners),
        )
        .unwrap();
        put(&mut restorer, &file_node("f", b"abc", me() + 1), b"abc");
        let unapplied = restorer.take_unapplied();
        assert_eq!(
            unapplied.iter().any(|u| u.what == "owner"),
            owners,
            "{unapplied:?}"
        );
    }
}

#[tokio::test]
async fn restore_plan_never_deletes_extra_files() {
    let fx = Fx::new().await;
    let mut reader = fx.reader(fx.store()).await;
    let selection = fx.selection(&["a"]);
    let settings = fx.settings(&reader);

    let dir = fx.restored(&fx.out, "a");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("extra"), b"mine").unwrap();
    fs::write(fx.out.join("elsewhere"), b"mine too").unwrap();
    fs::write(dir.join("y"), b"older y").unwrap();

    let out = agent_restore(&mut reader, &fx.snapshot, &selection, &fx.out, settings).await;
    assert!(out.failures.is_empty(), "{:?}", out.failures);
    assert_eq!(fs::read(dir.join("extra")).unwrap(), b"mine");
    assert_eq!(fs::read(fx.out.join("elsewhere")).unwrap(), b"mine too");
    assert_eq!(fs::read(dir.join("x")).unwrap(), b"content of a/x");
    assert_eq!(fs::read(dir.join("y")).unwrap(), b"content of a/y");
}
