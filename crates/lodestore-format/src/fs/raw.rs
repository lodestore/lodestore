//! A regular file or a block device read as a disk image.

use std::fs::File;
use std::io::{ErrorKind, Seek, SeekFrom};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileExt, FileTypeExt};
use std::path::Path;

use super::walk::data_extents;
use crate::error::{Error, Result};
use crate::image::{ImageExtent, ImageSource};

pub struct FileSource {
    file: File,
    size: u64,
    /// `(offset, length)` of the data of a sparse file; `None` reads as all data.
    data: Option<Vec<(u64, u64)>>,
}

impl FileSource {
    pub fn open(path: &Path) -> Result<FileSource> {
        let mut file = File::open(path)?;
        let kind = file.metadata()?.file_type();
        if kind.is_file() {
            let size = file.metadata()?.len();
            let data = data_extents(&OwnedFd::from(file.try_clone()?), size);
            Ok(FileSource { file, size, data })
        } else if kind.is_block_device() {
            let size = file.seek(SeekFrom::End(0))?;
            Ok(FileSource {
                file,
                size,
                data: None,
            })
        } else {
            Err(Error::InvalidInput(format!(
                "{} is not a regular file or a block device",
                path.display()
            )))
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::IMAGE_CHUNK_SIZE;

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
