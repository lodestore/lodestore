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

use crate::crypto::BlobId;
use crate::error::{Error, Result};
use crate::fs::restore::check_extents;
use crate::pb;
use crate::reader::Reader;
use crate::source::Meta;
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

fn bad(reason: impl Into<String>) -> Error {
    Error::corrupt("image", reason)
}

/// The rule for a user-chosen image (and disk) name.
pub fn valid_image_name(name: &str) -> bool {
    (1..=MAX_IMAGE_NAME_LEN).contains(&name.len())
        && name != "."
        && name != ".."
        && !name.bytes().any(|b| b == b'/' || b < 0x20 || b == 0x7f)
}

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

    /// Fails closed on a layout this build does not read; keys it does not
    /// know are skipped.
    pub fn from_meta(meta: &Meta, disk_name: &str) -> Result<Self> {
        let get = |key: &str| meta.get(key).map(String::as_str);
        match get(META_LAYOUT) {
            Some(LAYOUT_VERSION) => {}
            Some(v) if decimal(v).is_some_and(|n| n > 1) => {
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
            for e in segment.sparse_extents.iter().filter(|e| e.length > 0) {
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
/// snapshot holds, and checks its segments.
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
    Ok(LoadedImage {
        name: disk_name.as_bytes().to_vec(),
        layout: parse_layout(&segments)?,
    })
}
