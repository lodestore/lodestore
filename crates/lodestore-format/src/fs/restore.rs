use std::collections::HashMap;
use std::fs::{File, FileTimes};
use std::io::Write;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::time::SystemTime;

use rustix::fs::{
    AtFlags, CWD, Dir, FileType, Mode, OFlags, ftruncate, mkdirat, openat, renameat, statat,
    symlinkat, unlinkat,
};
use rustix::io::Errno;

use super::conv;
use super::meta::{Target, apply_metadata};
use super::walk::ExtentReader;
use crate::chunker::Chunker;
use crate::crypto::Key32;
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
pub struct RestoreSettings {
    pub overwrite: Overwrite,
    pub verify: bool,
    pub dry_run: bool,
    /// Chunk id key and chunker seed of the repository, to recognise files
    /// that already hold the right content. Wiped when the settings drop.
    pub k_id: Key32,
    pub seed: u64,
    /// Give restored entries the owner of the snapshot. Off, the owner is
    /// whoever creates them and nothing is reported about it.
    pub restore_owners: bool,
    /// With `restore_owners`, also keep setuid, setgid and file capabilities.
    /// Off, the owner is restored and those privileges are dropped.
    pub keep_privileges: bool,
}

/// A file or entry that could not be restored; the rest of the restore went on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreFailure {
    /// Path below the target.
    pub path: Vec<u8>,
    pub error: String,
}

#[derive(Debug, Default)]
pub struct RestoreStats {
    /// Files written or hard-linked: `created + overwritten`.
    pub restored: u64,
    /// Restored files that were not there before.
    pub created: u64,
    /// Restored files that replaced something already there.
    pub overwritten: u64,
    pub bytes: u64,
    pub skipped: u64,
    pub verified: u64,
    pub would_write: u64,
    pub would_skip: u64,
}

/// Whether the chunks of `source`, cut and hashed like a backup does, are
/// exactly the ones in `content`.
fn chunks_match(source: impl std::io::Read, content: &[Vec<u8>], s: &RestoreSettings) -> bool {
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
    /// Access and modification time to put back when the level is left: an
    /// ancestor of the restored paths keeps them although entries come and go.
    /// Left alone when the restore did not change the folder.
    keep_times: Option<(SystemTime, SystemTime)>,
}

/// Writes a snapshot tree below one target directory using only `*at` calls
/// relative to open directory descriptors. Names reach it already validated
/// by the tree decoder and are checked again here.
pub struct Restorer {
    settings: RestoreSettings,
    stats: RestoreStats,
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
    /// Something was at the final name when the file was begun.
    replaced: bool,
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
pub fn check_extents(node: &pb::Node) -> Result<Vec<(u64, u64)>> {
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
    pub fn open(target: &Path, settings: RestoreSettings) -> Result<Self> {
        Self::open_with(target, None, settings)
    }

    /// Opens `below` inside `root` without following any symlink under `root`:
    /// a restore into a folder can never leave it.
    pub fn open_in(root: &Path, below: &Path, settings: RestoreSettings) -> Result<Self> {
        Self::open_with(root, Some(below), settings)
    }

    fn open_with(target: &Path, below: Option<&Path>, settings: RestoreSettings) -> Result<Self> {
        let full = below.map_or_else(|| target.to_path_buf(), |b| target.join(b));
        let virtual_root = settings.dry_run && !full.exists();
        let root = if virtual_root {
            Level {
                fd: None,
                node: None,
                kind: Kind::Virtual,
                keep_times: None,
            }
        } else {
            let me = rustix::process::geteuid().as_raw();
            let trusted = |uid| uid == 0 || uid == me;
            let create = !settings.dry_run;
            let fd = match below {
                None => open_target(target, create, trusted)?,
                Some(below) => {
                    if below.is_absolute() {
                        return Err(Error::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            format!("{} is not relative to the restore root", below.display()),
                        )));
                    }
                    let root_fd = open_target(target, create, trusted)?;
                    open_below(root_fd, below, create, trusted, false)?
                }
            };
            if !settings.dry_run && settings.overwrite != Overwrite::Never {
                remove_leftovers(&fd);
            }
            Level {
                fd: Some(fd),
                node: None,
                kind: Kind::Live,
                keep_times: None,
            }
        };
        Ok(Restorer {
            settings,
            stats: RestoreStats::default(),
            failures: Vec::new(),
            stack: vec![root],
            names: Vec::new(),
            unapplied: Vec::new(),
            links: HashMap::new(),
        })
    }

    /// Whether the opened target can be written, asked of the open folder
    /// itself so no path is walked again.
    pub fn check_writable(&self) -> std::io::Result<()> {
        let Some(fd) = self.stack.first().and_then(|l| l.fd.as_ref()) else {
            return Ok(());
        };
        rustix::fs::accessat(fd, ".", rustix::fs::Access::WRITE_OK, AtFlags::empty())
            .map_err(Into::into)
    }

    pub fn take_unapplied(&mut self) -> Vec<Unapplied> {
        std::mem::take(&mut self.unapplied)
    }

    pub fn stats(&self) -> &RestoreStats {
        &self.stats
    }

    pub fn take_stats(&mut self) -> RestoreStats {
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
                    keep_times: None,
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
                        keep_times: None,
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
                keep_times: None,
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
                keep_times: None,
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
            apply_metadata(
                &Target::Fd(fd.as_fd()),
                node,
                &path,
                self.settings.restore_owners,
                self.settings.keep_privileges,
                &mut self.unapplied,
            );
        }
        if let (Some((accessed, modified)), Some(fd)) = (level.keep_times, &level.fd)
            && let Err(e) = fd.try_clone().and_then(|fd| {
                let dir = File::from(fd);
                if dir.metadata()?.modified()? == modified {
                    return Ok(());
                }
                dir.set_times(
                    FileTimes::new()
                        .set_accessed(accessed)
                        .set_modified(modified),
                )
            })
        {
            self.unapplied.push(Unapplied {
                path,
                what: "times".into(),
                error: e.to_string(),
            });
        }
    }

    /// Enters a directory that only leads to what is being restored. One that
    /// exists is entered as it is: its owner, mode, xattrs and times stay as
    /// they were and nothing in it is removed. One that is missing is created
    /// like [`Restorer::enter_dir`] does. Something that is not a directory is
    /// never replaced: the failure is reported and everything below skipped.
    pub fn enter_ancestor(&mut self, node: &pb::Node) -> Result<()> {
        validate_name(&node.name)?;
        let name = node.name.as_slice();
        if self.kind() != Kind::Live {
            return self.enter_dir(node);
        }
        match self.existing(name)? {
            None => return self.enter_dir(node),
            Some(FileType::Directory) => {}
            Some(_) => {
                self.block_ancestor(name);
                return Ok(());
            }
        }
        let fd = match openat(
            self.here(),
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::LOOP | Errno::NOTDIR) => {
                self.block_ancestor(name);
                return Ok(());
            }
            Err(e) => return Err(self.fail("cannot open directory", name, e)),
        };
        let keep_times = if self.settings.dry_run {
            None
        } else {
            let meta = File::from(fd.try_clone().map_err(Error::Io)?).metadata()?;
            Some((meta.accessed()?, meta.modified()?))
        };
        self.push_level(
            name,
            Level {
                fd: Some(fd),
                node: None,
                kind: Kind::Live,
                keep_times,
            },
        );
        Ok(())
    }

    fn block_ancestor(&mut self, name: &[u8]) {
        self.record_failure(
            name,
            "a non-directory is in the way of a parent folder".into(),
        );
        self.push_level(
            name,
            Level {
                fd: None,
                node: None,
                kind: Kind::Dead,
                keep_times: None,
            },
        );
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
        let existing = self.existing(name)?;
        let replaced = existing.is_some();
        match existing {
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
                                self.settings.restore_owners,
                                self.settings.keep_privileges,
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
            replaced,
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
        apply_metadata(
            &Target::Fd(file.as_fd()),
            node,
            &path,
            self.settings.restore_owners,
            self.settings.keep_privileges,
            &mut self.unapplied,
        );
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
        if sink.replaced {
            self.stats.overwritten += 1;
        } else {
            self.stats.created += 1;
        }
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
        self.stats.created += 1;
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
            self.settings.restore_owners,
            self.settings.keep_privileges,
            &mut unapplied,
        );
        self.unapplied = unapplied;
        self.place(&tmp, &node.name)
    }
}

/// Whether `below` inside `root` holds nothing, read without following any
/// symlink under `root`. A folder that does not exist yet is empty.
pub fn is_empty_below(root: &Path, below: &Path) -> std::io::Result<bool> {
    let me = rustix::process::geteuid().as_raw();
    let trusted = |uid| uid == 0 || uid == me;
    let opened = open_target(root, false, trusted)
        .and_then(|root_fd| open_below(root_fd, below, false, trusted, false));
    let fd = match opened {
        Ok(fd) => fd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(e),
    };
    for entry in Dir::read_from(fd)? {
        let entry = entry?;
        if !matches!(entry.file_name().to_bytes(), b"." | b"..") {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Opens the restore target one component at a time, creating missing folders
/// `0700` when `create` is set. A component that a user other than a
/// `trusted` one could swap is refused: a symlink it owns, or a folder it owns
/// that others can write to or that sits in a folder others can write to
/// (a name planted in `/var/tmp`).
fn open_target(
    target: &Path,
    create: bool,
    trusted: impl Fn(u32) -> bool,
) -> std::io::Result<OwnedFd> {
    let start = if target.is_absolute() { "/" } else { "." };
    let dir = openat(
        CWD,
        start,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    open_below(dir, target, create, trusted, true)
}

fn open_below(
    mut dir: OwnedFd,
    target: &Path,
    create: bool,
    trusted: impl Fn(u32) -> bool,
    follow_trusted_symlinks: bool,
) -> std::io::Result<OwnedFd> {
    use std::path::Component;

    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let refused = |name: &std::ffi::OsStr, why: &str| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing to restore into {}: {} {why}",
                target.display(),
                name.display()
            ),
        )
    };
    let mut shared = false;
    for part in target.components() {
        let name = match part {
            Component::Normal(name) => name,
            Component::RootDir | Component::CurDir => continue,
            Component::ParentDir | Component::Prefix(_) => {
                return Err(refused(part.as_os_str(), "is not allowed in the target"));
            }
        };
        let fd = match openat(&dir, name, flags | OFlags::NOFOLLOW, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) if create => {
                match mkdirat(&dir, name, Mode::from_bits_truncate(conv(0o700u32))) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(e) => return Err(e.into()),
                }
                openat(&dir, name, flags | OFlags::NOFOLLOW, Mode::empty())?
            }
            Err(Errno::LOOP | Errno::NOTDIR) => {
                let st = statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
                if FileType::from_raw_mode(st.st_mode) != FileType::Symlink {
                    return Err(Errno::NOTDIR.into());
                }
                if !follow_trusted_symlinks {
                    return Err(refused(name, "is a symlink inside the restore root"));
                }
                if !trusted(st.st_uid) {
                    return Err(refused(name, "is a symlink owned by another user"));
                }
                openat(&dir, name, flags, Mode::empty())?
            }
            Err(e) => return Err(e.into()),
        };
        let st = rustix::fs::fstat(&fd)?;
        let mode: u32 = conv(st.st_mode);
        if !trusted(st.st_uid) && (mode & 0o022 != 0 || shared) {
            return Err(refused(
                name,
                "belongs to another user and could be swapped by others",
            ));
        }
        shared = mode & 0o002 != 0;
        dir = fd;
    }
    Ok(dir)
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

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    fn base(tmp: &tempfile::TempDir) -> OwnedFd {
        openat(
            CWD,
            tmp.path(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap()
    }

    fn settings() -> RestoreSettings {
        RestoreSettings {
            overwrite: Overwrite::IfChanged,
            verify: false,
            dry_run: false,
            k_id: zeroize::Zeroizing::new([0; 32]),
            seed: 0,
            restore_owners: false,
            keep_privileges: false,
        }
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn open_target_creates_missing_folders_private() {
        let tmp = tempfile::tempdir().unwrap();
        open_below(base(&tmp), Path::new("a/./b"), true, |_| true, true).unwrap();
        assert_eq!(mode(&tmp.path().join("a")), 0o700);
        assert_eq!(mode(&tmp.path().join("a/b")), 0o700);
        let missing = open_below(base(&tmp), Path::new("c"), false, |_| true, true).unwrap_err();
        assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
        assert!(open_below(base(&tmp), Path::new("a/../a"), true, |_| true, true).is_err());
    }

    #[test]
    fn open_in_refuses_symlink_below_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("r");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("a")).unwrap();

        let err = Restorer::open_in(&root, Path::new("a/x"), settings())
            .err()
            .unwrap();
        assert!(err.to_string().contains("symlink"), "{err}");
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);

        Restorer::open_in(&root, Path::new("b/c"), settings()).unwrap();
        assert_eq!(mode(&root.join("b/c")), 0o700);
        assert!(Restorer::open_in(&root, Path::new("../x"), settings()).is_err());
        assert!(Restorer::open_in(&root, Path::new("/x"), settings()).is_err());
    }

    #[test]
    fn open_target_refuses_what_another_user_could_swap() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("real")).unwrap();
        symlink("real", root.join("link")).unwrap();
        std::fs::create_dir(root.join("open")).unwrap();
        std::fs::set_permissions(root.join("open"), std::fs::Permissions::from_mode(0o777))
            .unwrap();
        std::fs::write(root.join("file"), b"x").unwrap();

        let mine = |_| true;
        open_below(base(&tmp), Path::new("link/sub"), true, mine, true).unwrap();
        assert!(
            root.join("real/sub").is_dir(),
            "a trusted symlink is followed"
        );
        open_below(base(&tmp), Path::new("open"), false, mine, true).unwrap();

        let foreign = |_| false;
        for (path, why) in [("link", "symlink"), ("open", "could be swapped")] {
            let err = open_below(base(&tmp), Path::new(path), false, foreign, true).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{path}");
            assert!(err.to_string().contains(why), "{path}: {err}");
        }
        open_below(base(&tmp), Path::new("real"), false, foreign, true).unwrap();

        let not_dir = open_below(base(&tmp), Path::new("file/x"), true, mine, true).unwrap_err();
        assert_eq!(not_dir.raw_os_error(), Some(Errno::NOTDIR.raw_os_error()));
    }
}
