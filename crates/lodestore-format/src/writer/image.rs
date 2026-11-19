//! Backs up a disk image as fixed chunks, holes and segments; see
//! [`crate::image`] for the layout.

use super::{
    BackupOptions, BackupOutcome, BackupProgress, Deadline, Session, Writer, hostname, meta, now,
};
use crate::chunker::blob_id;
use crate::crypto::{BLOB_TYPE_DATA, BlobId};
use crate::error::{Error, Result};
use crate::image::{
    BitmapBinding, ChunkRef, DISKS_DIR, FullReason, IMAGE_CHUNK_SIZE, IMAGE_NAME_RULE, ImageExtent,
    ImageMeta, ImageParent, ImageSource, ReadMode, SEGMENT_CHUNKS, check_shape, segment_nodes,
    valid_image_name,
};
use crate::pb;
use crate::source::{self, SourceKey};
use crate::store::BlobStore;

/// Writer settings of an image run; none of them is part of the format.
#[derive(Debug, Clone)]
pub struct ImageOptions {
    /// Chunks per segment. The reader needs only that the segments agree.
    pub segment_chunks: u64,
    /// The bitmap that collects the changes made after this snapshot.
    pub bitmap: Option<BitmapBinding>,
    /// Why a full read was made although a bitmap was named; ignored by an
    /// incremental run.
    pub full_reason: Option<FullReason>,
}

impl Default for ImageOptions {
    fn default() -> Self {
        ImageOptions {
            segment_chunks: SEGMENT_CHUNKS,
            bitmap: None,
            full_reason: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ImageOutcome {
    pub outcome: BackupOutcome,
    /// The source recorded in the snapshot.
    pub source: pb::Source,
}

#[derive(Debug, PartialEq, Eq)]
struct DirtyRange {
    start: u64,
    len: u64,
    zero: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum ChunkPlan {
    Zero,
    Read,
    Reuse(BlobId),
    Merge {
        base: ChunkRef,
        ranges: Vec<DirtyRange>,
    },
}

/// The one place that decides what a chunk costs; a full read is the same
/// decision without a parent chunk.
fn plan_chunk(extents: &[ImageExtent], parent: Option<ChunkRef>) -> ChunkPlan {
    let Some(base) = parent else {
        return if extents.iter().all(|e| e.zero) {
            ChunkPlan::Zero
        } else {
            ChunkPlan::Read
        };
    };
    let mut ranges: Vec<DirtyRange> = Vec::new();
    let mut at = 0u64;
    for e in extents {
        if e.dirty {
            match ranges.last_mut() {
                Some(last) if last.start.saturating_add(last.len) == at => {
                    last.len = last.len.saturating_add(e.len);
                    last.zero &= e.zero;
                }
                _ => ranges.push(DirtyRange {
                    start: at,
                    len: e.len,
                    zero: e.zero,
                }),
            }
        }
        at = at.saturating_add(e.len);
    }
    match ranges.as_slice() {
        [] => match base {
            ChunkRef::Zero => ChunkPlan::Zero,
            ChunkRef::Data(id) => ChunkPlan::Reuse(id),
        },
        [all] if all.start == 0 && all.len == at => {
            if all.zero {
                ChunkPlan::Zero
            } else {
                ChunkPlan::Read
            }
        }
        _ => ChunkPlan::Merge { base, ranges },
    }
}

fn dir_node(name: &[u8]) -> pb::Node {
    pb::Node {
        name: name.to_vec(),
        r#type: pb::NodeType::Dir as i32,
        ..Default::default()
    }
}

fn lying(what: &str) -> Error {
    Error::InvalidInput(format!("the image source {what}"))
}

/// Checks that `extents` cover `len` bytes exactly, each run non-empty.
fn check_cover(extents: &[ImageExtent], len: u64) -> Result<()> {
    let mut covered = 0u64;
    for e in extents {
        if e.len == 0 {
            return Err(lying("reported an empty extent"));
        }
        covered = covered
            .checked_add(e.len)
            .ok_or_else(|| lying("reported extents that overflow"))?;
    }
    if covered != len {
        return Err(lying("reported extents that do not cover the chunk"));
    }
    Ok(())
}

impl<S: BlobStore> Writer<S> {
    /// Backs up `source` as the image `name`: a snapshot of `/disks/<name>`
    /// tagged with the source `image/<name>`. A failing source (or
    /// `options.cancel`) uploads what was read, writes no snapshot and
    /// returns the error.
    pub async fn backup_image<I: ImageSource>(
        &mut self,
        name: &str,
        source: &mut I,
        mut parent: Option<ImageParent<'_>>,
        mut options: BackupOptions<'_>,
        image: ImageOptions,
        progress: &dyn Fn(&BackupProgress),
    ) -> Result<ImageOutcome> {
        if !valid_image_name(name) {
            return Err(Error::InvalidInput(format!(
                "{name:?} is not an image name: {IMAGE_NAME_RULE}"
            )));
        }
        if options.parent.is_some() || options.source.is_some() {
            return Err(Error::InvalidInput(
                "an image backup takes no parent snapshot and no source of its own".into(),
            ));
        }
        let size = source.size();
        let chunk_count = check_shape(size, image.segment_chunks)?;
        if let Some(p) = &parent {
            if p.disk_size != size || u64::try_from(p.table.len()).ok() != Some(chunk_count) {
                return Err(Error::InvalidInput(
                    "the parent chunk table does not match the size of the image".into(),
                ));
            }
            if !source.has_dirty() {
                return Err(Error::InvalidInput(
                    "an incremental image backup needs a source with dirty ranges".into(),
                ));
            }
        }
        let key = SourceKey::image(name)?;
        let started = options.time.unwrap_or_else(now);
        let path = format!("/disks/{name}").into_bytes();
        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();
        let (user, group) = meta::owner_names(uid, gid);
        let stamp = |node: &mut pb::Node, mode: u32| {
            node.mode = mode;
            node.uid = uid;
            node.gid = gid;
            node.user.clone_from(&user);
            node.group.clone_from(&group);
            node.mtime = Some(started);
        };

        let mut run = Session::new(self, progress);
        run.apply(&mut options);
        let read = read_chunks(&mut run, source, parent.as_mut(), size, chunk_count, &path).await;
        let chunks = match read.and_then(|chunks| source.finish().map(|()| chunks)) {
            Ok(chunks) => chunks,
            Err(e) => {
                run.wind_down().await?;
                return Err(e);
            }
        };

        let mut segments = segment_nodes(size, &chunks, image.segment_chunks)?;
        for segment in &mut segments {
            stamp(segment, 0o600);
        }
        run.cur_read = 0;
        if parent.is_some() {
            run.stats.files_changed = 1;
        } else {
            run.stats.files_new = 1;
        }
        run.stats.bytes_total = size;
        run.files_done = 1;
        run.report(false, &path);
        let mut disk = dir_node(name.as_bytes());
        stamp(&mut disk, 0o700);
        disk.subtrees = run.write_tree(segments).await?;
        let mut disks = dir_node(DISKS_DIR);
        stamp(&mut disks, 0o700);
        disks.subtrees = run.write_tree(vec![disk]).await?;
        let root_tree = run.write_root(vec![disks]).await?;
        run.wind_down().await?;

        let stats = run.stats;
        let (read_mode, full_reason) = if parent.is_some() {
            (ReadMode::Incremental, None)
        } else {
            (ReadMode::Full, image.full_reason)
        };
        let image_meta = ImageMeta {
            read_mode,
            disk_size: size,
        };
        let bound = image_meta.to_meta_bound(name, image.bitmap.as_ref(), full_reason);
        let recorded = source::encode(&key, &bound)?
            .ok_or_else(|| Error::InvalidInput("an image has a source key".into()))?;
        let snapshot = pb::Snapshot {
            time: Some(started),
            hostname: options.hostname.unwrap_or_else(hostname),
            paths: vec![path],
            tags: options.tags,
            parent: parent
                .as_ref()
                .map(|p| p.snapshot_id.to_vec())
                .unwrap_or_default(),
            root_tree: root_tree.to_vec(),
            stats: Some(stats),
            index_refs: run.refs.iter().map(|r| r.to_vec()).collect(),
            agent_id: options.agent_id.unwrap_or_default(),
            source: Some(recorded.clone()),
            program_version: format!("lodestore-format {}", env!("CARGO_PKG_VERSION")),
            ..Default::default()
        };
        Deadline::check(options.deadline)?;
        let snapshot_id = self.put_snapshot(&snapshot).await?;
        Ok(ImageOutcome {
            outcome: BackupOutcome {
                snapshot_id,
                root_tree,
                time: started,
                stats,
                incomplete: false,
                errors_total: 0,
                warnings_total: 0,
            },
            source: recorded,
        })
    }
}

/// Stores a chunk read or assembled in full; zero bytes become a hole.
async fn store_chunk<S: BlobStore>(run: &mut Session<'_, '_, S>, data: &[u8]) -> Result<ChunkRef> {
    if data.iter().all(|&b| b == 0) {
        return Ok(ChunkRef::Zero);
    }
    let id: BlobId = blob_id(&run.w.keys.k_id, data);
    run.add_blob(BLOB_TYPE_DATA, id, data).await?;
    Ok(ChunkRef::Data(id))
}

async fn read_exact<I: ImageSource>(source: &mut I, offset: u64, len: usize) -> Result<Vec<u8>> {
    let data = source.read(offset, len).await?;
    if data.len() != len {
        return Err(lying("returned fewer bytes than asked for"));
    }
    Ok(data)
}

/// Cuts the image into chunks: holes cost nothing, a chunk of zero bytes
/// the source claims as data becomes a hole, a chunk clean since the parent
/// is taken from the parent, the rest is stored.
async fn read_chunks<S: BlobStore, I: ImageSource>(
    run: &mut Session<'_, '_, S>,
    source: &mut I,
    mut parent: Option<&mut ImageParent<'_>>,
    size: u64,
    chunk_count: u64,
    path: &[u8],
) -> Result<Vec<ChunkRef>> {
    let mut chunks = Vec::with_capacity(usize::try_from(chunk_count).unwrap_or(0));
    for index in 0..chunk_count {
        if let Some(e) = run.stopped() {
            return Err(e);
        }
        let offset = index * IMAGE_CHUNK_SIZE;
        let len = size
            .checked_sub(offset)
            .map_or(0, |rest| rest.min(IMAGE_CHUNK_SIZE));
        let want = usize::try_from(len).map_err(|_| lying("chunk is out of range"))?;
        let extents = source.extents(offset, len).await?;
        check_cover(&extents, len)?;
        let base = match &parent {
            Some(p) => Some(
                usize::try_from(index)
                    .ok()
                    .and_then(|at| p.table.get(at).copied())
                    .ok_or_else(|| Error::InvalidInput("the parent chunk table is short".into()))?,
            ),
            None => None,
        };
        let chunk = match plan_chunk(&extents, base) {
            ChunkPlan::Zero => ChunkRef::Zero,
            ChunkPlan::Read => {
                let data = read_exact(source, offset, want).await?;
                run.stats.bytes_read += len;
                store_chunk(run, &data).await?
            }
            ChunkPlan::Reuse(id) => {
                let loc = run.w.index.get(&id, BLOB_TYPE_DATA).ok_or_else(|| {
                    Error::InvalidInput(format!(
                        "chunk {index} of the parent snapshot is not in the index; \
                         the next backup reads the image in full"
                    ))
                })?;
                if usize::try_from(loc.ulen).ok() != Some(want) {
                    return Err(Error::corrupt(
                        "image",
                        format!(
                            "chunk {index} of the parent snapshot has {} bytes in the index, \
                             expected {want}",
                            loc.ulen
                        ),
                    ));
                }
                run.refs.insert(loc.shard);
                ChunkRef::Data(id)
            }
            ChunkPlan::Merge { base, ranges } => {
                let mut buf = match base {
                    ChunkRef::Zero => vec![0u8; want],
                    ChunkRef::Data(id) => {
                        let Some(p) = parent.as_mut() else {
                            return Err(Error::InvalidInput("no parent chunk source".into()));
                        };
                        p.chunks.chunk(&id).await?
                    }
                };
                if buf.len() != want {
                    return Err(Error::corrupt(
                        "image",
                        format!(
                            "chunk {index} of the parent snapshot has {} bytes, expected {want}",
                            buf.len()
                        ),
                    ));
                }
                for r in ranges {
                    let end = r
                        .start
                        .checked_add(r.len)
                        .ok_or_else(|| lying("reported extents that overflow"))?;
                    let (from, to) = (
                        usize::try_from(r.start).map_err(|_| lying("chunk is out of range"))?,
                        usize::try_from(end).map_err(|_| lying("chunk is out of range"))?,
                    );
                    let target = buf
                        .get_mut(from..to)
                        .ok_or_else(|| lying("reported extents that do not cover the chunk"))?;
                    if r.zero {
                        target.fill(0);
                    } else {
                        let data = read_exact(source, offset + r.start, to - from).await?;
                        run.stats.bytes_read += r.len;
                        target.copy_from_slice(&data);
                    }
                }
                store_chunk(run, &buf).await?
            }
        };
        chunks.push(chunk);
        run.cur_read = offset + len;
        run.report(false, path);
    }
    Ok(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIB: u64 = 1024;

    fn ext(len: u64, zero: bool, dirty: bool) -> ImageExtent {
        ImageExtent { len, zero, dirty }
    }

    fn id() -> BlobId {
        [7u8; 32]
    }

    fn range(start: u64, len: u64, zero: bool) -> DirtyRange {
        DirtyRange { start, len, zero }
    }

    #[test]
    fn plan_chunk_without_parent_ignores_dirty() {
        assert_eq!(plan_chunk(&[ext(8, true, true)], None), ChunkPlan::Zero);
        assert_eq!(
            plan_chunk(&[ext(4, true, false), ext(4, false, false)], None),
            ChunkPlan::Read
        );
    }

    #[test]
    fn plan_chunk_clean_chunk_follows_the_parent() {
        let clean = [ext(8, false, false)];
        assert_eq!(
            plan_chunk(&clean, Some(ChunkRef::Data(id()))),
            ChunkPlan::Reuse(id())
        );
        assert_eq!(plan_chunk(&clean, Some(ChunkRef::Zero)), ChunkPlan::Zero);
    }

    #[test]
    fn plan_chunk_fully_dirty_chunk_is_read_whole() {
        let base = Some(ChunkRef::Data(id()));
        assert_eq!(
            plan_chunk(&[ext(4, false, true), ext(4, true, true)], base),
            ChunkPlan::Read
        );
        assert_eq!(
            plan_chunk(&[ext(4, true, true), ext(4, true, true)], base),
            ChunkPlan::Zero
        );
    }

    #[test]
    fn plan_chunk_merges_the_dirty_middle() {
        let base = ChunkRef::Data(id());
        let extents = [
            ext(KIB, false, false),
            ext(KIB, false, true),
            ext(KIB, false, false),
        ];
        assert_eq!(
            plan_chunk(&extents, Some(base)),
            ChunkPlan::Merge {
                base,
                ranges: vec![range(KIB, KIB, false)]
            }
        );
    }

    #[test]
    fn plan_chunk_keeps_separate_ranges_in_order() {
        let extents = [
            ext(1, false, true),
            ext(2, false, false),
            ext(3, true, true),
            ext(4, false, false),
        ];
        assert_eq!(
            plan_chunk(&extents, Some(ChunkRef::Zero)),
            ChunkPlan::Merge {
                base: ChunkRef::Zero,
                ranges: vec![range(0, 1, false), range(3, 3, true)]
            }
        );
    }

    #[test]
    fn plan_chunk_joins_adjacent_dirty_extents() {
        let both_zero = [
            ext(2, false, false),
            ext(2, true, true),
            ext(2, true, true),
            ext(2, false, false),
        ];
        assert_eq!(
            plan_chunk(&both_zero, Some(ChunkRef::Zero)),
            ChunkPlan::Merge {
                base: ChunkRef::Zero,
                ranges: vec![range(2, 4, true)]
            }
        );
        let mixed = [
            ext(2, false, false),
            ext(2, true, true),
            ext(2, false, true),
            ext(2, false, false),
        ];
        assert_eq!(
            plan_chunk(&mixed, Some(ChunkRef::Zero)),
            ChunkPlan::Merge {
                base: ChunkRef::Zero,
                ranges: vec![range(2, 4, false)]
            }
        );
    }

    #[test]
    fn plan_chunk_dirty_zero_piece_is_a_zero_range() {
        let extents = [
            ext(2, false, false),
            ext(2, true, true),
            ext(2, false, false),
        ];
        assert_eq!(
            plan_chunk(&extents, Some(ChunkRef::Data(id()))),
            ChunkPlan::Merge {
                base: ChunkRef::Data(id()),
                ranges: vec![range(2, 2, true)]
            }
        );
    }
}
