//! Capturing and applying node metadata: extended attributes (raw, so ACLs
//! and capabilities travel as the kernel stores them), owner names, owner,
//! mode and times.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::sync::OnceLock;

use rustix::fs::{
    AtFlags, Gid, Mode, Timestamps, UTIME_OMIT, Uid, XattrFlags, chmodat, chownat, fchmod, fchown,
    fgetxattr, flistxattr, fsetxattr, futimens, lgetxattr, llistxattr, lsetxattr, utimensat,
};
use rustix::io::Errno;

use super::conv;
use super::restore::Unapplied;
use crate::pb;

fn grow(mut f: impl FnMut(&mut [u8]) -> rustix::io::Result<usize>) -> rustix::io::Result<Vec<u8>> {
    loop {
        let need = f(&mut [])?;
        if need == 0 {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; need];
        match f(&mut buf) {
            Ok(n) => {
                buf.truncate(n);
                return Ok(buf);
            }
            // The value grew between the size query and the read.
            Err(Errno::RANGE) => {}
            Err(e) => return Err(e),
        }
    }
}

fn collect(
    list: impl FnMut(&mut [u8]) -> rustix::io::Result<usize>,
    mut get: impl FnMut(&CString, &mut [u8]) -> rustix::io::Result<usize>,
) -> rustix::io::Result<Vec<pb::Xattr>> {
    let names = match grow(list) {
        Ok(n) => n,
        Err(Errno::NOTSUP) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for name in names.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let cname = CString::new(name).expect("split at NUL");
        match grow(|b| get(&cname, b)) {
            Ok(value) => out.push(pb::Xattr {
                name: name.to_vec(),
                value,
            }),
            // Removed after listing.
            Err(Errno::NODATA) => {}
            Err(e) => return Err(e),
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Xattrs of an open file or directory. A filesystem without xattr support
/// yields none.
pub fn capture_xattrs(fd: &impl AsFd) -> rustix::io::Result<Vec<pb::Xattr>> {
    collect(
        |b| flistxattr(fd, b),
        |name, b| fgetxattr(fd, name.as_c_str(), b),
    )
}

/// `/proc/self/fd/<dir>/<name>`: reaches an entry without following it and
/// without opening it, which a symlink, fifo or device cannot do safely.
fn entry_path(dir: BorrowedFd<'_>, name: &[u8]) -> Vec<u8> {
    let mut p = format!("/proc/self/fd/{}/", dir.as_raw_fd()).into_bytes();
    p.extend_from_slice(name);
    p
}

/// Xattrs of a symlink, fifo or device node. Linux only; elsewhere none.
pub fn capture_xattrs_at(dir: &impl AsFd, name: &[u8]) -> rustix::io::Result<Vec<pb::Xattr>> {
    if !cfg!(target_os = "linux") {
        return Ok(Vec::new());
    }
    let path = entry_path(dir.as_fd(), name);
    collect(
        |b| llistxattr(path.as_slice(), b),
        |n, b| lgetxattr(path.as_slice(), n.as_c_str(), b),
    )
}

struct OwnerNames {
    users: HashMap<u32, Vec<u8>>,
    groups: HashMap<u32, Vec<u8>>,
}

fn parse_ids(path: &str) -> HashMap<u32, Vec<u8>> {
    let mut map = HashMap::new();
    for line in std::fs::read(path)
        .unwrap_or_default()
        .split(|b| *b == b'\n')
    {
        let mut fields = line.split(|b| *b == b':');
        let (Some(name), Some(_), Some(id)) = (fields.next(), fields.next(), fields.next()) else {
            continue;
        };
        if let Some(id) = std::str::from_utf8(id).ok().and_then(|s| s.parse().ok()) {
            map.entry(id).or_insert_with(|| name.to_vec());
        }
    }
    map
}

/// Informational user and group names for a numeric owner; empty when the
/// host has no entry.
pub fn owner_names(uid: u32, gid: u32) -> (Vec<u8>, Vec<u8>) {
    static NAMES: OnceLock<OwnerNames> = OnceLock::new();
    let n = NAMES.get_or_init(|| OwnerNames {
        users: parse_ids("/etc/passwd"),
        groups: parse_ids("/etc/group"),
    });
    (
        n.users.get(&uid).cloned().unwrap_or_default(),
        n.groups.get(&gid).cloned().unwrap_or_default(),
    )
}

/// Where to apply metadata: an open file or directory, or an entry reached by
/// name that must not be followed (symlink, fifo, device).
pub enum Target<'a> {
    Fd(BorrowedFd<'a>),
    Entry {
        dir: BorrowedFd<'a>,
        name: &'a [u8],
        symlink: bool,
    },
}

fn timespec(ts: Option<&pb::Timestamp>) -> rustix::fs::Timespec {
    match ts {
        Some(t) => rustix::fs::Timespec {
            tv_sec: conv(t.seconds),
            tv_nsec: conv(t.nanos),
        },
        None => rustix::fs::Timespec {
            tv_sec: 0,
            tv_nsec: UTIME_OMIT as _,
        },
    }
}

/// Applies owner, xattrs, mode and times, in that order: a chown clears
/// setuid and setgid, an ACL xattr rewrites the group bits, and any of them
/// touches ctime. Every failure is recorded in `out` and the rest still runs.
pub fn apply_metadata(target: &Target<'_>, node: &pb::Node, path: &[u8], out: &mut Vec<Unapplied>) {
    let mut fail = |what: String, e: Errno| {
        out.push(Unapplied {
            path: path.to_vec(),
            what,
            error: e.to_string(),
        });
    };

    let (uid, gid) = (Uid::from_raw(node.uid), Gid::from_raw(node.gid));
    let owner = match target {
        Target::Fd(fd) => fchown(fd, Some(uid), Some(gid)),
        Target::Entry { dir, name, .. } => {
            chownat(dir, *name, Some(uid), Some(gid), AtFlags::SYMLINK_NOFOLLOW)
        }
    };
    if let Err(e) = owner {
        fail("owner".into(), e);
    }

    for x in &node.xattrs {
        let set = match target {
            Target::Fd(fd) => fsetxattr(fd, x.name.as_slice(), &x.value, XattrFlags::empty()),
            Target::Entry { dir, name, .. } => lsetxattr(
                entry_path(*dir, name).as_slice(),
                x.name.as_slice(),
                &x.value,
                XattrFlags::empty(),
            ),
        };
        if let Err(e) = set {
            fail(format!("xattr {}", String::from_utf8_lossy(&x.name)), e);
        }
    }

    let mode = Mode::from_bits_truncate(conv(node.mode & 0o7777));
    let chmod = match target {
        Target::Fd(fd) => Some(fchmod(fd, mode)),
        // A symlink has no mode of its own on Linux.
        Target::Entry { symlink: true, .. } => None,
        Target::Entry { dir, name, .. } => Some(chmodat(dir, *name, mode, AtFlags::empty())),
    };
    if let Some(Err(e)) = chmod {
        fail("mode".into(), e);
    }

    if node.atime.is_some() || node.mtime.is_some() {
        let times = Timestamps {
            last_access: timespec(node.atime.as_ref()),
            last_modification: timespec(node.mtime.as_ref()),
        };
        let r = match target {
            Target::Fd(fd) => futimens(fd, &times),
            Target::Entry { dir, name, .. } => {
                utimensat(dir, *name, &times, AtFlags::SYMLINK_NOFOLLOW)
            }
        };
        if let Err(e) = r {
            fail("times".into(), e);
        }
    }
}
