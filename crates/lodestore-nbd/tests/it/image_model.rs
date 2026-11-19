//! A model of a disk with dirty bitmaps and of one backup run as `lode` makes
//! it: the same `decide`, the same writer and, for the NBD transport, the same
//! client. The property in `image_props` compares every run with a full read of
//! the same disk state.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::future::Future;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use lodestore_format::crypto::{BlobId, Entropy, RepoId};
use lodestore_format::image::{
    BitmapNames, DecideInput, FullReason, IMAGE_CHUNK_SIZE, ImageExtent, ImageParent, ImageSource,
    ImageState, ParentChunks, ReadPlan, StateRead, decide, instance_snapshots, load_image,
    source_identity,
};
use lodestore_format::keys::KdfCost;
use lodestore_format::pb;
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::snapshot::SnapshotEntry;
use lodestore_format::source::SourceKey;
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, ImageOptions, Writer};
use lodestore_format::{Error, Result as FormatResult};
use lodestore_nbd::{Client, Limits, MemExport, NbdSource, serve};
use tokio::io::DuplexStream;
use tokio::task::JoinHandle;

pub const C: u64 = IMAGE_CHUNK_SIZE;
pub const MAX_DISK: u64 = 22 << 20;
pub const HOST: &[u8] = b"model-host";
const REFERENCE_HOST: &[u8] = b"reference";
const ZERO_BLOCK: u64 = 64 << 10;
const BASE_TIME: i64 = 1_700_000_000;
const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

pub fn key() -> SourceKey {
    SourceKey::image("disk").unwrap()
}

fn injected(what: &str) -> Error {
    Error::Io(std::io::Error::other(what.to_owned()))
}

pub struct Env {
    _dir: tempfile::TempDir,
    store: LocalFsStore,
    pub repo_id: RepoId,
}

impl Env {
    pub async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo");
        fs::create_dir(&path).unwrap();
        let store = LocalFsStore::new(path);
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
        let repo_id = *repo.read_keys().repo_id();
        Env {
            _dir: dir,
            store,
            repo_id,
        }
    }

    pub fn root(&self) -> &Path {
        self.store.root()
    }

    pub async fn writer(&self) -> Writer<&LocalFsStore> {
        let repo = Repository::open(&self.store, b"pw").await.unwrap();
        let keys = repo.read_keys().write_keys(repo.config_hash());
        Writer::new(&self.store, keys, repo.config()).await.unwrap()
    }

    pub async fn reader(&self) -> Reader<&LocalFsStore> {
        let repo = Repository::open(&self.store, b"pw").await.unwrap();
        Reader::open(&self.store, repo.read_keys().duplicate(), repo.config()).unwrap()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WriteOp {
    pub offset: u64,
    pub len: u64,
    pub fill: Option<u8>,
}

fn fill_bytes(target: &mut [u8], seed: u8) {
    for (i, b) in target.iter_mut().enumerate() {
        let i = i as u64;
        *b = ((u64::from(seed) * 131 + i * 31 + (i >> 9)) % 251 + 1) as u8;
    }
}

/// A bitmap script that rotates: the bitmap `g{n}` is read, `g{n+1}` collects
/// what changes meanwhile, and a resize drops them all as qemu does.
pub struct Bitmaps {
    pub n: u64,
    granule: u64,
    gens: BTreeMap<u64, BTreeSet<u64>>,
    disabled: BTreeSet<u64>,
}

impl Bitmaps {
    pub fn new(granule: u64) -> Self {
        Bitmaps {
            n: 1,
            granule,
            gens: BTreeMap::new(),
            disabled: BTreeSet::new(),
        }
    }

    pub fn ensure(&mut self, generation: u64) {
        self.gens.entry(generation).or_default();
    }

    /// What `qemu-img bitmap --disable` leaves: the bitmap is still offered and records nothing.
    pub fn disable(&mut self, generation: u64) {
        self.disabled.insert(generation);
    }

    pub fn exists(&self, generation: u64) -> bool {
        self.gens.contains_key(&generation)
    }

    fn mark(&mut self, offset: u64, len: u64) {
        let first = offset / self.granule;
        let last = (offset + len).div_ceil(self.granule);
        for (generation, set) in &mut self.gens {
            if !self.disabled.contains(generation) {
                set.extend(first..last);
            }
        }
    }

    pub fn ranges(&self, generation: u64, size: u64) -> Vec<Range<u64>> {
        let mut out: Vec<Range<u64>> = Vec::new();
        for &index in self.gens.get(&generation).into_iter().flatten() {
            let start = index * self.granule;
            if start >= size {
                break;
            }
            let end = (start + self.granule).min(size);
            match out.last_mut() {
                Some(last) if last.end == start => last.end = end,
                _ => out.push(start..end),
            }
        }
        out
    }

    /// What the old contract does after a run: the one bitmap is cleared, so
    /// every write between the end of the read and the clear is lost.
    pub fn clear_current(&mut self) {
        self.gens.entry(self.n).or_default().clear();
    }

    /// What a resize that recreates the bitmaps under the names they had
    /// would do: the generation is not skipped, nothing is marked dirty.
    pub fn recreate_under_same_names(&mut self, n: u64) {
        self.gens.clear();
        self.n = n;
        self.ensure(n);
        self.ensure(n + 1);
    }

    fn rotate(&mut self) {
        self.gens.remove(&self.n);
        self.n += 1;
    }

    fn reset_after_resize(&mut self) {
        self.gens.clear();
        self.n += 1;
        self.ensure(self.n);
        self.ensure(self.n + 1);
    }
}

#[derive(Default)]
pub struct Machine {
    pub state: Option<Vec<u8>>,
    pub history: Vec<Vec<u8>>,
}

pub struct World {
    pub disk: Vec<u8>,
    pub bitmaps: Bitmaps,
    pub machine: Machine,
    pub rejected: HashSet<BlobId>,
    pub clock: i64,
    pub last_time: Option<i64>,
}

impl World {
    pub fn new(disk: Vec<u8>, granule: u64) -> Self {
        World {
            disk,
            bitmaps: Bitmaps::new(granule),
            machine: Machine::default(),
            rejected: HashSet::new(),
            clock: 0,
            last_time: None,
        }
    }

    pub fn write(&mut self, op: &WriteOp) {
        let size = self.disk.len() as u64;
        let offset = op.offset % size;
        let len = op.len.min(size - offset);
        let target = &mut self.disk[offset as usize..(offset + len) as usize];
        match op.fill {
            Some(seed) => fill_bytes(target, seed),
            None => target.fill(0),
        }
        self.bitmaps.mark(offset, len);
    }

    pub fn resize(&mut self, new_size: u64) {
        self.disk.resize(new_size as usize, 0);
        self.bitmaps.reset_after_resize();
    }

    pub fn stale_state(&mut self, back: usize) -> bool {
        let len = self.machine.history.len();
        if back >= len {
            return false;
        }
        self.machine.state = Some(self.machine.history[len - 1 - back].clone());
        true
    }

    fn time(&mut self, back: bool) -> pb::Timestamp {
        let seconds = match self.last_time.filter(|_| back) {
            Some(t) => t - 1,
            None => {
                self.clock += 100;
                BASE_TIME + self.clock
            }
        };
        self.last_time = Some(seconds);
        pb::Timestamp { seconds, nanos: 0 }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ZeroHint {
    Always,
    Never,
    Alternate,
}

#[derive(Debug, Clone, Copy)]
pub enum Transport {
    Direct(ZeroHint),
    Nbd { block_max: u32 },
}

fn inside(ranges: &[Range<u64>], start: u64, end: u64) -> bool {
    let at = ranges.partition_point(|r| r.end <= start);
    ranges
        .get(at)
        .is_some_and(|r| r.start <= start && end <= r.end)
}

/// An `ImageSource` over frozen data and the exported granules of the bitmap.
pub struct ModelSource {
    data: Vec<u8>,
    dirty: Vec<Range<u64>>,
    has_dirty: bool,
    has_next: bool,
    hint: ZeroHint,
    refuse: Arc<AtomicBool>,
    outside: u64,
}

impl ModelSource {
    fn plain(data: Vec<u8>) -> Self {
        ModelSource {
            data,
            dirty: Vec::new(),
            has_dirty: false,
            has_next: false,
            hint: ZeroHint::Never,
            refuse: Arc::new(AtomicBool::new(false)),
            outside: 0,
        }
    }
}

impl ImageSource for ModelSource {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }

    fn has_dirty(&self) -> bool {
        self.has_dirty
    }

    async fn extents(&mut self, offset: u64, len: u64) -> FormatResult<Vec<ImageExtent>> {
        let end = offset + len;
        let mut out = Vec::new();
        let mut pos = offset;
        while pos < end {
            let mut next = end.min((pos / ZERO_BLOCK + 1) * ZERO_BLOCK);
            let at = self.dirty.partition_point(|r| r.end <= pos);
            let dirty = match self.dirty.get(at) {
                Some(r) if r.start <= pos => {
                    next = next.min(r.end);
                    true
                }
                Some(r) => {
                    next = next.min(r.start);
                    false
                }
                None => false,
            };
            let all_zero = self.data[pos as usize..next as usize]
                .iter()
                .all(|b| *b == 0);
            let zero = all_zero
                && match self.hint {
                    ZeroHint::Always => true,
                    ZeroHint::Never => false,
                    ZeroHint::Alternate => (pos / ZERO_BLOCK).is_multiple_of(2),
                };
            out.push(ImageExtent {
                len: next - pos,
                zero,
                dirty,
            });
            pos = next;
        }
        Ok(out)
    }

    async fn read(&mut self, offset: u64, len: usize) -> FormatResult<Vec<u8>> {
        let end = offset + len as u64;
        if self.has_dirty && !inside(&self.dirty, offset, end) {
            self.outside += 1;
            if self.refuse.load(Ordering::SeqCst) {
                return Err(injected("a read outside the dirty ranges"));
            }
        }
        self.data
            .get(offset as usize..end as usize)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| Error::InvalidInput("a read beyond the model disk".into()))
    }
}

pub enum AnySource {
    Direct(ModelSource),
    Nbd(NbdSource<DuplexStream>),
}

impl AnySource {
    fn has_next(&self, next_name: &str) -> bool {
        match self {
            AnySource::Direct(s) => s.has_next,
            AnySource::Nbd(s) => s.has_context(next_name),
        }
    }
}

impl ImageSource for AnySource {
    fn size(&self) -> u64 {
        match self {
            AnySource::Direct(s) => s.size(),
            AnySource::Nbd(s) => s.size(),
        }
    }

    fn has_dirty(&self) -> bool {
        match self {
            AnySource::Direct(s) => s.has_dirty(),
            AnySource::Nbd(s) => s.has_dirty(),
        }
    }

    async fn extents(&mut self, offset: u64, len: u64) -> FormatResult<Vec<ImageExtent>> {
        match self {
            AnySource::Direct(s) => s.extents(offset, len).await,
            AnySource::Nbd(s) => s.extents(offset, len).await,
        }
    }

    async fn read(&mut self, offset: u64, len: usize) -> FormatResult<Vec<u8>> {
        match self {
            AnySource::Direct(s) => s.read(offset, len).await,
            AnySource::Nbd(s) => s.read(offset, len).await,
        }
    }
}

/// Counts what is read and makes the source stop or fail after `k` reads.
pub struct Counting<S> {
    pub inner: S,
    pub reads: u64,
    pub bytes: u64,
    fail_after: Option<u64>,
    cancel_after: Option<(u64, Arc<AtomicBool>)>,
}

impl<S> Counting<S> {
    pub fn new(
        inner: S,
        fail_after: Option<u64>,
        cancel_after: Option<(u64, Arc<AtomicBool>)>,
    ) -> Self {
        Counting {
            inner,
            reads: 0,
            bytes: 0,
            fail_after,
            cancel_after,
        }
    }
}

impl<S: ImageSource + Send> ImageSource for Counting<S> {
    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn has_dirty(&self) -> bool {
        self.inner.has_dirty()
    }

    async fn extents(&mut self, offset: u64, len: u64) -> FormatResult<Vec<ImageExtent>> {
        self.inner.extents(offset, len).await
    }

    async fn read(&mut self, offset: u64, len: usize) -> FormatResult<Vec<u8>> {
        self.reads += 1;
        if self.fail_after.is_some_and(|k| self.reads > k) {
            return Err(injected("the source failed"));
        }
        self.bytes += len as u64;
        let data = self.inner.read(offset, len).await?;
        if let Some((k, flag)) = &self.cancel_after
            && self.reads >= *k
        {
            flag.store(true, Ordering::SeqCst);
        }
        Ok(data)
    }

    fn finish(&mut self) -> FormatResult<()> {
        self.inner.finish()
    }
}

struct CountingParent<'a> {
    inner: Reader<&'a LocalFsStore>,
    count: u64,
    fail: bool,
}

impl ParentChunks for CountingParent<'_> {
    fn chunk<'a>(
        &'a mut self,
        id: &'a BlobId,
    ) -> Pin<Box<dyn Future<Output = FormatResult<Vec<u8>>> + Send + 'a>> {
        Box::pin(async move {
            self.count += 1;
            if self.fail {
                return Err(injected("the parent chunk is gone"));
            }
            self.inner.chunk(id).await
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    None,
    CancelAfterReads(u64),
    SourceFailsAfterReads(u64),
    ParentFails,
    Rejected,
}

#[derive(Debug, Clone, Copy)]
pub struct RunSpec {
    pub failure: Failure,
    pub window: Option<WriteOp>,
    pub export_bitmap: bool,
    pub late_next: bool,
    pub skip_rotation: bool,
    pub time_back: bool,
}

impl RunSpec {
    pub fn plain() -> Self {
        RunSpec {
            failure: Failure::None,
            window: None,
            export_bitmap: true,
            late_next: false,
            skip_rotation: false,
            time_back: false,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Cfg {
    pub segment_chunks: u64,
    pub transport: Transport,
}

pub struct RunOutcome {
    pub plan: ReadPlan,
    pub snapshot: Option<BlobId>,
    pub root_tree: Option<BlobId>,
    pub error: Option<String>,
    pub rejected: bool,
    pub clean: bool,
    pub rotated: bool,
    pub record_bound: bool,
    pub frozen: Vec<u8>,
    pub time: pb::Timestamp,
    pub source_bytes: u64,
    pub parent_reads: u64,
    pub dirty_bytes: u64,
    pub partial_chunks: u64,
    pub outside_reads: u64,
    pub window_applied: bool,
}

struct Opened {
    source: AnySource,
    refuse: Arc<AtomicBool>,
    server: Option<JoinHandle<(lodestore_nbd::Result<()>, MemExport)>>,
}

async fn open_source(
    cfg: &Cfg,
    frozen: &[u8],
    exports: Vec<(String, Vec<Range<u64>>)>,
    requested: [&str; 2],
) -> Opened {
    match cfg.transport {
        Transport::Direct(hint) => {
            let refuse = Arc::new(AtomicBool::new(false));
            let source = ModelSource {
                data: frozen.to_vec(),
                dirty: exports.first().map(|e| e.1.clone()).unwrap_or_default(),
                has_dirty: !exports.is_empty(),
                has_next: exports.len() > 1,
                hint,
                refuse: Arc::clone(&refuse),
                outside: 0,
            };
            Opened {
                source: AnySource::Direct(source),
                refuse,
                server: None,
            }
        }
        Transport::Nbd { block_max } => {
            let mut export = MemExport::new(frozen.to_vec()).min_block(512);
            for (name, ranges) in exports {
                export = export.with_bitmap(&name, ranges);
            }
            let refuse = export.dirty_only_switch();
            let (client_side, server_side) = tokio::io::duplex(1 << 20);
            let limits = Limits {
                max_read: block_max,
                ..Limits::default()
            };
            let server = tokio::spawn(async move {
                let mut export = export;
                let served = serve(server_side, &mut export, &limits).await;
                (served, export)
            });
            let client = Client::handshake(client_side, "", &requested, limits)
                .await
                .unwrap();
            Opened {
                source: AnySource::Nbd(NbdSource::new(client)),
                refuse,
                server: Some(server),
            }
        }
    }
}

/// Closes the source and returns how many reads fell outside the dirty ranges.
async fn close(
    source: AnySource,
    server: Option<JoinHandle<(lodestore_nbd::Result<()>, MemExport)>>,
) -> u64 {
    match source {
        AnySource::Direct(s) => s.outside,
        AnySource::Nbd(s) => {
            let _ = s.into_client().disconnect().await;
            match server {
                Some(task) => task.await.unwrap().1.reads_outside_dirty(),
                None => 0,
            }
        }
    }
}

fn dirty_stats(ranges: &[Range<u64>], size: u64) -> (u64, u64) {
    let dirty = ranges.iter().map(|r| r.end - r.start).sum();
    let mut partial = 0;
    for index in 0..size.div_ceil(C) {
        let (from, to) = (index * C, ((index + 1) * C).min(size));
        let hit: u64 = ranges
            .iter()
            .map(|r| r.end.min(to).saturating_sub(r.start.max(from)))
            .sum();
        if hit > 0 && hit < to - from {
            partial += 1;
        }
    }
    (dirty, partial)
}

/// One run as `lode` makes it: the decision comes from `decide` over a fresh
/// listing of the bucket and the state cache, never from the model.
pub async fn run_backup(env: &Env, cfg: &Cfg, w: &mut World, spec: &RunSpec) -> RunOutcome {
    let key = key();
    let names = BitmapNames::new(&env.repo_id, HOST, &key);
    let identity = source_identity("model");

    let state = match w.machine.state.take() {
        None => StateRead::Missing,
        Some(bytes) => match ImageState::parse(&bytes, &env.repo_id, HOST, &key) {
            Ok(state) => StateRead::Found(state),
            Err(_) => StateRead::Unreadable,
        },
    };

    let n = w.bitmaps.n;
    w.bitmaps.ensure(n);
    if !spec.late_next {
        w.bitmaps.ensure(n + 1);
    }
    let frozen = w.disk.clone();
    let size = frozen.len() as u64;
    let (name_now, name_next) = (names.name(n), names.name(n + 1));
    let first = w.bitmaps.ranges(n, size);
    let mut exports = Vec::new();
    if spec.export_bitmap {
        exports.push((name_now.clone(), first.clone()));
        if w.bitmaps.exists(n + 1) {
            exports.push((name_next.clone(), w.bitmaps.ranges(n + 1, size)));
        }
    }
    let Opened {
        source,
        refuse,
        server,
    } = open_source(cfg, &frozen, exports, [&name_now, &name_next]).await;

    let listing = env.reader().await.list_snapshots().await.unwrap();
    let entries: Vec<SnapshotEntry> = listing
        .entries
        .into_iter()
        .filter(|e| !w.rejected.contains(&e.id))
        .collect();
    let instance = instance_snapshots(&entries, &key, HOST);
    let decision = decide(&DecideInput {
        names: &names,
        bitmap: Some(n),
        export_has_bitmap: source.has_dirty(),
        export_has_next: source.has_next(&name_next),
        export_size: size,
        source: identity,
        state: &state,
        instance: &instance,
    });

    let mut plan = decision.plan;
    let mut parent_reader = None;
    let mut parent_table = None;
    if let ReadPlan::Incremental { parent } = plan {
        let mut reader = env.reader().await;
        match load_image(&mut reader, &parent, "disk").await {
            Ok(image) if image.layout.size == size => {
                parent_table = Some((
                    parent,
                    image
                        .layout
                        .chunk_table()
                        .expect("a loaded layout has a chunk table"),
                ));
                parent_reader = Some(CountingParent {
                    inner: reader,
                    count: 0,
                    fail: spec.failure == Failure::ParentFails,
                });
                refuse.store(true, Ordering::SeqCst);
            }
            _ => plan = ReadPlan::Full(FullReason::ParentUnreadable),
        }
    }
    let full_reason = match plan {
        ReadPlan::Full(reason) => Some(reason),
        ReadPlan::Incremental { .. } => None,
    };

    let cancel = Arc::new(AtomicBool::new(false));
    let (fail_after, cancel_after) = match spec.failure {
        Failure::CancelAfterReads(k) => (None, Some((k, Arc::clone(&cancel)))),
        Failure::SourceFailsAfterReads(k) => (Some(k), None),
        _ => (None, None),
    };
    let mut counting = Counting::new(source, fail_after, cancel_after);
    let time = w.time(spec.time_back);
    let options = BackupOptions {
        time: Some(time),
        hostname: Some(HOST.to_vec()),
        cancel: Arc::clone(&cancel),
        ..BackupOptions::default()
    };
    let image = ImageOptions {
        segment_chunks: cfg.segment_chunks,
        bitmap: decision.record.clone(),
        full_reason,
    };
    let parent = match (parent_reader.as_mut(), parent_table) {
        (Some(chunks), Some((snapshot_id, table))) => Some(ImageParent {
            snapshot_id,
            disk_size: size,
            table,
            chunks,
        }),
        _ => None,
    };
    let result = env
        .writer()
        .await
        .backup_image("disk", &mut counting, parent, options, image, &|_| {})
        .await;

    let source_bytes = counting.bytes;
    let outside_reads = close(counting.inner, server).await;
    let parent_reads = parent_reader.as_ref().map_or(0, |p| p.count);
    let (dirty_bytes, partial_chunks) = dirty_stats(&first, size);

    let mut out = RunOutcome {
        plan,
        snapshot: None,
        root_tree: None,
        error: None,
        rejected: false,
        clean: false,
        rotated: false,
        record_bound: decision.record.is_some(),
        frozen,
        time,
        source_bytes,
        parent_reads,
        dirty_bytes,
        partial_chunks,
        outside_reads,
        window_applied: false,
    };
    match result {
        Ok(done) => {
            let id = done.outcome.snapshot_id;
            out.snapshot = Some(id);
            out.root_tree = Some(done.outcome.root_tree);
            out.rejected = spec.failure == Failure::Rejected;
            if out.rejected {
                w.rejected.insert(id);
            } else if let Some(binding) = &decision.record {
                let state = ImageState {
                    parent: id,
                    bitmap_gen: binding.generation,
                    disk_size: size,
                    source: identity,
                };
                let bytes = state.encode(&env.repo_id, HOST, &key);
                w.machine.history.push(bytes.clone());
                w.machine.state = Some(bytes);
            }
        }
        Err(e) => out.error = Some(e.to_string()),
    }
    if let Some(window) = &spec.window {
        w.write(window);
        out.window_applied = true;
    }
    if out.snapshot.is_some() && !out.rejected {
        if spec.late_next {
            w.bitmaps.ensure(n + 1);
        }
        if !spec.skip_rotation {
            w.bitmaps.rotate();
            out.rotated = true;
        }
    }
    out.clean = out.rotated && out.record_bound && !spec.time_back;
    out
}

async fn full_read(
    env: &Env,
    data: &[u8],
    hostname: &[u8],
    time: pb::Timestamp,
    segment_chunks: u64,
) -> (BlobId, BlobId) {
    let mut source = ModelSource::plain(data.to_vec());
    let options = BackupOptions {
        time: Some(time),
        hostname: Some(hostname.to_vec()),
        ..BackupOptions::default()
    };
    let image = ImageOptions {
        segment_chunks,
        bitmap: None,
        full_reason: None,
    };
    let done = env
        .writer()
        .await
        .backup_image("disk", &mut source, None, options, image, &|_| {})
        .await
        .unwrap();
    (done.outcome.snapshot_id, done.outcome.root_tree)
}

/// What a full read of the same state of the disk gives: same time, same
/// segments, another host so that no instance sees it.
pub async fn reference_full_read(
    env: &Env,
    data: &[u8],
    time: pb::Timestamp,
    segment_chunks: u64,
) -> BlobId {
    reference_full_snapshot(env, data, time, segment_chunks)
        .await
        .1
}

/// The snapshot and the root tree of [`reference_full_read`].
pub async fn reference_full_snapshot(
    env: &Env,
    data: &[u8],
    time: pb::Timestamp,
    segment_chunks: u64,
) -> (BlobId, BlobId) {
    full_read(env, data, REFERENCE_HOST, time, segment_chunks).await
}

/// A second machine without a bitmap writes a full read of the same instance.
pub async fn other_machine(env: &Env, cfg: &Cfg, w: &mut World) {
    let time = w.time(false);
    full_read(env, &w.disk, HOST, time, cfg.segment_chunks).await;
}

/// The restored image, or why the restore failed: the error or the failures
/// the report lists.
pub async fn restore_outcome(env: &Env, id: &BlobId) -> Result<Vec<u8>, String> {
    let tmp = tempfile::tempdir().unwrap();
    let out: &Path = &tmp.path().join("out");
    let report = env
        .reader()
        .await
        .restore_image(id, "disk", out, &RestoreOptions::default(), &|_| {})
        .await
        .map_err(|e| e.to_string())?;
    if !report.errors.is_empty() {
        let all: Vec<&str> = report.errors.iter().map(|f| f.error.as_str()).collect();
        return Err(all.join("; "));
    }
    Ok(fs::read(out.join("disk.img")).unwrap())
}

pub async fn restore_bytes(env: &Env, id: &BlobId) -> Vec<u8> {
    restore_outcome(env, id).await.unwrap()
}

/// The first run of 64 hex digits in a message: the id of the blob it names.
fn blob_id_in(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        let run = bytes[start..]
            .iter()
            .take_while(|b| b.is_ascii_hexdigit())
            .count();
        if run >= 64 {
            return Some(&text[start..start + 64]);
        }
        start += run.max(1);
    }
    None
}

/// Two failed restores of the same tree fail for the same reason: the same
/// kind of error and the same blob.
pub fn same_restore_failure(a: &str, b: &str) -> bool {
    let (id_a, id_b) = (blob_id_in(a), blob_id_in(b));
    let mask = |text: &str, id: Option<&str>| match id {
        Some(id) => text.replace(id, "<blob>"),
        None => text.to_owned(),
    };
    id_a == id_b && mask(a, id_a) == mask(b, id_b)
}

fn pack_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            pack_files(&path, out);
        } else if !path.file_name().unwrap().to_string_lossy().starts_with('.') {
            out.push(path);
        }
    }
}

fn sorted_packs(env: &Env) -> Vec<PathBuf> {
    let mut packs = Vec::new();
    pack_files(&env.root().join("packs"), &mut packs);
    packs.sort();
    packs
}

/// Flips one byte of one pack file; false when the repository has no pack.
pub fn corrupt_pack(env: &Env, pick: usize, offset: u64) -> bool {
    let packs = sorted_packs(env);
    if packs.is_empty() {
        return false;
    }
    let path = &packs[pick % packs.len()];
    let mut bytes = fs::read(path).unwrap();
    let at = (offset % bytes.len() as u64) as usize;
    bytes[at] ^= 0xff;
    fs::write(path, bytes).unwrap();
    true
}

/// Swaps the contents of two pack files; false when they are one file.
pub fn swap_packs(env: &Env, a: usize, b: usize) -> bool {
    let packs = sorted_packs(env);
    if packs.is_empty() || a % packs.len() == b % packs.len() {
        return false;
    }
    let (first, second) = (&packs[a % packs.len()], &packs[b % packs.len()]);
    let (bytes_first, bytes_second) = (fs::read(first).unwrap(), fs::read(second).unwrap());
    fs::write(first, bytes_second).unwrap();
    fs::write(second, bytes_first).unwrap();
    true
}

pub fn initial_disk(fills: &[Option<u8>], tail: u64) -> Vec<u8> {
    let mut disk = vec![0u8; fills.len() * C as usize + tail as usize];
    for (index, fill) in fills.iter().enumerate() {
        if let Some(seed) = fill {
            let from = index * C as usize;
            fill_bytes(&mut disk[from..from + C as usize], *seed);
        }
    }
    if tail > 0 && fills.iter().any(Option::is_some) {
        let from = fills.len() * C as usize;
        fill_bytes(&mut disk[from..], 7);
    }
    disk
}

#[derive(Default)]
pub struct Tally {
    pub incrementals: Cell<u64>,
    pub merged_chunks: Cell<u64>,
    pub below_chunk: Cell<u64>,
    pub equal_chunk: Cell<u64>,
    pub above_chunk: Cell<u64>,
    pub multi_segment: Cell<u64>,
    pub nbd_cases: Cell<u64>,
    pub full_after_failure: Cell<u64>,
    pub resizes: Cell<u64>,
    pub same_size_after_resize: Cell<u64>,
    pub window_in_increment: Cell<u64>,
    pub late_next_unbound: Cell<u64>,
    pub parent_not_newest: Cell<u64>,
    pub gen_mismatch: Cell<u64>,
    pub bitmap_missing: Cell<u64>,
    pub backups: Cell<u64>,
}

impl Tally {
    pub fn add(cell: &Cell<u64>, n: u64) {
        cell.set(cell.get() + n);
    }
}

pub fn same_bytes(a: &[u8], b: &[u8]) -> Result<(), String> {
    if a.len() != b.len() {
        return Err(format!("sizes differ: {} and {}", a.len(), b.len()));
    }
    match a.iter().zip(b).position(|(x, y)| x != y) {
        Some(at) => Err(format!("first difference at byte {at}")),
        None => Ok(()),
    }
}
