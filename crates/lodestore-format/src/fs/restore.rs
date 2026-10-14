use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::path::Path;

use rustix::fs::{
    AtFlags, CWD, Dir, FileType, Mode, OFlags, ftruncate, mkdirat, openat, renameat, statat,
    symlinkat, unlinkat,
};
use rustix::io::Errno;

use super::conv;
use super::meta::{Target, apply_metadata};
use super::walk::ExtentReader;
use crate::chunker::Chunker;
use crate::error::{Error, Result};
use crate::pb;
use crate::tree::validate_name;

const TMP_PREFIX: &str = ".lode-restore-";

/// What to do with a file that already exists in the target.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Overwrite {
    /// Replace it.
    Always,
    /// Keep it when it holds the same chunks, replace it otherwise.
    #[default]
    IfChanged,
    /// Leave it alone.
    Never,
}

/// How a [`Restorer`] treats what it finds in the target.
#[derive(Clone)]
pub(crate) struct Settings {
    pub overwrite: Overwrite,
    pub verify: bool,
    pub dry_run: bool,
    /// Chunk id key and chunker seed of the repository, to recognise files
    /// that already hold the right content.
    pub k_id: [u8; 32],
    pub seed: u64,
}

/// A file or entry that could not be restored; the rest of the restore went on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreFailure {
    /// Path below the target.
    pub path: Vec<u8>,
    pub error: String,
}

#[derive(Debug, Default)]
pub(crate) struct Stats {
    pub restored: u64,
    pub bytes: u64,
    pub skipped: u64,
    pub verified: u64,
    pub would_write: u64,
    pub would_skip: u64,
}

/// Whether the chunks of `source`, cut and hashed like a backup does, are
/// exactly the ones in `content`.
fn chunks_match(source: impl std::io::Read, content: &[Vec<u8>], s: &Settings) -> bool {
    let mut ids = content.iter();
    for chunk in Chunker::new(source, &s.k_id, s.seed) {
        let Ok(chunk) = chunk else { return false };
        if ids.next().is_none_or(|id| id.as_slice() != chunk.id) {
            return false;
        }
    }
    ids.next().is_none()
}

/// What a level of the directory stack stands for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// An open directory in the target.
    Live,
    /// A dry run, where this directory does not exist yet.
    Virtual,
    /// Skipped, with the reason already reported.
    Dead,
}

/// Something the restore created but could not fully apply (an owner, an
/// xattr, a mode, a time, a device node or a hardlink). Never aborts the
/// restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unapplied {
    pub path: Vec<u8>,
    pub what: String,
    pub error: String,
}

struct Level {
    fd: Option<OwnedFd>,
    node: Option<pb::Node>,
    kind: Kind,
}

/// Writes a snapshot tree below one target directory using only `*at` calls
/// relative to open directory descriptors. Names reach it already validated
/// by the tree decoder and are checked again here.
pub struct Restorer {
    settings: Settings,
    stats: Stats,
    failures: Vec<RestoreFailure>,
    stack: Vec<Level>,
    names: Vec<Vec<u8>>,
    unapplied: Vec<Unapplied>,
    /// First restored member of each hardlink group, as a path below the target.
    links: HashMap<u64, Vec<u8>>,
}

/// A file being written under a temporary name; it takes its final name in
/// [`Restorer::finish_file`] and is removed if dropped before that.
pub struct FileSink {
    file: Option<File>,
    dir: OwnedFd,
    tmp: String,
    name: Vec<u8>,
    written: u64,
    extents: Vec<(u64, u64)>,
    extent: usize,
    extent_pos: u64,
    done: bool,
}

impl FileSink {
    /// Appends the next piece of the file's content. A dense file takes it
    /// sequentially; a sparse one spreads it over its data extents.
    pub fn write(&mut self, mut data: &[u8]) -> Result<()> {
        let file = self.file.as_mut().ok_or(Error::Interrupted)?;
        if self.extents.is_empty() {
            file.write_all(data)?;
            self.written += data.len() as u64;
            return Ok(());
        }
        while !data.is_empty() {
            let Some(&(offset, len)) = self.extents.get(self.extent) else {
                return Err(Error::corrupt(
                    String::from_utf8_lossy(&self.name),
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
            file.write_all_at(&data[..n], offset + self.extent_pos)?;
            self.extent_pos += n as u64;
            self.written += n as u64;
            data = &data[n..];
        }
        Ok(())
    }
}

impl Drop for FileSink {
    fn drop(&mut self) {
        if !self.done {
            let _ = unlinkat(&self.dir, self.tmp.as_str(), AtFlags::empty());
        }
    }
}

/// Extents must be ascending, disjoint and inside the file.
pub(crate) fn check_extents(node: &pb::Node) -> Result<Vec<(u64, u64)>> {
    let bad = || {
        Error::corrupt(
            String::from_utf8_lossy(&node.name),
            "sparse extents are out of order or outside the file",
        )
    };
    let mut end = 0u64;
    let mut out = Vec::with_capacity(node.sparse_extents.len());
    for e in &node.sparse_extents {
        let stop = e.offset.checked_add(e.length).ok_or_else(bad)?;
        if e.offset < end || stop > node.size {
            return Err(bad());
        }
        end = stop;
        out.push((e.offset, e.length));
    }
    Ok(out)
}

fn tmp_name() -> Result<String> {
    let mut rnd = [0u8; 8];
    getrandom::fill(&mut rnd).map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?;
    Ok(format!(
        "{TMP_PREFIX}{}",
        rnd.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}

impl Restorer {
    /// Opens the target once, creating it unless this is a dry run.
    pub(crate) fn open(target: &Path, settings: Settings) -> Result<Self> {
        let virtual_root = settings.dry_run && !target.exists();
        let root = if virtual_root {
            Level {
                fd: None,
                node: None,
                kind: Kind::Virtual,
            }
        } else {
            if !settings.dry_run {
                std::fs::create_dir_all(target)?;
            }
            let fd = openat(
                CWD,
                target,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
            if !settings.dry_run && settings.overwrite != Overwrite::Never {
                remove_leftovers(&fd);
            }
            Level {
                fd: Some(fd),
                node: None,
                kind: Kind::Live,
            }
        };
        Ok(Restorer {
            settings,
            stats: Stats::default(),
            failures: Vec::new(),
            stack: vec![root],
            names: Vec::new(),
            unapplied: Vec::new(),
            links: HashMap::new(),
        })
    }

    pub fn take_unapplied(&mut self) -> Vec<Unapplied> {
        std::mem::take(&mut self.unapplied)
    }

    pub(crate) fn stats(&self) -> &Stats {
        &self.stats
    }

    pub(crate) fn take_stats(&mut self) -> Stats {
        std::mem::take(&mut self.stats)
    }

    pub fn take_failures(&mut self) -> Vec<RestoreFailure> {
        std::mem::take(&mut self.failures)
    }

    fn kind(&self) -> Kind {
        self.stack.last().map_or(Kind::Dead, |l| l.kind)
    }

    /// The directory being written; only called while [`Restorer::kind`] is `Live`.
    fn here(&self) -> &OwnedFd {
        self.stack
            .last()
            .and_then(|l| l.fd.as_ref())
            .expect("a live level holds an open directory")
    }

    /// Path of `name` below the target, for reports.
    pub fn path_of(&self, name: &[u8]) -> Vec<u8> {
        let mut p = self.names.join(&b'/');
        if !p.is_empty() {
            p.push(b'/');
        }
        p.extend_from_slice(name);
        p
    }

    fn fail(&self, what: &str, name: &[u8], e: Errno) -> Error {
        let path = String::from_utf8_lossy(&self.path_of(name)).into_owned();
        let io: std::io::Error = e.into();
        Error::Io(std::io::Error::new(
            io.kind(),
            format!("{what} {path}: {e}"),
        ))
    }

    fn note(&mut self, name: &[u8], what: &str, e: Errno) {
        self.unapplied.push(Unapplied {
            path: self.path_of(name),
            what: what.to_owned(),
            error: e.to_string(),
        });
    }

    /// Records a problem with one entry; the restore goes on with the rest.
    pub fn record_failure(&mut self, name: &[u8], error: String) {
        self.failures.push(RestoreFailure {
            path: self.path_of(name),
            error,
        });
    }

    fn skip_file(&mut self) {
        if self.settings.dry_run {
            self.stats.would_skip += 1;
        } else {
            self.stats.skipped += 1;
        }
    }

    /// What is at `name` in the current directory, without following symlinks.
    fn existing(&self, name: &[u8]) -> Result<Option<FileType>> {
        match statat(self.here(), name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => Ok(Some(FileType::from_raw_mode(st.st_mode))),
            Err(Errno::NOENT) => Ok(None),
            Err(e) => Err(self.fail("cannot inspect", name, e)),
        }
    }

    fn push_level(&mut self, name: &[u8], level: Level) {
        self.names.push(name.to_vec());
        self.stack.push(level);
    }

    /// Creates (or reuses) a directory and descends into it. It is created
    /// `0700`; its owner, xattrs, mode and times are applied in
    /// [`Restorer::leave_dir`], once its children exist. Something else in the
    /// way is replaced, or with `Overwrite::Never` the directory is skipped
    /// with everything below it.
    pub fn enter_dir(&mut self, node: &pb::Node) -> Result<()> {
        validate_name(&node.name)?;
        let name = node.name.as_slice();
        let kind = self.kind();
        if kind != Kind::Live {
            self.push_level(
                name,
                Level {
                    fd: None,
                    node: None,
                    kind,
                },
            );
            return Ok(());
        }
        let dry = self.settings.dry_run;
        let mut created = false;
        match self.existing(name)? {
            Some(FileType::Directory) => {}
            None if dry => {
                self.push_virtual(name);
                return Ok(());
            }
            None => created = true,
            Some(_) if self.settings.overwrite == Overwrite::Never => {
                self.record_failure(
                    name,
                    "a file is in the way of this directory and --overwrite never was given".into(),
                );
                self.push_level(
                    name,
                    Level {
                        fd: None,
                        node: None,
                        kind: Kind::Dead,
                    },
                );
                return Ok(());
            }
            Some(_) if dry => {
                self.push_virtual(name);
                return Ok(());
            }
            Some(_) => {
                unlinkat(self.here(), name, AtFlags::empty())
                    .map_err(|e| self.fail("cannot replace", name, e))?;
                created = true;
            }
        }
        if created {
            match mkdirat(self.here(), name, Mode::from_bits_truncate(conv(0o700u32))) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(e) => return Err(self.fail("cannot create directory", name, e)),
            }
        }
        let fd = openat(
            self.here(),
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| self.fail("cannot open directory", name, e))?;
        if !dry && self.settings.overwrite != Overwrite::Never {
            remove_leftovers(&fd);
        }
        let apply = !dry && (created || self.settings.overwrite != Overwrite::Never);
        self.push_level(
            name,
            Level {
                fd: Some(fd),
                node: apply.then(|| pb::Node {
                    subtrees: Vec::new(),
                    ..node.clone()
                }),
                kind: Kind::Live,
            },
        );
        Ok(())
    }

    fn push_virtual(&mut self, name: &[u8]) {
        self.push_level(
            name,
            Level {
                fd: None,
                node: None,
                kind: Kind::Virtual,
            },
        );
    }

    /// Applies the directory entered last, after its children.
    pub fn leave_dir(&mut self) {
        if self.stack.len() < 2 {
            return;
        }
        let level = self.stack.pop().expect("length checked above");
        let path = self.names.join(&b'/');
        self.names.pop();
        if let (Some(node), Some(fd)) = (&level.node, &level.fd) {
            apply_metadata(&Target::Fd(fd.as_fd()), node, &path, &mut self.unapplied);
        }
    }

    fn remember_link(&mut self, node: &pb::Node) {
        if node.hardlink_group != 0 {
            let path = self.path_of(&node.name);
            self.links.entry(node.hardlink_group).or_insert(path);
        }
    }

    /// Decides what to do with a regular file. `None` means there is nothing
    /// to write: the file was skipped, counted for a dry run, or could not be
    /// restored (reported). Otherwise the content goes into the sink.
    pub fn begin_file(&mut self, node: &pb::Node) -> Result<Option<FileSink>> {
        validate_name(&node.name)?;
        let extents = check_extents(node)?;
        match self.kind() {
            Kind::Dead => return Ok(None),
            Kind::Virtual => {
                self.stats.would_write += 1;
                self.remember_link(node);
                return Ok(None);
            }
            Kind::Live => {}
        }
        let name = node.name.as_slice();
        match self.existing(name)? {
            Some(FileType::Directory) => {
                self.record_failure(name, "a directory is in the way of this file".into());
                return Ok(None);
            }
            Some(FileType::RegularFile) => match self.settings.overwrite {
                Overwrite::Never => {
                    self.skip_file();
                    return Ok(None);
                }
                Overwrite::IfChanged => {
                    if let Some(file) = self.identical(name, node) {
                        if !self.settings.dry_run {
                            let path = self.path_of(name);
                            apply_metadata(
                                &Target::Fd(file.as_fd()),
                                node,
                                &path,
                                &mut self.unapplied,
                            );
                            self.remember_link(node);
                        }
                        self.skip_file();
                        return Ok(None);
                    }
                }
                Overwrite::Always => {}
            },
            Some(_) if self.settings.overwrite == Overwrite::Never => {
                self.skip_file();
                return Ok(None);
            }
            Some(_) | None => {}
        }
        if self.settings.dry_run {
            self.stats.would_write += 1;
            self.remember_link(node);
            return Ok(None);
        }
        let tmp = tmp_name()?;
        let fd = openat(
            self.here(),
            tmp.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_bits_truncate(conv(0o600u32)),
        )
        .map_err(|e| self.fail("cannot create file", &node.name, e))?;
        let dir = self.here().try_clone().map_err(Error::Io)?;
        Ok(Some(FileSink {
            file: Some(File::from(fd)),
            dir,
            tmp,
            name: node.name.clone(),
            written: 0,
            extents,
            extent: 0,
            extent_pos: 0,
            done: false,
        }))
    }

    /// The existing file at `name`, opened, when it already holds this node's
    /// content: the same size and the same chunk ids.
    // ponytail: a sparse node is never recognised and is written again; reading
    // the existing file through the node's extents would not see data in its
    // holes anyway.
    fn identical(&self, name: &[u8], node: &pb::Node) -> Option<File> {
        if !node.sparse_extents.is_empty() {
            return None;
        }
        let fd = openat(
            self.here(),
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .ok()?;
        let file = File::from(fd);
        let meta = file.metadata().ok()?;
        if !meta.is_file() || meta.len() != node.size {
            return None;
        }
        let reader = file.try_clone().ok()?;
        chunks_match(reader, &node.content, &self.settings).then_some(file)
    }

    /// Reads the temporary file back and compares its chunk ids with the node's.
    fn verify(&self, dir: &OwnedFd, tmp: &str, node: &pb::Node, extents: &[(u64, u64)]) -> bool {
        let Ok(fd) = openat(
            dir,
            tmp,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) else {
            return false;
        };
        let file = File::from(fd);
        if extents.is_empty() {
            chunks_match(file, &node.content, &self.settings)
        } else {
            chunks_match(
                ExtentReader::new(file, extents.to_vec()),
                &node.content,
                &self.settings,
            )
        }
    }

    /// Checks the length, verifies it if asked, applies owner, xattrs, mode
    /// and times, and renames into place.
    pub fn finish_file(&mut self, mut sink: FileSink, node: &pb::Node) -> Result<()> {
        let sparse = !sink.extents.is_empty();
        let expected = if sparse {
            sink.extents.iter().map(|e| e.1).sum()
        } else {
            node.size
        };
        if sink.written != expected {
            return Err(Error::corrupt(
                String::from_utf8_lossy(&self.path_of(&node.name)),
                format!(
                    "content is {} bytes, the node says {expected}",
                    sink.written
                ),
            ));
        }
        let file = sink.file.take().ok_or(Error::Interrupted)?;
        if sparse && let Err(e) = ftruncate(&file, node.size) {
            return Err(self.fail("cannot size file", &node.name, e));
        }
        if self.settings.verify {
            if !self.verify(&sink.dir, &sink.tmp, node, &sink.extents) {
                return Err(Error::corrupt(
                    String::from_utf8_lossy(&self.path_of(&node.name)),
                    "the file read back from disk does not match the snapshot",
                ));
            }
            self.stats.verified += 1;
        }
        let path = self.path_of(&node.name);
        apply_metadata(&Target::Fd(file.as_fd()), node, &path, &mut self.unapplied);
        drop(file);
        renameat(
            &sink.dir,
            sink.tmp.as_str(),
            &sink.dir,
            sink.name.as_slice(),
        )
        .map_err(|e| self.fail("cannot place file", &sink.name, e))?;
        sink.done = true;
        self.stats.restored += 1;
        self.stats.bytes += node.size;
        if node.hardlink_group != 0 {
            self.links.entry(node.hardlink_group).or_insert(path);
        }
        Ok(())
    }

    /// Makes `tmp` in the current directory another name for the file already
    /// restored at `src` (a path below the target).
    #[cfg(target_os = "linux")]
    fn link_from(&self, src: &[u8], tmp: &str) -> rustix::io::Result<()> {
        use rustix::fs::{ResolveFlags, fstat, linkat};
        use std::os::fd::AsRawFd;

        let source = rustix::fs::openat2(
            self.stack[0].fd.as_ref().ok_or(Errno::NOENT)?,
            src,
            OFlags::PATH | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
        )?;
        if FileType::from_raw_mode(fstat(&source)?.st_mode) != FileType::RegularFile {
            return Err(Errno::INVAL);
        }
        let magic = format!("/proc/self/fd/{}", source.as_raw_fd());
        linkat(
            CWD,
            magic.as_str(),
            self.here(),
            tmp,
            AtFlags::SYMLINK_FOLLOW,
        )
    }

    // Development hosts only; this resolves intermediate symlinks.
    #[cfg(not(target_os = "linux"))]
    fn link_from(&self, src: &[u8], tmp: &str) -> rustix::io::Result<()> {
        let root = self.stack[0].fd.as_ref().ok_or(Errno::NOENT)?;
        rustix::fs::linkat(root, src, self.here(), tmp, AtFlags::empty())
    }

    /// For a file that belongs to a hardlink group already restored, links it
    /// to that first member and returns `true`; the caller then skips reading
    /// its content. When the link cannot be made the member is restored as a
    /// separate file and the reason is listed.
    pub fn link_if_seen(&mut self, node: &pb::Node) -> Result<bool> {
        validate_name(&node.name)?;
        if node.hardlink_group == 0 {
            return Ok(false);
        }
        let Some(src) = self.links.get(&node.hardlink_group).cloned() else {
            return Ok(false);
        };
        match self.kind() {
            Kind::Dead => return Ok(true),
            Kind::Virtual => {
                self.stats.would_write += 1;
                return Ok(true);
            }
            Kind::Live => {}
        }
        match self.existing(&node.name)? {
            Some(FileType::Directory) => {
                self.record_failure(&node.name, "a directory is in the way of this file".into());
                return Ok(true);
            }
            Some(_) if self.settings.overwrite == Overwrite::Never => {
                self.skip_file();
                return Ok(true);
            }
            _ => {}
        }
        if self.settings.dry_run {
            self.stats.would_write += 1;
            return Ok(true);
        }
        let tmp = tmp_name()?;
        if let Err(e) = self.link_from(&src, &tmp) {
            self.note(&node.name, "hardlink", e);
            return Ok(false);
        }
        self.place(&tmp, &node.name)?;
        self.stats.restored += 1;
        self.stats.bytes += node.size;
        Ok(true)
    }

    fn place(&self, tmp: &str, name: &[u8]) -> Result<()> {
        renameat(self.here(), tmp, self.here(), name).map_err(|e| {
            let _ = unlinkat(self.here(), tmp, AtFlags::empty());
            self.fail("cannot place", name, e)
        })
    }

    #[cfg(not(target_vendor = "apple"))]
    fn mknod(&self, tmp: &str, ty: pb::NodeType, rdev: u64) -> rustix::io::Result<()> {
        let kind = match ty {
            pb::NodeType::Chardev => FileType::CharacterDevice,
            pb::NodeType::Blockdev => FileType::BlockDevice,
            _ => FileType::Fifo,
        };
        rustix::fs::mknodat(
            self.here(),
            tmp,
            kind,
            Mode::from_bits_truncate(conv(0o600u32)),
            rdev,
        )
    }

    // Development hosts only.
    #[cfg(target_vendor = "apple")]
    fn mknod(&self, _: &str, _: pb::NodeType, _: u64) -> rustix::io::Result<()> {
        Err(Errno::NOSYS)
    }

    /// Symlinks, fifos and device nodes. Each is created under a temporary
    /// name, given its metadata by name without following it, and renamed into
    /// place, so nothing is ever written through a symlink.
    pub fn create_other(&mut self, node: &pb::Node) -> Result<()> {
        validate_name(&node.name)?;
        if self.kind() != Kind::Live {
            return Ok(());
        }
        let ty = pb::NodeType::try_from(node.r#type).unwrap_or_default();
        match self.existing(&node.name)? {
            Some(FileType::Directory) => {
                self.record_failure(&node.name, "a directory is in the way of this entry".into());
                return Ok(());
            }
            Some(_) if self.settings.overwrite == Overwrite::Never => return Ok(()),
            _ => {}
        }
        if self.settings.dry_run {
            return Ok(());
        }
        let tmp = tmp_name()?;
        let made = match ty {
            pb::NodeType::Symlink => {
                if node.link_target.is_empty() || node.link_target.contains(&0) {
                    return Err(Error::corrupt(
                        String::from_utf8_lossy(&node.name),
                        "symlink target is empty or contains NUL",
                    ));
                }
                symlinkat(node.link_target.as_slice(), self.here(), tmp.as_str())
            }
            pb::NodeType::Fifo | pb::NodeType::Chardev | pb::NodeType::Blockdev => {
                self.mknod(&tmp, ty, node.rdev)
            }
            _ => {
                self.unapplied.push(Unapplied {
                    path: self.path_of(&node.name),
                    what: format!("{ty:?} node"),
                    error: "this node type is not restored by this build".into(),
                });
                return Ok(());
            }
        };
        match made {
            Ok(()) => {}
            // Without CAP_MKNOD a device cannot be made; that is reported, not fatal.
            Err(e)
                if e == Errno::NOSYS
                    || matches!(ty, pb::NodeType::Chardev | pb::NodeType::Blockdev) =>
            {
                self.note(&node.name, "device node", e);
                return Ok(());
            }
            Err(e) => return Err(self.fail("cannot create", &node.name, e)),
        }
        let path = self.path_of(&node.name);
        let mut unapplied = std::mem::take(&mut self.unapplied);
        apply_metadata(
            &Target::Entry {
                dir: self.here().as_fd(),
                name: tmp.as_bytes(),
                symlink: ty == pb::NodeType::Symlink,
            },
            node,
            &path,
            &mut unapplied,
        );
        self.unapplied = unapplied;
        self.place(&tmp, &node.name)
    }
}

/// Removes what an interrupted restore left behind in `dir`. The prefix differs
/// from the local store's temp files, so a restore over a live repository
/// folder leaves that repository's in-flight writes alone.
fn remove_leftovers(dir: &OwnedFd) {
    let Ok(entries) = Dir::read_from(dir) else {
        return;
    };
    let stale: Vec<_> = entries
        .flatten()
        .filter(|e| e.file_name().to_bytes().starts_with(TMP_PREFIX.as_bytes()))
        .map(|e| e.file_name().to_owned())
        .collect();
    for name in stale {
        let _ = unlinkat(dir, name.as_c_str(), AtFlags::empty());
    }
}
