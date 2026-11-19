//! Backs up a disk image as fixed chunks, holes and segments; see
//! [`crate::image`] for the layout.

use super::{
    BackupOptions, BackupOutcome, BackupProgress, Deadline, Session, Writer, hostname, meta, now,
};
use crate::chunker::blob_id;
use crate::crypto::{BLOB_TYPE_DATA, BlobId};
use crate::error::{Error, Result};
use crate::image::{
    ChunkRef, DISKS_DIR, IMAGE_CHUNK_SIZE, IMAGE_NAME_RULE, ImageExtent, ImageMeta, ImageSource,
    ReadMode, SEGMENT_CHUNKS, check_shape, segment_nodes, valid_image_name,
};
use crate::pb;
use crate::source::{self, SourceKey};
use crate::store::BlobStore;

/// Writer settings of an image run; none of them is part of the format.
#[derive(Debug, Clone, Copy)]
pub struct ImageOptions {
    /// Chunks per segment. The reader needs only that the segments agree.
    pub segment_chunks: u64,
}

impl Default for ImageOptions {
    fn default() -> Self {
        ImageOptions {
            segment_chunks: SEGMENT_CHUNKS,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ImageOutcome {
    pub outcome: BackupOutcome,
    /// The source recorded in the snapshot.
    pub source: pb::Source,
}

enum ChunkPlan {
    Zero,
    Read,
}

/// The one place that decides what a chunk costs.
fn plan_chunk(extents: &[ImageExtent]) -> ChunkPlan {
    if extents.iter().all(|e| e.zero) {
        ChunkPlan::Zero
    } else {
        ChunkPlan::Read
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
        let read = read_chunks(&mut run, source, size, chunk_count, &path).await;
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
        run.stats.files_new = 1;
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
        let image_meta = ImageMeta {
            read_mode: ReadMode::Full,
            disk_size: size,
        };
        let recorded = source::encode(&key, &image_meta.to_meta(name))?
            .ok_or_else(|| Error::InvalidInput("an image has a source key".into()))?;
        let snapshot = pb::Snapshot {
            time: Some(started),
            hostname: options.hostname.unwrap_or_else(hostname),
            paths: vec![path],
            tags: options.tags,
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

/// Cuts the image into chunks: holes cost nothing, a chunk of zero bytes
/// the source claims as data becomes a hole, the rest is stored.
async fn read_chunks<S: BlobStore, I: ImageSource>(
    run: &mut Session<'_, '_, S>,
    source: &mut I,
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
        let len = IMAGE_CHUNK_SIZE.min(size - offset);
        let extents = source.extents(offset, len).await?;
        check_cover(&extents, len)?;
        let chunk = match plan_chunk(&extents) {
            ChunkPlan::Zero => ChunkRef::Zero,
            ChunkPlan::Read => {
                let want = usize::try_from(len).map_err(|_| lying("chunk is out of range"))?;
                let data = source.read(offset, want).await?;
                if data.len() != want {
                    return Err(lying("returned fewer bytes than asked for"));
                }
                run.stats.bytes_read += len;
                if data.iter().all(|&b| b == 0) {
                    ChunkRef::Zero
                } else {
                    let id: BlobId = blob_id(&run.w.keys.k_id, &data);
                    run.add_blob(BLOB_TYPE_DATA, id, &data).await?;
                    ChunkRef::Data(id)
                }
            }
        };
        chunks.push(chunk);
        run.cur_read = offset + len;
        run.report(false, path);
    }
    Ok(chunks)
}
