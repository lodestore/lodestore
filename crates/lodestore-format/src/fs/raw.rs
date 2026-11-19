//! A regular file or a block device read as a disk image.

use std::fs::File;
use std::io::{ErrorKind, Seek, SeekFrom};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt};
use std::path::Path;

use rustix::fs::{Mode, OFlags};

use super::restore::check_extents;
use super::walk::data_extents;
use crate::error::{Error, Result};
use crate::image::{ImageExtent, ImageSource};
use crate::pb;

pub struct FileSource {
    file: File,
    size: u64,
    /// `(offset, length)` of the data of a sparse file; `None` reads as all data.
    data: Option<Vec<(u64, u64)>>,
    stamp: Stamp,
}

/// What must not change while the image is read.
#[derive(Debug, PartialEq, Eq)]
enum Stamp {
    File {
        len: u64,
        mtime: (i64, i64),
        ctime: (i64, i64),
    },
    Device(u64),
}

fn stamp(file: &mut File) -> Result<Stamp> {
    let meta = file.metadata()?;
    let kind = meta.file_type();
    if kind.is_file() {
        Ok(Stamp::File {
            len: meta.len(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        })
    } else if kind.is_block_device() {
        Ok(Stamp::Device(file.seek(SeekFrom::End(0))?))
    } else {
        Err(Error::InvalidInput("not a regular file or a block device".into()))
    }
}

impl FileSource {
    pub fn open(path: &Path) -> Result<FileSource> {
        // NONBLOCK so that a FIFO at `path` fails the type check instead of hanging the open.
        let fd = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|e| Error::Io(e.into()))?;
        let mut file = File::from(fd);
        let stamp = stamp(&mut file).map_err(|e| match e {
            Error::InvalidInput(_) => Error::InvalidInput(format!(
                "{} is not a regular file or a block device",
                path.display()
            )),
            e => e,
        })?;
        let flags = rustix::fs::fcntl_getfl(&file).map_err(|e| Error::Io(e.into()))?;
        rustix::fs::fcntl_setfl(&file, flags - OFlags::NONBLOCK).map_err(|e| Error::Io(e.into()))?;
        let (size, data) = match stamp {
            Stamp::File { len, .. } => (len, data_extents(&OwnedFd::from(file.try_clone()?), len)),
            Stamp::Device(size) => (size, None),
        };
        Ok(FileSource {
            file,
            size,
            data,
            stamp,
        })
    }
}

impl ImageSource for FileSource {
    fn size(&self) -> u64 {
        self.size
    }

    async fn extents(&mut self, offset: u64, len: u64) -> Result<Vec<ImageExtent>> {
        let end = offset
            .checked_add(len)
            .filter(|&e| e <= self.size)
            .ok_or_else(|| Error::InvalidInput("the range is past the end of the image".into()))?;
        let Some(data) = &self.data else {
            return Ok(vec![ImageExtent { len, zero: false }]);
        };
        let mut out = Vec::new();
        let mut pos = offset;
        let mut push = |from: u64, to: u64, zero: bool| {
            if to > from {
                out.push(ImageExtent {
                    len: to - from,
                    zero,
                });
            }
        };
        for &(d_off, d_len) in data {
            let d_end = d_off.saturating_add(d_len);
            if d_end <= pos {
                continue;
            }
            if d_off >= end {
                break;
            }
            let start = d_off.max(pos);
            push(pos, start, true);
            pos = d_end.min(end);
            push(start, pos, false);
        }
        push(pos, end, true);
        Ok(out)
    }

    fn finish(&mut self) -> Result<()> {
        if stamp(&mut self.file)? != self.stamp {
            return Err(Error::InvalidInput(
                "the image source changed while it was read; stop what writes to it, \
                 or back up a snapshot of it, and run the backup again"
                    .into(),
            ));
        }
        Ok(())
    }

    async fn read(&mut self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        self.file
            .read_exact_at(&mut buf, offset)
            .map_err(|e| match e.kind() {
                ErrorKind::UnexpectedEof => {
                    Error::InvalidInput(format!("the image ended at {offset}"))
                }
                _ => Error::Io(e),
            })?;
        Ok(buf)
    }
}

/// Opens a block device for writing, exclusively: the kernel refuses a device
/// that is mounted, in swap or held by another device. Returns its size.
pub fn open_block_device(path: &Path) -> Result<(File, u64)> {
    // NONBLOCK so that a FIFO at `path` fails the type check instead of hanging the open.
    let fd = rustix::fs::open(
        path,
        OFlags::WRONLY | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|e| Error::Io(e.into()))?;
    let mut file = File::from(fd);
    if !file.metadata()?.file_type().is_block_device() {
        return Err(Error::InvalidInput(format!(
            "{} is not a block device",
            path.display()
        )));
    }
    let size = file.seek(SeekFrom::End(0))?;
    Ok((file, size))
}

/// Writes one image node into an open file or device: data at its extents,
/// zeros in every hole, nothing past `size`.
pub struct RawSink<'a> {
    file: &'a File,
    name: String,
    size: u64,
    written: u64,
    extents: Vec<(u64, u64)>,
    extent: usize,
    extent_pos: u64,
}

impl<'a> RawSink<'a> {
    pub fn new(file: &'a File, node: &pb::Node) -> Result<RawSink<'a>> {
        Ok(RawSink {
            file,
            name: String::from_utf8_lossy(&node.name).into_owned(),
            size: node.size,
            written: 0,
            extents: check_extents(node)?,
            extent: 0,
            extent_pos: 0,
        })
    }

    pub fn write(&mut self, mut data: &[u8]) -> Result<()> {
        while !data.is_empty() {
            let Some(&(offset, len)) = self.extents.get(self.extent) else {
                return Err(Error::corrupt(
                    self.name.clone(),
                    "content is longer than its data extents",
                ));
            };
            let room = len - self.extent_pos;
            if room == 0 {
                self.extent += 1;
                self.extent_pos = 0;
                continue;
            }
            let n = usize::try_from(room).map_or(data.len(), |r| r.min(data.len()));
            self.file
                .write_all_at(&data[..n], offset + self.extent_pos)?;
            self.extent_pos += n as u64;
            self.written += n as u64;
            data = &data[n..];
        }
        Ok(())
    }

    /// Zeroes the holes when asked, syncs, and returns the image size.
    pub fn finish(self, zero_holes: bool) -> Result<u64> {
        let expected: u64 = self.extents.iter().map(|e| e.1).sum();
        if self.written != expected {
            return Err(Error::corrupt(
                self.name,
                format!(
                    "content is {} bytes, the node says {expected}",
                    self.written
                ),
            ));
        }
        if zero_holes {
            let mut pos = 0;
            for &(offset, len) in &self.extents {
                write_zeros_at(self.file, pos, offset - pos)?;
                pos = offset + len;
            }
            write_zeros_at(self.file, pos, self.size - pos)?;
        }
        self.file.sync_all()?;
        Ok(self.size)
    }
}

fn write_zeros_at(file: &File, mut at: u64, mut n: u64) -> Result<()> {
    static ZEROS: [u8; 64 << 10] = [0; 64 << 10];
    while n > 0 {
        let step = usize::try_from(n).map_or(ZEROS.len(), |n| n.min(ZEROS.len()));
        file.write_all_at(&ZEROS[..step], at)?;
        at += step as u64;
        n -= step as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::IMAGE_CHUNK_SIZE;

    fn image_node(size: u64, extents: &[(u64, u64)]) -> pb::Node {
        pb::Node {
            name: b"disk.img".to_vec(),
            size,
            sparse_extents: extents
                .iter()
                .map(|&(offset, length)| pb::Extent { offset, length })
                .collect(),
            ..Default::default()
        }
    }

    fn filled(len: u64) -> (tempfile::TempDir, File) {
        let dir = tempfile::tempdir().unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(dir.path().join("dev"))
            .unwrap();
        file.write_all_at(&vec![0xFF; len as usize], 0).unwrap();
        (dir, file)
    }

    #[test]
    fn raw_sink_zeroes_holes_and_keeps_the_tail() {
        let mib = 1u64 << 20;
        let (_dir, file) = filled(20 * mib);
        let node = image_node(12 * mib, &[(4 * mib, 4 * mib)]);
        let mut sink = RawSink::new(&file, &node).unwrap();
        sink.write(&vec![7; 3 * mib as usize]).unwrap();
        sink.write(&vec![7; mib as usize]).unwrap();
        assert_eq!(sink.finish(true).unwrap(), 12 * mib);
        let mut all = vec![0u8; 20 * mib as usize];
        file.read_exact_at(&mut all, 0).unwrap();
        let m = mib as usize;
        assert!(all[..4 * m].iter().all(|&b| b == 0));
        assert!(all[4 * m..8 * m].iter().all(|&b| b == 7));
        assert!(all[8 * m..12 * m].iter().all(|&b| b == 0));
        assert!(all[12 * m..].iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn raw_sink_rejects_content_longer_than_extents() {
        let (_dir, file) = filled(1 << 20);
        let node = image_node(1 << 20, &[(0, 4096)]);
        let mut sink = RawSink::new(&file, &node).unwrap();
        assert!(matches!(sink.write(&[1; 4097]), Err(Error::Corrupt { .. })));
    }

    #[test]
    fn raw_sink_rejects_short_content() {
        let (_dir, file) = filled(1 << 20);
        let node = image_node(1 << 20, &[(0, 4096)]);
        let mut sink = RawSink::new(&file, &node).unwrap();
        sink.write(&[1; 4095]).unwrap();
        assert!(matches!(sink.finish(true), Err(Error::Corrupt { .. })));
    }

    #[test]
    fn open_block_device_refuses_a_regular_file() {
        let (dir, _file) = filled(4096);
        let err = open_block_device(&dir.path().join("dev")).unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
        assert!(err.to_string().contains("not a block device"), "{err}");
    }

    #[tokio::test]
    async fn file_source_reports_holes_of_a_sparse_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.raw");
        let file = File::create(&path).unwrap();
        file.set_len(3 * IMAGE_CHUNK_SIZE).unwrap();
        file.write_all_at(b"data", IMAGE_CHUNK_SIZE + 8).unwrap();
        drop(file);

        let mut source = FileSource::open(&path).unwrap();
        assert_eq!(source.size(), 3 * IMAGE_CHUNK_SIZE);
        let first = source.extents(0, IMAGE_CHUNK_SIZE).await.unwrap();
        let second = source
            .extents(IMAGE_CHUNK_SIZE, IMAGE_CHUNK_SIZE)
            .await
            .unwrap();
        let total = |e: &[ImageExtent]| e.iter().map(|x| x.len).sum::<u64>();
        assert_eq!(total(&first), IMAGE_CHUNK_SIZE);
        assert_eq!(total(&second), IMAGE_CHUNK_SIZE);
        // A filesystem that cannot report holes reads everything as data.
        if first.iter().any(|e| e.zero) {
            assert_eq!(first.len(), 1);
            assert!(second.iter().any(|e| !e.zero));
        }
        let read = source.read(IMAGE_CHUNK_SIZE + 8, 4).await.unwrap();
        assert_eq!(read, b"data");
        assert!(source.read(3 * IMAGE_CHUNK_SIZE - 2, 4).await.is_err());

        assert!(matches!(
            FileSource::open(dir.path()),
            Err(Error::InvalidInput(_))
        ));
    }
}
