use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;

use rustix::fs::{
    AtFlags, CWD, Dir, FileType, Mode, OFlags, SeekFrom, Stat, fstat, openat, seek, statat,
};
use rustix::io::Errno;

use super::filter::PathMatcher;
use super::{conv, meta, timestamp};
use crate::pb;

/// Reading never updates access time where the kernel lets us ask for that,
/// so an unchanged tree keeps the access times it is recorded with.
#[cfg(target_os = "linux")]
const NOATIME: OFlags = OFlags::NOATIME;
#[cfg(not(target_os = "linux"))]
const NOATIME: OFlags = OFlags::empty();

fn open_quiet(parent: &OwnedFd, name: &[u8], flags: OFlags) -> rustix::io::Result<OwnedFd> {
    match openat(parent, name, flags | NOATIME, Mode::empty()) {
        // O_NOATIME is only allowed on files we own.
        Err(Errno::PERM) => openat(parent, name, flags, Mode::empty()),
        r => r,
    }
}

/// Data extents of a file as `(offset, length)`, or `None` when the file is
/// dense (one extent covering it all) or the filesystem cannot tell.
pub fn data_extents(fd: &OwnedFd, size: u64) -> Option<Vec<(u64, u64)>> {
    let mut out = Vec::new();
    let mut pos = 0u64;
    while pos < size {
        let data = match seek(fd, SeekFrom::Data(pos)) {
            Ok(d) => d,
            Err(Errno::NXIO) => break,
            Err(_) => return None,
        };
        let hole = seek(fd, SeekFrom::Hole(data)).ok()?;
        let end = hole.min(size);
        if end <= data {
            return None;
        }
        out.push((data, end - data));
        pos = end;
    }
    seek(fd, SeekFrom::Start(0)).ok()?;
    (out != [(0, size)]).then_some(out)
}

/// The concatenation of a file's data extents, read with `pread`.
pub struct ExtentReader {
    file: File,
    extents: Vec<(u64, u64)>,
    index: usize,
    done: u64,
}

impl ExtentReader {
    pub fn new(file: File, extents: Vec<(u64, u64)>) -> Self {
        ExtentReader {
            file,
            extents,
            index: 0,
            done: 0,
        }
    }
}

impl Read for ExtentReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while let Some(&(offset, len)) = self.extents.get(self.index) {
            if self.done == len {
                self.index += 1;
                self.done = 0;
                continue;
            }
            if buf.is_empty() {
                return Ok(0);
            }
            let want = usize::try_from(len - self.done).map_or(buf.len(), |n| n.min(buf.len()));
            let got = self.file.read_at(&mut buf[..want], offset + self.done)?;
            if got == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            self.done += got as u64;
            return Ok(got);
        }
        Ok(0)
    }
}

/// What the backup walk leaves out.
#[derive(Clone, Debug, Default)]
pub struct WalkOptions {
    /// Paths matching this are not entered, or recorded if they are files.
    pub exclude: Option<PathMatcher>,
    /// A directory on another device than the backed-up path it is below is
    /// recorded as an empty directory.
    pub one_file_system: bool,
    /// A directory holding a valid `CACHEDIR.TAG` keeps only that file.
    pub exclude_caches: bool,
}

pub const CACHEDIR_TAG: &[u8] = b"CACHEDIR.TAG";
const CACHEDIR_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";

impl Walker {
    /// Whether `dirfd` holds a `CACHEDIR.TAG` that is a regular file starting
    /// with the signature from the Cache Directory Tagging Specification.
    pub fn has_cache_tag(dirfd: &OwnedFd) -> bool {
        let Ok(fd) = Walker::open_file(dirfd, CACHEDIR_TAG) else {
            return false;
        };
        if !fstat(&fd).is_ok_and(|st| Walker::node_type(&st) == Some(pb::NodeType::File)) {
            return false;
        }
        let mut head = Vec::new();
        File::from(fd)
            .take(CACHEDIR_SIGNATURE.len() as u64)
            .read_to_end(&mut head)
            .is_ok()
            && head == CACHEDIR_SIGNATURE
    }
}

pub struct DirEntry {
    pub name: Vec<u8>,
    pub stat: rustix::io::Result<Stat>,
}

/// Directory-fd based reads of the source tree. Every call takes a single name
/// relative to an open directory, so a symlink swapped in during the walk is
/// never followed.
pub struct Walker;

impl Walker {
    pub fn open_root() -> rustix::io::Result<OwnedFd> {
        openat(
            CWD,
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
    }

    pub fn open_dir(parent: &OwnedFd, name: &[u8]) -> rustix::io::Result<OwnedFd> {
        open_quiet(
            parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        )
    }

    /// Opens a file for reading. `NONBLOCK` keeps a fifo swapped in after the
    /// `stat` from blocking the walk; the caller re-checks the type with `fstat`.
    pub fn open_file(parent: &OwnedFd, name: &[u8]) -> rustix::io::Result<OwnedFd> {
        open_quiet(
            parent,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        )
    }

    pub fn stat(parent: &OwnedFd, name: &[u8]) -> rustix::io::Result<Stat> {
        statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
    }

    /// Entries of a directory ordered by name bytes, without `.` and `..`.
    pub fn read_dir(dirfd: &OwnedFd) -> rustix::io::Result<Vec<DirEntry>> {
        let mut out = Vec::new();
        for entry in Dir::read_from(dirfd)? {
            let entry = entry?;
            let name = entry.file_name();
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            out.push(DirEntry {
                name: name.to_bytes().to_vec(),
                stat: statat(dirfd, name, AtFlags::SYMLINK_NOFOLLOW),
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// The `hardlink_group` of a regular file: 0 for a file with one name,
    /// otherwise a number (from 1, per snapshot) shared by all names of the inode.
    pub fn hardlink_group(groups: &mut HashMap<(u64, u64), u64>, st: &Stat) -> u64 {
        if st.st_nlink <= 1 {
            return 0;
        }
        let next = groups.len() as u64 + 1;
        *groups
            .entry((conv(st.st_dev), conv(st.st_ino)))
            .or_insert(next)
    }

    pub fn node_type(st: &Stat) -> Option<pb::NodeType> {
        match FileType::from_raw_mode(st.st_mode) {
            FileType::RegularFile => Some(pb::NodeType::File),
            FileType::Directory => Some(pb::NodeType::Dir),
            FileType::Symlink => Some(pb::NodeType::Symlink),
            FileType::Fifo => Some(pb::NodeType::Fifo),
            FileType::CharacterDevice => Some(pb::NodeType::Chardev),
            FileType::BlockDevice => Some(pb::NodeType::Blockdev),
            _ => None,
        }
    }

    /// The manifest node for an entry. Directories outside the backed-up
    /// paths (`bare`) carry no times and no link count, since they change for
    /// reasons unrelated to the data (a sibling directory appearing in `/tmp`). Access time is recorded on Linux only, where reads do not
    /// disturb it (see `open_quiet`); elsewhere the backup's own read would give
    /// an unchanged tree a new id on every run.
    pub fn node_of(name: &[u8], st: &Stat, ty: pb::NodeType, bare: bool) -> pb::Node {
        let is_file = ty == pb::NodeType::File;
        let is_device = matches!(ty, pb::NodeType::Chardev | pb::NodeType::Blockdev);
        let (uid, gid): (u32, u32) = (conv(st.st_uid), conv(st.st_gid));
        let (user, group) = meta::owner_names(uid, gid);
        pb::Node {
            name: name.to_vec(),
            r#type: ty as i32,
            mode: conv::<_, u32>(st.st_mode) & 0o7777,
            uid,
            gid,
            user,
            group,
            mtime: (!bare).then(|| timestamp(st.st_mtime, st.st_mtime_nsec)),
            atime: (!bare && cfg!(target_os = "linux"))
                .then(|| timestamp(st.st_atime, st.st_atime_nsec)),
            ctime: (!bare).then(|| timestamp(st.st_ctime, st.st_ctime_nsec)),
            size: if is_file { conv(st.st_size) } else { 0 },
            rdev: if is_device { conv(st.st_rdev) } else { 0 },
            inode: conv(st.st_ino),
            dev: conv(st.st_dev),
            nlink: if bare { 0 } else { conv(st.st_nlink) },
            ..Default::default()
        }
    }
}
