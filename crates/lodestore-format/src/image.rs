//! Disk images: an image is cut into fixed 4 MiB chunks aligned at `i * 4 MiB`
//! and stored as ordinary `File` nodes below `/disks/NAME`, named `000000`,
//! `000001`, ... in the order of the image. A segment holds up to
//! [`SEGMENT_CHUNKS`] chunks. Its `content` lists only the non-zero chunks;
//! a zero chunk is a hole that no blob backs, recorded by `sparse_extents`.
//! Runs of data chunks are merged into one extent, so a segment has one
//! spelling. A segment without data carries the one empty extent, as a file
//! that is all hole does.
//!
//! The reader joins the segments in order into one sparse file `NAME.img`;
//! it learns nothing from the segment size the writer chose beyond that the
//! segments agree. Everything below is read from a bucket and is untrusted.

use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;

use crate::crypto::RepoId;
use crate::crypto::{BLOB_TYPE_DATA, BlobId};
use crate::error::{Error, Result};
use crate::fs::restore::check_extents;
use crate::pb;
use crate::reader::Reader;
use crate::snapshot::SnapshotEntry;
use crate::source::{Meta, SourceKey};
use crate::store::BlobStore;
use crate::tree::validate_name;

pub const IMAGE_CHUNK_SIZE: u64 = 4 << 20;
const _: () = assert!(IMAGE_CHUNK_SIZE as usize == crate::pack::MAX_DATA_ULEN);

pub const SEGMENT_CHUNKS: u64 = 65_536;
pub const MAX_IMAGE_SIZE: u64 = 16 << 40;
pub const MAX_SEGMENTS: usize = 64;
const _: () = assert!(MAX_IMAGE_SIZE == MAX_SEGMENTS as u64 * SEGMENT_CHUNKS * IMAGE_CHUNK_SIZE);
pub const MAX_IMAGE_NAME_LEN: usize = 200;
pub const DISKS_DIR: &[u8] = b"disks";

pub const LAYOUT_VERSION: &str = "1";
pub const META_LAYOUT: &str = "image_layout";
pub const META_CHUNK_SIZE: &str = "chunk_size";
pub const META_READ_MODE: &str = "read_mode";
pub const DISK_SIZE: &str = "size";
pub const DISK_BITMAP_NAME: &str = "bitmap_name";
pub const DISK_BITMAP_GEN: &str = "bitmap_gen";
pub const DISK_FULL_REASON: &str = "full_reason";

fn bad(reason: impl Into<String>) -> Error {
    Error::corrupt("image", reason)
}

/// The rule for a user-chosen image (and disk) name.
pub fn valid_image_name(name: &str) -> bool {
    (1..=MAX_IMAGE_NAME_LEN).contains(&name.len())
        && name != "."
        && name != ".."
        && !name.bytes().any(|b| b == b'/' || b < 0x20 || b == 0x7f)
        && crate::source::encoded_segment_len(name) <= MAX_ENCODED_NAME_LEN
}

/// Room for the name in a source key: `image/` and the percent-encoded name.
const MAX_ENCODED_NAME_LEN: usize = crate::source::MAX_KEY_LEN - "image/".len();

pub const IMAGE_NAME_RULE: &str = "1 to 200 bytes, at most 506 once each byte outside \
    A-Z a-z 0-9 - . _ ~ counts as three, without '/' or control characters, and not . or ..";

/// The meta key of one disk's field. The name hashes into the key because a
/// meta key is `[a-z0-9_]{1,64}` and a disk name is any UTF-8 up to 200 bytes.
pub fn disk_meta_key(disk_name: &str, field: &str) -> String {
    let mut key = String::from("disk_");
    for byte in blake3::hash(disk_name.as_bytes()).as_bytes().iter().take(8) {
        let _ = write!(key, "{byte:02x}");
    }
    key.push('_');
    key.push_str(field);
    key
}

pub fn segment_name(index: usize) -> Vec<u8> {
    format!("{index:06}").into_bytes()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkRef {
    Zero,
    Data(BlobId),
}

/// A run of an image the source reports on, in image order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageExtent {
    pub len: u64,
    /// Reading the run yields zeros.
    pub zero: bool,
    /// The dirty bitmap marks these bytes as changed since the parent. Without
    /// a bitmap the source agreed on with the writer, this is always `false`.
    pub dirty: bool,
}

/// Where the bytes of an image come from. `extents(offset, len)` covers
/// exactly `[offset, offset + len)` in order, every `len` above zero;
/// `read(offset, len)` returns exactly `len` bytes.
pub trait ImageSource {
    fn size(&self) -> u64;

    fn extents(
        &mut self,
        offset: u64,
        len: u64,
    ) -> impl Future<Output = Result<Vec<ImageExtent>>> + Send;

    fn read(&mut self, offset: u64, len: usize) -> impl Future<Output = Result<Vec<u8>>> + Send;

    /// Whether `dirty` on the extents comes from a dirty bitmap.
    fn has_dirty(&self) -> bool {
        false
    }

    /// Called after the last read; an error means the source changed while it
    /// was read, and the backup writes no snapshot.
    fn finish(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Where the bytes of a parent chunk come from. In `lode` this is a
/// [`Reader`], whose `read_blob` checks the keyed id of what it returns.
pub trait ParentChunks: Send {
    fn chunk<'a>(
        &'a mut self,
        id: &'a BlobId,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'a>>;
}

impl<S: BlobStore> ParentChunks for Reader<S> {
    fn chunk<'a>(
        &'a mut self,
        id: &'a BlobId,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'a>> {
        Box::pin(self.read_blob(id, BLOB_TYPE_DATA))
    }
}

/// The snapshot an incremental image run builds on. `disk_size` and `table`
/// come from the parent's own tree, never from a cache.
pub struct ImageParent<'a> {
    pub snapshot_id: BlobId,
    pub disk_size: u64,
    pub table: Vec<ChunkRef>,
    pub chunks: &'a mut dyn ParentChunks,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadMode {
    Full,
    Incremental,
}

impl ReadMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ReadMode::Full => "full",
            ReadMode::Incremental => "incremental",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        [ReadMode::Full, ReadMode::Incremental]
            .into_iter()
            .find(|m| m.as_str() == text)
    }
}

/// What a reader needs from the source meta of an image snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageMeta {
    pub read_mode: ReadMode,
    pub disk_size: u64,
}

fn decimal(value: &str) -> Option<u64> {
    let canonical = !value.is_empty()
        && value.bytes().all(|b| b.is_ascii_digit())
        && (value == "0" || !value.starts_with('0'));
    if canonical { value.parse().ok() } else { None }
}

impl ImageMeta {
    pub fn to_meta(&self, disk_name: &str) -> Meta {
        Meta::from([
            (META_LAYOUT.to_owned(), LAYOUT_VERSION.to_owned()),
            (META_CHUNK_SIZE.to_owned(), IMAGE_CHUNK_SIZE.to_string()),
            (
                META_READ_MODE.to_owned(),
                self.read_mode.as_str().to_owned(),
            ),
            (
                disk_meta_key(disk_name, DISK_SIZE),
                self.disk_size.to_string(),
            ),
        ])
    }

    /// `to_meta` plus the bitmap that collects changes after this snapshot and
    /// the reason for a full read.
    pub fn to_meta_bound(
        &self,
        disk_name: &str,
        binding: Option<&BitmapBinding>,
        full_reason: Option<FullReason>,
    ) -> Meta {
        let mut meta = self.to_meta(disk_name);
        if let Some(b) = binding {
            meta.insert(disk_meta_key(disk_name, DISK_BITMAP_NAME), b.name.clone());
            meta.insert(
                disk_meta_key(disk_name, DISK_BITMAP_GEN),
                b.generation.to_string(),
            );
        }
        if let Some(reason) = full_reason {
            meta.insert(
                disk_meta_key(disk_name, DISK_FULL_REASON),
                reason.as_str().to_owned(),
            );
        }
        meta
    }

    /// Fails closed on a layout this build does not read; keys it does not
    /// know are skipped.
    pub fn from_meta(meta: &Meta, disk_name: &str) -> Result<Self> {
        let get = |key: &str| meta.get(key).map(String::as_str);
        match get(META_LAYOUT) {
            Some(LAYOUT_VERSION) => {}
            Some(v) if matches!(decimal(v), Some(2..)) => {
                return Err(bad(format!(
                    "disk image layout {v} is newer than this build reads"
                )));
            }
            _ => return Err(bad("the disk image layout version is missing or malformed")),
        }
        if get(META_CHUNK_SIZE) != Some(IMAGE_CHUNK_SIZE.to_string().as_str()) {
            return Err(bad(
                "the disk image chunk size is not the one this build reads",
            ));
        }
        let read_mode = get(META_READ_MODE)
            .and_then(ReadMode::parse)
            .ok_or_else(|| bad("the disk image read mode is missing or unknown"))?;
        let disk_size = get(&disk_meta_key(disk_name, DISK_SIZE))
            .and_then(decimal)
            .filter(|&n| n <= MAX_IMAGE_SIZE)
            .ok_or_else(|| bad("the disk size is missing, malformed or too large"))?;
        Ok(ImageMeta {
            read_mode,
            disk_size,
        })
    }
}

/// Why a run read the whole image although a bitmap was named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullReason {
    NoBitmap,
    BitmapMissing,
    NoState,
    StateUnreadable,
    NoParent,
    ParentNotNewest,
    ParentUnreadable,
    NameChanged,
    GenMismatch,
    SizeChanged,
    SourceChanged,
}

impl FullReason {
    pub const ALL: [FullReason; 11] = [
        FullReason::NoBitmap,
        FullReason::BitmapMissing,
        FullReason::NoState,
        FullReason::StateUnreadable,
        FullReason::NoParent,
        FullReason::ParentNotNewest,
        FullReason::ParentUnreadable,
        FullReason::NameChanged,
        FullReason::GenMismatch,
        FullReason::SizeChanged,
        FullReason::SourceChanged,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            FullReason::NoBitmap => "no_bitmap",
            FullReason::BitmapMissing => "bitmap_missing",
            FullReason::NoState => "no_state",
            FullReason::StateUnreadable => "state_unreadable",
            FullReason::NoParent => "no_parent",
            FullReason::ParentNotNewest => "parent_not_newest",
            FullReason::ParentUnreadable => "parent_unreadable",
            FullReason::NameChanged => "name_changed",
            FullReason::GenMismatch => "gen_mismatch",
            FullReason::SizeChanged => "size_changed",
            FullReason::SourceChanged => "source_changed",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == text)
    }

    /// `None` when the snapshot records no reason; an unknown code is an error.
    pub fn from_meta(meta: &Meta, disk_name: &str) -> Result<Option<Self>> {
        let Some(code) = meta.get(&disk_meta_key(disk_name, DISK_FULL_REASON)) else {
            return Ok(None);
        };
        Self::parse(code)
            .map(Some)
            .ok_or_else(|| bad("the full read reason is not one this build knows"))
    }
}

/// The bitmap that collects the changes made after a snapshot, and its generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitmapBinding {
    pub name: String,
    pub generation: u64,
}

impl BitmapBinding {
    /// `None` when the snapshot records no binding; one key without the other
    /// or a malformed value is an error.
    pub fn from_meta(meta: &Meta, disk_name: &str) -> Result<Option<Self>> {
        let name = meta.get(&disk_meta_key(disk_name, DISK_BITMAP_NAME));
        let generation = meta.get(&disk_meta_key(disk_name, DISK_BITMAP_GEN));
        let (name, generation) = match (name, generation) {
            (None, None) => return Ok(None),
            (Some(name), Some(generation)) => (name, generation),
            _ => return Err(bad("the bitmap name and generation are not both recorded")),
        };
        if name.is_empty() || name.bytes().any(|b| b < 0x20 || b == 0x7f) {
            return Err(bad("the bitmap name is empty or has control characters"));
        }
        let generation = decimal(generation)
            .filter(|&n| n >= 1)
            .ok_or_else(|| bad("the bitmap generation is not a positive decimal"))?;
        Ok(Some(BitmapBinding {
            name: name.clone(),
            generation,
        }))
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Exactly 64 lower-case hex digits.
fn hex32_lower(text: &str) -> Option<[u8; 32]> {
    let canonical =
        text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if canonical {
        blake3::Hash::from_hex(text).ok().map(|h| *h.as_bytes())
    } else {
        None
    }
}

/// Names one instance in the repository: BLAKE3 of the host name, a zero
/// byte and the source key.
pub fn instance_id(hostname: &[u8], key: &SourceKey) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(hostname);
    hasher.update(&[0]);
    hasher.update(key.as_str().as_bytes());
    *hasher.finalize().as_bytes()
}

/// Builds and reads the bitmap names `lode-{repo16}-{inst16}-g{N}` of one
/// instance in one repository.
#[derive(Debug, Clone)]
pub struct BitmapNames {
    prefix: String,
}

impl BitmapNames {
    pub fn new(repo_id: &RepoId, hostname: &[u8], key: &SourceKey) -> Self {
        let instance = instance_id(hostname, key);
        BitmapNames {
            prefix: format!(
                "lode-{}-{}-g",
                hex_lower(repo_id.get(..8).unwrap_or_default()),
                hex_lower(instance.get(..8).unwrap_or_default())
            ),
        }
    }

    pub fn name(&self, generation: u64) -> String {
        format!("{}{generation}", self.prefix)
    }

    pub fn first(&self) -> String {
        self.name(1)
    }

    /// The generation of a name of this instance; `None` for any other name.
    /// The top value is refused so that `generation + 1` never overflows.
    pub fn parse(&self, name: &str) -> Option<u64> {
        decimal(name.strip_prefix(self.prefix.as_str())?).filter(|n| (1..u64::MAX).contains(n))
    }

    pub fn binding(&self, generation: u64) -> BitmapBinding {
        BitmapBinding {
            name: self.name(generation),
            generation,
        }
    }
}

pub const MAX_STATE_BYTES: usize = 4096;
const STATE_VERSION: u64 = 1;
const STATE_KEYS: [&str; 9] = [
    "version",
    "repo_id",
    "host",
    "key",
    "source",
    "parent",
    "bitmap_gen",
    "disk_size",
    "chunk_size",
];

/// The identity of the local source, from its canonical URI.
pub fn source_identity(canonical: &str) -> [u8; 32] {
    *blake3::hash(canonical.as_bytes()).as_bytes()
}

/// A cache of the binding the last run recorded. The bucket is the truth: the
/// chunk ids come from the parent's tree, not from here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageState {
    pub parent: BlobId,
    pub bitmap_gen: u64,
    pub disk_size: u64,
    pub source: [u8; 32],
}

impl ImageState {
    pub fn encode(&self, repo_id: &RepoId, hostname: &[u8], key: &SourceKey) -> Vec<u8> {
        let value = serde_json::json!({
            "version": STATE_VERSION,
            "repo_id": hex_lower(repo_id),
            "host": hex_lower(hostname),
            "key": key.as_str(),
            "source": hex_lower(&self.source),
            "parent": hex_lower(&self.parent),
            "bitmap_gen": self.bitmap_gen,
            "disk_size": self.disk_size,
            "chunk_size": IMAGE_CHUNK_SIZE,
        });
        serde_json::to_vec(&value).unwrap_or_default()
    }

    /// Accepts only what `encode` writes for this repository, host and instance.
    pub fn parse(bytes: &[u8], repo_id: &RepoId, hostname: &[u8], key: &SourceKey) -> Result<Self> {
        if bytes.len() > MAX_STATE_BYTES {
            return Err(bad("the image state file is too large"));
        }
        let value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|_| bad("the image state is not JSON"))?;
        let object = value
            .as_object()
            .filter(|o| {
                o.len() == STATE_KEYS.len() && STATE_KEYS.iter().all(|k| o.contains_key(*k))
            })
            .ok_or_else(|| bad("the image state does not have exactly the known fields"))?;
        let text = |name: &str| object.get(name).and_then(serde_json::Value::as_str);
        let number = |name: &str| object.get(name).and_then(serde_json::Value::as_u64);
        if number("version") != Some(STATE_VERSION) {
            return Err(bad("the image state has another version"));
        }
        if text("repo_id") != Some(hex_lower(repo_id).as_str())
            || text("host") != Some(hex_lower(hostname).as_str())
            || text("key") != Some(key.as_str())
        {
            return Err(bad(
                "the image state belongs to another repository or instance",
            ));
        }
        let source = text("source")
            .and_then(hex32_lower)
            .ok_or_else(|| bad("the image state source is not 64 lower-case hex digits"))?;
        let parent = text("parent")
            .and_then(hex32_lower)
            .ok_or_else(|| bad("the image state parent is not 64 lower-case hex digits"))?;
        let bitmap_gen = number("bitmap_gen")
            .filter(|&n| n >= 1)
            .ok_or_else(|| bad("the image state generation is not a positive integer"))?;
        let disk_size = number("disk_size")
            .filter(|&n| n <= MAX_IMAGE_SIZE)
            .ok_or_else(|| bad("the image state disk size is missing or too large"))?;
        if number("chunk_size") != Some(IMAGE_CHUNK_SIZE) {
            return Err(bad("the image state has another chunk size"));
        }
        let state = ImageState {
            parent,
            bitmap_gen,
            disk_size,
            source,
        };
        // A repeated key or other spelling would parse to the same fields.
        if state.encode(repo_id, hostname, key) != bytes {
            return Err(bad("the image state is not in the canonical spelling"));
        }
        Ok(state)
    }
}

/// What reading the state file gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateRead {
    Missing,
    Unreadable,
    Found(ImageState),
}

/// One snapshot of an instance as the bucket records it; what could not be
/// read is `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceSnapshot {
    pub id: BlobId,
    pub parent: Option<BlobId>,
    pub disk_size: Option<u64>,
    pub binding: Option<BitmapBinding>,
    /// The snapshot records a bitmap binding that does not parse; decide counts it as a fork.
    pub binding_unreadable: bool,
}

/// The snapshots whose claim is `key` and whose manifest host is `hostname`,
/// in the order given.
pub fn instance_snapshots(
    entries: &[SnapshotEntry],
    key: &SourceKey,
    hostname: &[u8],
) -> Vec<InstanceSnapshot> {
    let disk = key.segment(1);
    entries
        .iter()
        .filter_map(|entry| {
            let snapshot = &entry.snapshot;
            let (claimed, meta) = crate::source::decode(snapshot.source.as_ref()).ok()?;
            if &claimed != key || snapshot.hostname != hostname {
                return None;
            }
            Some(InstanceSnapshot {
                id: entry.id,
                parent: BlobId::try_from(snapshot.parent.as_slice()).ok(),
                disk_size: disk
                    .as_deref()
                    .and_then(|d| ImageMeta::from_meta(&meta, d).ok())
                    .map(|m| m.disk_size),
                binding: disk
                    .as_deref()
                    .and_then(|d| BitmapBinding::from_meta(&meta, d).ok().flatten()),
                binding_unreadable: disk
                    .as_deref()
                    .is_some_and(|d| BitmapBinding::from_meta(&meta, d).is_err()),
            })
        })
        .collect()
}

pub struct DecideInput<'a> {
    pub names: &'a BitmapNames,
    pub bitmap: Option<u64>,
    pub export_has_bitmap: bool,
    pub export_has_next: bool,
    pub export_size: u64,
    pub source: [u8; 32],
    pub state: &'a StateRead,
    /// Oldest first; the last one is the newest.
    pub instance: &'a [InstanceSnapshot],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadPlan {
    Full(FullReason),
    Incremental { parent: BlobId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub plan: ReadPlan,
    /// The binding of the next bitmap, if the export confirmed it before the first read.
    pub record: Option<BitmapBinding>,
    /// The generation to propose when no bitmap is bound.
    pub restart: u64,
}

/// Reads the whole image whenever any field of the binding is missing or differs.
pub fn decide(input: &DecideInput<'_>) -> Decision {
    let record = input
        .bitmap
        .and_then(|n| n.checked_add(1))
        .filter(|_| input.export_has_next)
        .map(|next| input.names.binding(next));
    let highest = input
        .instance
        .iter()
        .filter_map(|s| s.binding.as_ref().map(|b| b.generation))
        .chain(input.bitmap)
        .max()
        .unwrap_or(0);
    Decision {
        plan: plan(input),
        record,
        restart: highest.saturating_add(1),
    }
}

fn plan(input: &DecideInput<'_>) -> ReadPlan {
    use FullReason::*;
    let Some(n) = input.bitmap else {
        return ReadPlan::Full(NoBitmap);
    };
    if !input.export_has_bitmap {
        return ReadPlan::Full(BitmapMissing);
    }
    let state = match input.state {
        StateRead::Missing => return ReadPlan::Full(NoState),
        StateRead::Unreadable => return ReadPlan::Full(StateUnreadable),
        StateRead::Found(state) => state,
    };
    if state.source != input.source {
        return ReadPlan::Full(SourceChanged);
    }
    let Some((newest, older)) = input.instance.split_last() else {
        return ReadPlan::Full(NoParent);
    };
    if newest.id != state.parent {
        return ReadPlan::Full(ParentNotNewest);
    }
    let (Some(disk_size), Some(binding)) = (newest.disk_size, &newest.binding) else {
        return ReadPlan::Full(ParentUnreadable);
    };
    if binding.name != input.names.name(binding.generation) {
        return ReadPlan::Full(NameChanged);
    }
    if binding.generation != n || state.bitmap_gen != n {
        return ReadPlan::Full(GenMismatch);
    }
    let forked = older.iter().any(|s| {
        s.binding.as_ref().is_some_and(|b| b.generation >= n)
            || s.binding_unreadable
            || (newest.parent.is_some() && s.parent == newest.parent)
    });
    if forked {
        return ReadPlan::Full(GenMismatch);
    }
    if state.disk_size != disk_size || disk_size != input.export_size {
        return ReadPlan::Full(SizeChanged);
    }
    ReadPlan::Incremental {
        parent: state.parent,
    }
}

/// Checks what a writer asks for and returns the number of chunks.
pub(crate) fn check_shape(size: u64, segment_chunks: u64) -> Result<u64> {
    if size > MAX_IMAGE_SIZE {
        return Err(Error::InvalidInput(format!(
            "the image is {size} bytes, more than the {MAX_IMAGE_SIZE} an image may have"
        )));
    }
    if !(1..=SEGMENT_CHUNKS).contains(&segment_chunks) {
        return Err(Error::InvalidInput(format!(
            "a segment holds 1 to {SEGMENT_CHUNKS} chunks, not {segment_chunks}"
        )));
    }
    let chunks = size.div_ceil(IMAGE_CHUNK_SIZE);
    if chunks.div_ceil(segment_chunks) > MAX_SEGMENTS as u64 {
        return Err(Error::InvalidInput(format!(
            "the image needs more than {MAX_SEGMENTS} segments of {segment_chunks} chunks"
        )));
    }
    Ok(chunks)
}

/// The canonical segment nodes of an image; the caller sets owner and times.
pub fn segment_nodes(size: u64, chunks: &[ChunkRef], segment_chunks: u64) -> Result<Vec<pb::Node>> {
    let total = check_shape(size, segment_chunks)?;
    if chunks.len() as u64 != total {
        return Err(Error::InvalidInput(format!(
            "an image of {size} bytes has {total} chunks, not {}",
            chunks.len()
        )));
    }
    let per_segment = usize::try_from(segment_chunks)
        .map_err(|_| Error::InvalidInput("segment size is out of range".into()))?;
    let mut nodes = Vec::new();
    for (index, piece) in chunks.chunks(per_segment).enumerate() {
        let first = (index as u64) * segment_chunks;
        let end = first + piece.len() as u64;
        let bytes = (end * IMAGE_CHUNK_SIZE).min(size) - first * IMAGE_CHUNK_SIZE;
        nodes.push(segment_node(index, bytes, piece));
    }
    if nodes.is_empty() {
        nodes.push(segment_node(0, 0, &[]));
    }
    Ok(nodes)
}

fn segment_node(index: usize, bytes: u64, piece: &[ChunkRef]) -> pb::Node {
    let mut content = Vec::new();
    let mut extents: Vec<pb::Extent> = Vec::new();
    let mut run_start: Option<u64> = None;
    for (at, chunk) in piece.iter().enumerate() {
        let at = at as u64;
        match (chunk, run_start) {
            (ChunkRef::Data(id), start) => {
                content.push(id.to_vec());
                run_start = start.or(Some(at));
            }
            (ChunkRef::Zero, Some(start)) => {
                extents.push(extent(start, at, bytes));
                run_start = None;
            }
            (ChunkRef::Zero, None) => {}
        }
    }
    if let Some(start) = run_start {
        extents.push(extent(start, piece.len() as u64, bytes));
    }
    if extents.is_empty() {
        extents.push(pb::Extent {
            offset: 0,
            length: 0,
        });
    }
    pb::Node {
        name: segment_name(index),
        r#type: pb::NodeType::File as i32,
        size: bytes,
        content,
        sparse_extents: extents,
        ..Default::default()
    }
}

/// The extent of chunks `[from, to)` of a segment of `bytes` bytes.
fn extent(from: u64, to: u64, bytes: u64) -> pb::Extent {
    let offset = from * IMAGE_CHUNK_SIZE;
    pb::Extent {
        offset,
        length: (to * IMAGE_CHUNK_SIZE).min(bytes) - offset,
    }
}

/// The segments of one image as read from a tree, checked.
#[derive(Debug, Clone)]
pub struct ImageLayout {
    pub size: u64,
    pub segment_size: u64,
    pub segments: Vec<pb::Node>,
}

/// Checks the nodes below `/disks/NAME` as a layout this build reads.
pub fn parse_layout(nodes: &[pb::Node]) -> Result<ImageLayout> {
    let Some(first) = nodes.first() else {
        return Err(bad("the image has no segments"));
    };
    if nodes.len() > MAX_SEGMENTS {
        return Err(bad(format!(
            "the image has more than {MAX_SEGMENTS} segments"
        )));
    }
    let segment_size = first.size;
    let last = nodes.len() - 1;
    let mut size = 0u64;
    for (index, node) in nodes.iter().enumerate() {
        if node.name != segment_name(index) {
            return Err(bad("segments are not named 000000, 000001, ... in order"));
        }
        if node.r#type != pb::NodeType::File as i32 || node.hardlink_group != 0 {
            return Err(bad("a segment is not a plain file"));
        }
        if node.size > SEGMENT_CHUNKS * IMAGE_CHUNK_SIZE {
            return Err(bad("a segment is larger than a segment may be"));
        }
        if index < last {
            if node.size != segment_size || node.size == 0 || node.size % IMAGE_CHUNK_SIZE != 0 {
                return Err(bad(
                    "segments before the last are not equal whole numbers of chunks",
                ));
            }
        } else if index > 0 && (node.size == 0 || node.size > segment_size) {
            return Err(bad("the last segment is empty or larger than the others"));
        }
        check_segment(node)?;
        size = size
            .checked_add(node.size)
            .filter(|&n| n <= MAX_IMAGE_SIZE)
            .ok_or_else(|| bad("the image is larger than an image may be"))?;
    }
    Ok(ImageLayout {
        size,
        segment_size,
        segments: nodes.to_vec(),
    })
}

fn check_segment(node: &pb::Node) -> Result<()> {
    let extents = check_extents(node)?;
    if extents.is_empty() {
        return Err(bad("a segment has no extents"));
    }
    if node.content.iter().any(|id| id.len() != 32) {
        return Err(bad("a chunk id is not 32 bytes"));
    }
    if matches!(extents.as_slice(), [(0, 0)]) {
        if !node.content.is_empty() {
            return Err(bad("a segment without data lists chunks"));
        }
        return Ok(());
    }
    let mut chunks = 0u64;
    let mut previous_end: Option<u64> = None;
    for &(offset, length) in &extents {
        let end = offset
            .checked_add(length)
            .ok_or_else(|| bad("an extent overflows"))?;
        if length == 0 {
            return Err(bad("an empty extent next to data"));
        }
        if offset % IMAGE_CHUNK_SIZE != 0 || (end % IMAGE_CHUNK_SIZE != 0 && end != node.size) {
            return Err(bad("an extent does not lie on chunk boundaries"));
        }
        if previous_end == Some(offset) {
            return Err(bad("adjacent extents are not merged"));
        }
        previous_end = Some(end);
        chunks = chunks
            .checked_add(length.div_ceil(IMAGE_CHUNK_SIZE))
            .ok_or_else(|| bad("the chunk count overflows"))?;
    }
    if chunks != node.content.len() as u64 {
        return Err(bad("the chunk ids do not match the data extents"));
    }
    Ok(())
}

impl ImageLayout {
    /// One entry per chunk of the image.
    pub fn chunk_table(&self) -> Vec<ChunkRef> {
        let total = usize::try_from(self.size.div_ceil(IMAGE_CHUNK_SIZE)).unwrap_or(0);
        let mut table = vec![ChunkRef::Zero; total];
        let per_segment = self.segment_size / IMAGE_CHUNK_SIZE;
        for (index, segment) in self.segments.iter().enumerate() {
            let base = (index as u64).saturating_mul(per_segment);
            let mut ids = segment.content.iter();
            for e in &segment.sparse_extents {
                let first = base.saturating_add(e.offset / IMAGE_CHUNK_SIZE);
                for k in 0..e.length.div_ceil(IMAGE_CHUNK_SIZE) {
                    let Some(id) = ids
                        .next()
                        .and_then(|raw| BlobId::try_from(raw.as_slice()).ok())
                    else {
                        return table;
                    };
                    let slot = usize::try_from(first.saturating_add(k))
                        .ok()
                        .and_then(|at| table.get_mut(at));
                    if let Some(slot) = slot {
                        *slot = ChunkRef::Data(id);
                    }
                }
            }
        }
        table
    }
}

fn merged_extents(layout: &ImageLayout) -> Vec<pb::Extent> {
    let mut out: Vec<pb::Extent> = Vec::new();
    for (index, segment) in layout.segments.iter().enumerate() {
        let shift = (index as u64).saturating_mul(layout.segment_size);
        for e in segment.sparse_extents.iter().filter(|e| e.length > 0) {
            let offset = shift.saturating_add(e.offset);
            match out.last_mut() {
                Some(prev) if prev.offset.saturating_add(prev.length) == offset => {
                    prev.length = prev.length.saturating_add(e.length);
                }
                _ => out.push(pb::Extent {
                    offset,
                    length: e.length,
                }),
            }
        }
    }
    if out.is_empty() {
        out.push(pb::Extent {
            offset: 0,
            length: 0,
        });
    }
    out
}

/// The file node of the whole image without its chunk ids, so that a plan can
/// be measured without copying them.
pub fn merge_head(layout: &ImageLayout, file_name: &[u8]) -> pb::Node {
    let mut node = layout.segments.first().cloned().unwrap_or_default();
    node.name = file_name.to_vec();
    node.size = layout.size;
    node.content = Vec::new();
    node.sparse_extents = merged_extents(layout);
    node.subtrees = Vec::new();
    node.link_target = Vec::new();
    node
}

/// The file node of the whole image, as one sparse file.
pub fn merge_segments(layout: &ImageLayout, file_name: &[u8]) -> pb::Node {
    let mut node = merge_head(layout, file_name);
    node.content = layout
        .segments
        .iter()
        .flat_map(|s| s.content.iter().cloned())
        .collect();
    node
}

/// One disk of a snapshot, ready to be restored.
#[derive(Debug, Clone)]
pub struct LoadedImage {
    pub name: Vec<u8>,
    pub layout: ImageLayout,
}

impl LoadedImage {
    /// `NAME.img`, which a restore writes.
    pub fn file_name(&self) -> Vec<u8> {
        let mut name = self.name.clone();
        name.extend_from_slice(b".img");
        name
    }

    pub fn head(&self) -> Result<pb::Node> {
        let name = self.file_name();
        validate_name(&name)?;
        Ok(merge_head(&self.layout, &name))
    }

    pub fn file_node(&self) -> Result<pb::Node> {
        let name = self.file_name();
        validate_name(&name)?;
        Ok(merge_segments(&self.layout, &name))
    }
}

/// Finds `/disks/NAME` in a snapshot by exact name, whatever else the
/// snapshot holds, and checks its segments against the size the source meta records.
pub async fn load_image<S: BlobStore>(
    reader: &mut Reader<S>,
    id: &BlobId,
    disk_name: &str,
) -> Result<LoadedImage> {
    if !valid_image_name(disk_name) {
        return Err(Error::InvalidInput(format!(
            "{disk_name:?} is not a disk image name: 1 to {MAX_IMAGE_NAME_LEN} bytes, \
             without '/' or control characters, and not . or .."
        )));
    }
    let is_dir = |node: &&pb::Node| node.r#type == pb::NodeType::Dir as i32;
    let top = reader.list_dir(id, b"/").await?;
    if !top.iter().filter(is_dir).any(|n| n.name == DISKS_DIR) {
        return Err(bad("the snapshot has no /disks"));
    }
    let disks = reader.list_dir(id, b"/disks").await?;
    if !disks
        .iter()
        .filter(is_dir)
        .any(|n| n.name == disk_name.as_bytes())
    {
        return Err(bad(format!("the snapshot has no disk {disk_name}")));
    }
    let segments = reader
        .list_dir(id, format!("/disks/{disk_name}").as_bytes())
        .await?;
    let layout = parse_layout(&segments)?;
    let manifest = reader.snapshot(id).await?;
    let (_, meta) = crate::source::decode(manifest.source.as_ref())?;
    let meta = ImageMeta::from_meta(&meta, disk_name)?;
    if layout.size != meta.disk_size {
        return Err(bad(format!(
            "the segments hold {} bytes; the snapshot says the disk has {}",
            layout.size, meta.disk_size
        )));
    }
    Ok(LoadedImage {
        name: disk_name.as_bytes().to_vec(),
        layout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_shape_accepts_the_bounds_and_refuses_past_them() {
        assert_eq!(
            check_shape(MAX_IMAGE_SIZE, SEGMENT_CHUNKS).ok(),
            Some(MAX_IMAGE_SIZE / IMAGE_CHUNK_SIZE)
        );
        assert!(matches!(
            check_shape(MAX_IMAGE_SIZE + 1, SEGMENT_CHUNKS),
            Err(Error::InvalidInput(_))
        ));
        assert_eq!(
            check_shape(MAX_SEGMENTS as u64 * IMAGE_CHUNK_SIZE, 1).ok(),
            Some(MAX_SEGMENTS as u64)
        );
        assert!(matches!(
            check_shape((MAX_SEGMENTS as u64 + 1) * IMAGE_CHUNK_SIZE, 1),
            Err(Error::InvalidInput(_))
        ));
    }
}
