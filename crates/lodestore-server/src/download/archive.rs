//! Snapshot selections as tar.gz, tar.zst or zip, written straight to any
//! `Write`. Nothing touches the disk: chunks are read, decrypted and handed to
//! the encoder one at a time.

use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::rc::Rc;

use anyhow::{Context, ensure};
use lodestore_format::crypto::{BLOB_TYPE_DATA, BlobId};
use lodestore_format::fs::restore::check_extents;
use lodestore_format::pb;
use lodestore_format::reader::Reader;
use lodestore_format::selection::Selection;
use lodestore_format::store::BlobStore;
use lodestore_format::tree::{PlanStep, SelectionWalk};
use serde::{Deserialize, Serialize};
use tar::{Builder, EntryType, Header};
use tokio::runtime::Handle;
use zip::write::{FullFileOptions, SimpleFileOptions, StreamWriter};
use zip::{CompressionMethod, DateTime, ZipWriter};

const ZSTD_LEVEL: i32 = 3;
const ZIP_LARGE_FILE: u64 = 3 << 30;
const ZIP_EXTENDED_TIMESTAMP: u16 = 0x5455;
const ZIP_FIRST_SECOND: i64 = 315_532_800;
const ZIP_LAST_SECOND: i64 = 4_354_819_199;
const TAR_LINK_NAME_LEN: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveFormat {
    TarGz,
    TarZst,
    Zip,
}

impl ArchiveFormat {
    pub fn extension(self) -> &'static str {
        match self {
            ArchiveFormat::TarGz => "tar.gz",
            ArchiveFormat::TarZst => "tar.zst",
            ArchiveFormat::Zip => "zip",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            ArchiveFormat::TarGz => "application/gzip",
            ArchiveFormat::TarZst => "application/zstd",
            ArchiveFormat::Zip => "application/zip",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArchiveReport {
    pub files: u64,
    pub dirs: u64,
    /// Entries the format cannot carry: links and special files in a zip,
    /// extended attributes with a name that is not text.
    pub skipped: u64,
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn seconds(node: &pb::Node) -> i64 {
    node.mtime.as_ref().map_or(0, |t| t.seconds)
}

trait Sink {
    fn hardlinks(&self) -> bool;
    fn dir(&mut self, node: &pb::Node, rel: &[u8]) -> io::Result<()>;
    fn file(&mut self, node: &pb::Node, rel: &[u8], content: &mut dyn Read) -> io::Result<()>;
    fn hardlink(&mut self, node: &pb::Node, rel: &[u8], first: &[u8]) -> io::Result<()>;
    /// Writes a symlink or a special file; `false` when the format has no
    /// place for it.
    fn other(&mut self, node: &pb::Node, rel: &[u8]) -> io::Result<bool>;
    fn skipped(&self) -> u64;
}

struct TarSink<E: Write> {
    builder: Builder<E>,
    skipped: u64,
}

fn tar_path(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}

fn tar_header(node: &pb::Node, kind: EntryType) -> Header {
    let mut header = Header::new_gnu();
    header.set_entry_type(kind);
    header.set_mode(node.mode & 0o7777);
    header.set_uid(node.uid.into());
    header.set_gid(node.gid.into());
    header.set_mtime(u64::try_from(seconds(node)).unwrap_or(0));
    if let Ok(user) = std::str::from_utf8(&node.user) {
        let _ = header.set_username(user);
    }
    if let Ok(group) = std::str::from_utf8(&node.group) {
        let _ = header.set_groupname(group);
    }
    header.set_size(0);
    header
}

fn device_numbers(rdev: u64) -> (u32, u32) {
    let major = ((rdev >> 32) & 0xffff_f000) | ((rdev >> 8) & 0x0000_0fff);
    let minor = ((rdev >> 12) & 0xffff_ff00) | (rdev & 0x0000_00ff);
    (major as u32, minor as u32)
}

impl<E: Write> TarSink<E> {
    fn new(out: E) -> Self {
        TarSink {
            builder: Builder::new(out),
            skipped: 0,
        }
    }

    fn xattrs(&mut self, node: &pb::Node) -> io::Result<()> {
        let mut records = Vec::new();
        for attr in &node.xattrs {
            match std::str::from_utf8(&attr.name) {
                Ok(name) if !name.is_empty() && !name.contains(['=', '\n']) => {
                    records.push((format!("SCHILY.xattr.{name}"), attr.value.as_slice()));
                }
                _ => self.skipped += 1,
            }
        }
        self.builder
            .append_pax_extensions(records.iter().map(|(key, value)| (key.as_str(), *value)))
    }
}

impl<E: Write> Sink for TarSink<E> {
    fn hardlinks(&self) -> bool {
        true
    }

    fn dir(&mut self, node: &pb::Node, rel: &[u8]) -> io::Result<()> {
        self.xattrs(node)?;
        let mut header = tar_header(node, EntryType::Directory);
        self.builder
            .append_data(&mut header, tar_path(rel), io::empty())
    }

    fn file(&mut self, node: &pb::Node, rel: &[u8], content: &mut dyn Read) -> io::Result<()> {
        self.xattrs(node)?;
        let mut header = tar_header(node, EntryType::Regular);
        header.set_size(node.size);
        self.builder
            .append_data(&mut header, tar_path(rel), content)
    }

    fn hardlink(&mut self, node: &pb::Node, rel: &[u8], first: &[u8]) -> io::Result<()> {
        self.xattrs(node)?;
        let mut header = tar_header(node, EntryType::Link);
        self.builder
            .append_link(&mut header, tar_path(rel), tar_path(first))
    }

    fn other(&mut self, node: &pb::Node, rel: &[u8]) -> io::Result<bool> {
        let kind = match pb::NodeType::try_from(node.r#type) {
            Ok(pb::NodeType::Symlink) => EntryType::Symlink,
            Ok(pb::NodeType::Fifo) => EntryType::Fifo,
            Ok(pb::NodeType::Chardev) => EntryType::Char,
            Ok(pb::NodeType::Blockdev) => EntryType::Block,
            _ => return Ok(false),
        };
        self.xattrs(node)?;
        let mut header = tar_header(node, kind);
        match kind {
            EntryType::Symlink if node.link_target.len() > TAR_LINK_NAME_LEN => self
                .builder
                .append_link(&mut header, tar_path(rel), tar_path(&node.link_target))?,
            EntryType::Symlink => {
                header.set_link_name_literal(&node.link_target)?;
                self.builder
                    .append_data(&mut header, tar_path(rel), io::empty())?;
            }
            _ => {
                if kind != EntryType::Fifo {
                    let (major, minor) = device_numbers(node.rdev);
                    header.set_device_major(major)?;
                    header.set_device_minor(minor)?;
                }
                self.builder
                    .append_data(&mut header, tar_path(rel), io::empty())?;
            }
        }
        Ok(true)
    }

    fn skipped(&self) -> u64 {
        self.skipped
    }
}

struct ZipSink<W: Write> {
    zip: ZipWriter<StreamWriter<W>>,
    names: HashSet<String>,
}

fn zip_time(node: &pb::Node) -> DateTime {
    let stamp = seconds(node).clamp(ZIP_FIRST_SECOND, ZIP_LAST_SECOND);
    time::OffsetDateTime::from_unix_timestamp(stamp)
        .ok()
        .and_then(|t| DateTime::try_from(time::PrimitiveDateTime::new(t.date(), t.time())).ok())
        .unwrap_or_default()
}

/// Entry options with the DOS time and an extended timestamp field (`UT`,
/// 0x5455) holding the UTC mtime. The DOS time has no zone, so readers take it
/// as local time; unzip, 7-Zip and macOS prefer the UTC field when it exists.
fn zip_options(node: &pb::Node) -> io::Result<FullFileOptions<'static, 'static>> {
    let mtime = u32::try_from(seconds(node).max(0)).unwrap_or(u32::MAX);
    let mut field = [1u8; 5];
    field[1..].copy_from_slice(&mtime.to_le_bytes());
    let mut options = SimpleFileOptions::default()
        .last_modified_time(zip_time(node))
        .into_full_options();
    options
        .add_extra_field(ZIP_EXTENDED_TIMESTAMP, field, false)
        .map_err(io::Error::other)?;
    Ok(options)
}

impl<W: Write> ZipSink<W> {
    fn new(out: W) -> Self {
        ZipSink {
            zip: ZipWriter::new_stream(out),
            names: HashSet::new(),
        }
    }

    fn unique(&mut self, rel: &[u8], dir: bool) -> String {
        let base = lossy(rel);
        let mut name = base.clone();
        let mut n = 1u32;
        while !self.names.insert(name.clone()) {
            name = format!("{base}~{n}");
            n += 1;
        }
        if dir { format!("{name}/") } else { name }
    }
}

impl<W: Write> Sink for ZipSink<W> {
    fn hardlinks(&self) -> bool {
        false
    }

    fn dir(&mut self, node: &pb::Node, rel: &[u8]) -> io::Result<()> {
        let name = self.unique(rel, true);
        self.zip
            .add_directory(name, zip_options(node)?)
            .map_err(io::Error::other)
    }

    fn file(&mut self, node: &pb::Node, rel: &[u8], content: &mut dyn Read) -> io::Result<()> {
        let name = self.unique(rel, false);
        let options = zip_options(node)?
            .compression_method(CompressionMethod::Deflated)
            .large_file(node.size >= ZIP_LARGE_FILE);
        self.zip
            .start_file(name, options)
            .map_err(io::Error::other)?;
        io::copy(content, &mut self.zip)?;
        Ok(())
    }

    fn hardlink(&mut self, _: &pb::Node, _: &[u8], _: &[u8]) -> io::Result<()> {
        Err(io::Error::other("a zip has no hard links"))
    }

    fn other(&mut self, _: &pb::Node, _: &[u8]) -> io::Result<bool> {
        Ok(false)
    }

    fn skipped(&self) -> u64 {
        0
    }
}

/// The plaintext of one file, handed over a chunk at a time by `next`; holes
/// of a sparse file come out as zeros.
struct Content<F> {
    next: F,
    extents: Vec<(u64, u64)>,
    extent: usize,
    extent_pos: u64,
    size: u64,
    pos: u64,
    current: Vec<u8>,
    current_pos: usize,
}

impl<F: FnMut() -> io::Result<Option<Vec<u8>>>> Content<F> {
    fn new(next: F, extents: Vec<(u64, u64)>, size: u64) -> Self {
        Content {
            next,
            extents,
            extent: 0,
            extent_pos: 0,
            size,
            pos: 0,
            current: Vec::new(),
            current_pos: 0,
        }
    }
}

impl<F: FnMut() -> io::Result<Option<Vec<u8>>>> Read for Content<F> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let sparse = !self.extents.is_empty();
        while sparse
            && self
                .extents
                .get(self.extent)
                .is_some_and(|&(_, len)| len == 0)
        {
            self.extent += 1;
        }
        if self.pos >= self.size || out.is_empty() {
            return Ok(0);
        }
        let hole_until = if sparse {
            match self.extents.get(self.extent) {
                Some(&(offset, _)) if self.pos < offset => Some(offset),
                Some(_) => None,
                None => Some(self.size),
            }
        } else {
            None
        };
        if let Some(until) = hole_until {
            let n = out
                .len()
                .min(usize::try_from(until - self.pos).unwrap_or(usize::MAX));
            out[..n].fill(0);
            self.pos += n as u64;
            return Ok(n);
        }
        while self.current_pos == self.current.len() {
            self.current =
                (self.next)()?.ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
            self.current_pos = 0;
        }
        let mut n = out.len().min(self.current.len() - self.current_pos);
        if sparse {
            let (_, len) = self.extents[self.extent];
            n = n.min(usize::try_from(len - self.extent_pos).unwrap_or(usize::MAX));
        }
        out[..n].copy_from_slice(&self.current[self.current_pos..self.current_pos + n]);
        self.current_pos += n;
        self.pos += n as u64;
        if sparse {
            self.extent_pos += n as u64;
            if self.extent_pos == self.extents[self.extent].1 {
                self.extent += 1;
                self.extent_pos = 0;
            }
        }
        Ok(n)
    }
}

/// Checks that the chunks of the file add up to what the node claims before a
/// byte of the file is written, then reads it chunk by chunk.
fn file_content<'a, S: BlobStore>(
    handle: &'a Handle,
    reader: &'a mut Reader<S>,
    node: &pb::Node,
) -> anyhow::Result<impl Read + 'a> {
    let extents = check_extents(node)?;
    let held: u64 = handle
        .block_on(reader.chunk_locations(node))?
        .iter()
        .map(|l| u64::from(l.ulen))
        .sum();
    let expected = if extents.is_empty() {
        node.size
    } else {
        extents.iter().map(|e| e.1).sum()
    };
    ensure!(
        held == expected,
        "its chunks hold {held} bytes, the node says {expected}"
    );
    let mut chunks = node
        .content
        .iter()
        .map(|raw| BlobId::try_from(raw.as_slice()))
        .collect::<Result<VecDeque<_>, _>>()
        .context("content id must be 32 bytes")?;
    let next = move || {
        let Some(id) = chunks.pop_front() else {
            return Ok(None);
        };
        handle
            .block_on(reader.read_blob(&id, BLOB_TYPE_DATA))
            .map(Some)
            .map_err(io::Error::other)
    };
    Ok(Content::new(next, extents, node.size))
}

fn walk<S: BlobStore>(
    handle: &Handle,
    reader: &mut Reader<S>,
    snapshot: &BlobId,
    selection: &Selection,
    sink: &mut dyn Sink,
) -> anyhow::Result<ArchiveReport> {
    let mut walk = handle.block_on(SelectionWalk::start(reader, snapshot, selection))?;
    let mut dirs: Vec<Vec<u8>> = Vec::new();
    let mut firsts: HashMap<u64, Vec<u8>> = HashMap::new();
    let mut report = ArchiveReport::default();
    while let Some(step) = handle.block_on(walk.next(reader))? {
        let (node, ancestor, enters) = match step {
            PlanStep::Leave => {
                dirs.pop();
                continue;
            }
            PlanStep::Enter { node, ancestor } => (node, ancestor, true),
            PlanStep::Entry(node) => (node, false, false),
        };
        let mut path = dirs.last().cloned().unwrap_or_default();
        path.push(b'/');
        path.extend_from_slice(&node.name);
        if enters {
            dirs.push(path.clone());
        }
        if ancestor {
            continue;
        }
        let rel = &path[1..];
        let named = || format!("archiving {}", lossy(rel));
        if enters {
            sink.dir(&node, rel).with_context(named)?;
            report.dirs += 1;
        } else if node.r#type == pb::NodeType::File as i32 {
            let first = (node.hardlink_group != 0 && sink.hardlinks())
                .then(|| firsts.get(&node.hardlink_group))
                .flatten();
            if let Some(first) = first {
                sink.hardlink(&node, rel, first).with_context(named)?;
            } else {
                let mut content = file_content(handle, reader, &node).with_context(named)?;
                sink.file(&node, rel, &mut content).with_context(named)?;
                if node.hardlink_group != 0 {
                    firsts
                        .entry(node.hardlink_group)
                        .or_insert_with(|| rel.to_vec());
                }
            }
            report.files += 1;
        } else if !sink.other(&node, rel).with_context(named)? {
            report.skipped += 1;
        }
    }
    report.skipped += sink.skipped();
    Ok(report)
}

/// Passes writes through until the archive is given up on; after that they
/// vanish, so the encoders cannot complete a stream that was cut short when
/// they are dropped.
struct Gate<W> {
    inner: W,
    open: Rc<Cell<bool>>,
}

impl<W: Write> Write for Gate<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.open.get() {
            self.inner.write(buf)
        } else {
            Ok(buf.len())
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.open.get() {
            self.inner.flush()
        } else {
            Ok(())
        }
    }
}

fn tar_archive<S: BlobStore, E: Write>(
    handle: &Handle,
    reader: &mut Reader<S>,
    snapshot: &BlobId,
    selection: &Selection,
    open: &Cell<bool>,
    encoder: E,
    finish: impl FnOnce(E) -> io::Result<()>,
) -> anyhow::Result<ArchiveReport> {
    let mut sink = TarSink::new(encoder);
    match walk(handle, reader, snapshot, selection, &mut sink) {
        Ok(report) => {
            finish(sink.builder.into_inner()?)?;
            Ok(report)
        }
        Err(error) => {
            open.set(false);
            Err(error)
        }
    }
}

/// Writes what `selection` picks from `snapshot` to `out`. Meant for a thread
/// of its own: chunks are fetched with `handle.block_on`. If the archive fails
/// midway it is left unfinished, so it cannot pass for a whole one.
pub fn write_archive<S: BlobStore, W: Write>(
    handle: &Handle,
    reader: &mut Reader<S>,
    snapshot: &BlobId,
    selection: &Selection,
    format: ArchiveFormat,
    out: W,
) -> anyhow::Result<ArchiveReport> {
    let open = Rc::new(Cell::new(true));
    let gate = Gate {
        inner: out,
        open: Rc::clone(&open),
    };
    match format {
        ArchiveFormat::TarGz => tar_archive(
            handle,
            reader,
            snapshot,
            selection,
            &open,
            flate2::write::GzEncoder::new(gate, flate2::Compression::default()),
            |encoder| encoder.finish().map(drop),
        ),
        ArchiveFormat::TarZst => tar_archive(
            handle,
            reader,
            snapshot,
            selection,
            &open,
            zstd::stream::write::Encoder::new(gate, ZSTD_LEVEL)?,
            |encoder| encoder.finish().map(drop),
        ),
        ArchiveFormat::Zip => {
            let mut sink = ZipSink::new(gate);
            match walk(handle, reader, snapshot, selection, &mut sink) {
                Ok(report) => {
                    sink.zip.finish()?;
                    Ok(report)
                }
                Err(error) => {
                    open.set(false);
                    Err(error)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Member = (Vec<u8>, EntryType, Option<Vec<u8>>, Vec<(String, Vec<u8>)>);

    fn chunks(parts: &[&[u8]]) -> impl FnMut() -> io::Result<Option<Vec<u8>>> {
        let mut parts: VecDeque<Vec<u8>> = parts.iter().map(|p| p.to_vec()).collect();
        move || Ok(parts.pop_front())
    }

    fn read_all(mut content: impl Read, buffer: usize) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; buffer];
        loop {
            match content.read(&mut buf)? {
                0 => return Ok(out),
                n => out.extend_from_slice(&buf[..n]),
            }
        }
    }

    #[test]
    fn content_fills_the_holes_of_a_sparse_file() {
        let expected = b"\0\0\0\0abc\0\0\0de\0\0";
        let splits: [&[&[u8]]; 3] = [
            &[b"abc", b"de"],
            &[b"ab", b"cd", b"e"],
            &[b"a", b"", b"bcde"],
        ];
        for parts in splits {
            for buffer in [1, 3, 64] {
                let content = Content::new(chunks(parts), vec![(4, 3), (10, 2)], 14);
                assert_eq!(read_all(content, buffer).unwrap(), expected);
            }
        }
        let leading = Content::new(chunks(&[b"xy"]), vec![(0, 2)], 5);
        assert_eq!(read_all(leading, 2).unwrap(), b"xy\0\0\0");
        let empty_extent = Content::new(chunks(&[b"xy"]), vec![(1, 0), (2, 2)], 4);
        assert_eq!(read_all(empty_extent, 8).unwrap(), b"\0\0xy");
    }

    #[test]
    fn content_dense_reads_through_empty_chunks_and_stops_at_the_size() {
        let content = Content::new(chunks(&[b"ab", b"", b"cde"]), Vec::new(), 5);
        assert_eq!(read_all(content, 2).unwrap(), b"abcde");
        let short = Content::new(chunks(&[b"ab"]), Vec::new(), 5);
        let error = read_all(short, 8).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    fn file_node(name: &[u8], size: u64) -> pb::Node {
        pb::Node {
            name: name.to_vec(),
            r#type: pb::NodeType::File as i32,
            mode: 0o644,
            size,
            mtime: Some(pb::Timestamp {
                seconds: 1_600_000_000,
                nanos: 0,
            }),
            ..Default::default()
        }
    }

    fn tar_members(bytes: &[u8]) -> Vec<Member> {
        let mut archive = tar::Archive::new(bytes);
        archive
            .entries()
            .unwrap()
            .map(|entry| {
                let mut entry = entry.unwrap();
                let pax = entry
                    .pax_extensions()
                    .unwrap()
                    .map(|all| {
                        all.map(|e| {
                            let e = e.unwrap();
                            (e.key().unwrap().to_string(), e.value_bytes().to_vec())
                        })
                        .collect()
                    })
                    .unwrap_or_default();
                (
                    entry.path_bytes().into_owned(),
                    entry.header().entry_type(),
                    entry.link_name_bytes().map(|l| l.into_owned()),
                    pax,
                )
            })
            .collect()
    }

    #[test]
    fn tar_keeps_name_bytes_and_writes_xattrs_as_schily_pax() {
        let mut node = file_node(b"odd-\xff", 4);
        node.xattrs = vec![
            pb::Xattr {
                name: b"user.k".to_vec(),
                value: vec![0, 1, 0xff],
            },
            pb::Xattr {
                name: b"user.\xff".to_vec(),
                value: b"lost".to_vec(),
            },
            pb::Xattr {
                name: b"user.a=b".to_vec(),
                value: b"lost".to_vec(),
            },
        ];
        let mut sink = TarSink::new(Vec::new());
        sink.file(&node, b"dir/odd-\xff", &mut &b"data"[..])
            .unwrap();
        assert_eq!(sink.skipped(), 2);
        let members = tar_members(&sink.builder.into_inner().unwrap());
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].0, b"dir/odd-\xff");
        assert_eq!(
            members[0].3,
            [("SCHILY.xattr.user.k".to_string(), vec![0, 1, 0xff])]
        );
    }

    #[test]
    fn tar_carries_long_names_and_long_symlink_targets() {
        let long = format!("{}/{}", "d".repeat(120), "f".repeat(120));
        let mut link = file_node(b"ln", 0);
        link.r#type = pb::NodeType::Symlink as i32;
        link.link_target = format!("./{}/", "t".repeat(150)).into_bytes();
        let mut short = file_node(b"ln2", 0);
        short.r#type = pb::NodeType::Symlink as i32;
        short.link_target = b"./x/".to_vec();

        let mut sink = TarSink::new(Vec::new());
        sink.file(&file_node(b"f", 1), long.as_bytes(), &mut &b"x"[..])
            .unwrap();
        assert!(sink.other(&link, b"ln").unwrap());
        assert!(sink.other(&short, b"ln2").unwrap());
        let members = tar_members(&sink.builder.into_inner().unwrap());
        assert_eq!(members[0].0, long.as_bytes());
        assert_eq!(members[1].2.as_deref(), Some(link.link_target.as_slice()));
        assert_eq!(members[2].2.as_deref(), Some(&b"./x/"[..]));
    }

    #[test]
    fn tar_writes_device_numbers_and_skips_unknown_types() {
        let mut node = file_node(b"null", 0);
        node.r#type = pb::NodeType::Chardev as i32;
        node.rdev = (1u64 << 8) | 3;
        let mut sink = TarSink::new(Vec::new());
        assert!(sink.other(&node, b"null").unwrap());
        node.r#type = pb::NodeType::Unspecified as i32;
        assert!(!sink.other(&node, b"nothing").unwrap());
        let bytes = sink.builder.into_inner().unwrap();
        let mut archive = tar::Archive::new(&bytes[..]);
        let entry = archive.entries().unwrap().next().unwrap().unwrap();
        assert_eq!(entry.header().entry_type(), EntryType::Char);
        assert_eq!(entry.header().device_major().unwrap(), Some(1));
        assert_eq!(entry.header().device_minor().unwrap(), Some(3));
    }

    #[test]
    fn zip_names_are_lossy_and_stay_unique() {
        let mut sink = ZipSink::new(Vec::new());
        sink.file(&file_node(b"a", 1), b"a\xff", &mut &b"1"[..])
            .unwrap();
        sink.file(&file_node(b"a", 1), b"a\xfe", &mut &b"2"[..])
            .unwrap();
        sink.dir(&file_node(b"d", 0), b"d").unwrap();
        let bytes = sink.zip.finish().unwrap().into_inner();
        let zip = zip::ZipArchive::new(io::Cursor::new(bytes)).unwrap();
        let mut names: Vec<String> = zip.file_names().map(|n| n.unwrap().into_owned()).collect();
        names.sort();
        assert_eq!(names, ["a\u{fffd}", "a\u{fffd}~1", "d/"]);
    }

    #[test]
    fn zip_entries_carry_the_utc_mtime() {
        let mut node = file_node(b"a", 1);
        node.mtime = Some(pb::Timestamp {
            seconds: 1_600_000_000,
            nanos: 0,
        });
        let mut sink = ZipSink::new(Vec::new());
        sink.file(&node, b"a", &mut &b"1"[..]).unwrap();
        sink.dir(&node, b"d").unwrap();
        let bytes = sink.zip.finish().unwrap().into_inner();
        let mut zip = zip::ZipArchive::new(io::Cursor::new(bytes)).unwrap();
        for i in 0..zip.len() {
            let entry = zip.by_index(i).unwrap();
            let stamps: Vec<Option<u32>> = entry
                .extra_data_fields()
                .filter_map(|field| match field {
                    zip::extra_fields::ExtraField::ExtendedTimestamp(t) => Some(t.mod_time()),
                    _ => None,
                })
                .collect();
            assert_eq!(stamps, [Some(1_600_000_000)], "{:?}", entry.name());
            let dos = entry.last_modified().unwrap();
            assert_eq!(
                (dos.year(), dos.month(), dos.day(), dos.hour(), dos.minute()),
                (2020, 9, 13, 12, 26)
            );
        }
    }

    #[test]
    fn zip_time_is_clamped_to_the_dos_range() {
        let mut node = file_node(b"a", 0);
        for (seconds, year) in [(-5, 1980), (1_600_000_000, 2020), (i64::MAX, 2107)] {
            node.mtime = Some(pb::Timestamp { seconds, nanos: 0 });
            assert_eq!(zip_time(&node).year(), year);
        }
    }
}
