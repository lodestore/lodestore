//! Counting what a backup is going to read, ahead of the backup itself.
//!
//! The walk applies the same filters as the backup's own (`exclude`,
//! `one_file_system`, `exclude_caches`) so that, once it is done, the totals
//! equal what the backup read or reused. Directories it cannot read are
//! skipped silently: the backup reports them.

use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rustix::fs::Stat;

use super::conv;
use super::walk::{CACHEDIR_TAG, WalkOptions, Walker};
use crate::pb;

/// Regular files and their bytes seen so far; final once `done()`.
#[derive(Debug, Default)]
pub struct ScanTotals {
    files: AtomicU64,
    bytes: AtomicU64,
    done: AtomicBool,
}

impl ScanTotals {
    pub fn files(&self) -> u64 {
        self.files.load(Ordering::Relaxed)
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    pub fn done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
}

struct Scan<'a> {
    walk: &'a WalkOptions,
    stop: &'a AtomicBool,
    cancel: &'a AtomicBool,
    totals: &'a ScanTotals,
    root_dev: u64,
}

/// Counts the files below `roots` (canonical paths, as the backup takes them).
/// Returns early without marking the totals done when `stop` or `cancel` is set.
pub fn scan(
    roots: &[PathBuf],
    walk: &WalkOptions,
    stop: &AtomicBool,
    cancel: &AtomicBool,
    totals: &ScanTotals,
) {
    let mut taken: Vec<&Path> = Vec::new();
    for root in roots {
        if taken.iter().any(|t| root.starts_with(t)) {
            continue;
        }
        taken.push(root);
        let mut scan = Scan {
            walk,
            stop,
            cancel,
            totals,
            root_dev: 0,
        };
        let _ = scan.root(root);
        if scan.stopped() {
            return;
        }
    }
    totals.done.store(true, Ordering::Release);
}

impl Scan<'_> {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed) || self.cancel.load(Ordering::Relaxed)
    }

    fn root(&mut self, root: &Path) -> rustix::io::Result<()> {
        let mut fd = Walker::open_root()?;
        let mut path = Vec::new();
        let comps: Vec<&[u8]> = root
            .components()
            .filter_map(|c| match c {
                Component::Normal(n) => Some(n.as_bytes()),
                _ => None,
            })
            .collect();
        let Some((last, parents)) = comps.split_last() else {
            self.root_dev = conv(rustix::fs::fstat(&fd)?.st_dev);
            return self.dir(&fd, b"");
        };
        for name in parents {
            fd = Walker::open_dir(&fd, name)?;
            path.push(b'/');
            path.extend_from_slice(name);
        }
        path.push(b'/');
        path.extend_from_slice(last);
        let st = Walker::stat(&fd, last)?;
        if self.excluded(&path, &st) {
            return Ok(());
        }
        self.root_dev = conv(st.st_dev);
        self.entry(&fd, last, &st, &path);
        Ok(())
    }

    fn excluded(&self, path: &[u8], st: &Stat) -> bool {
        self.walk
            .exclude
            .as_ref()
            .is_some_and(|m| m.is_match(path, Walker::node_type(st) == Some(pb::NodeType::Dir)))
    }

    fn entry(&self, dirfd: &OwnedFd, name: &[u8], st: &Stat, path: &[u8]) {
        match Walker::node_type(st) {
            Some(pb::NodeType::File) => {
                self.totals.files.fetch_add(1, Ordering::Relaxed);
                self.totals
                    .bytes
                    .fetch_add(conv(st.st_size), Ordering::Relaxed);
            }
            Some(pb::NodeType::Dir) => {
                if self.walk.one_file_system && conv::<_, u64>(st.st_dev) != self.root_dev {
                    return;
                }
                if let Ok(sub) = Walker::open_dir(dirfd, name) {
                    let _ = self.dir(&sub, path);
                }
            }
            _ => {}
        }
    }

    fn dir(&self, dirfd: &OwnedFd, path: &[u8]) -> rustix::io::Result<()> {
        let mut entries = Walker::read_dir(dirfd)?;
        if self.walk.exclude_caches && Walker::has_cache_tag(dirfd) {
            entries.retain(|e| e.name == CACHEDIR_TAG);
        }
        for entry in entries {
            if self.stopped() {
                return Ok(());
            }
            let Ok(st) = entry.stat else { continue };
            let mut full = path.to_vec();
            full.push(b'/');
            full.extend_from_slice(&entry.name);
            if self.excluded(&full, &st) {
                continue;
            }
            self.entry(dirfd, &entry.name, &st, &full);
        }
        Ok(())
    }
}
