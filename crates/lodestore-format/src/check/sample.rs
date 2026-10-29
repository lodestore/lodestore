//! The test restore: restore a random sample of files into a scratch folder,
//! compare their chunks with the snapshot and delete them again.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::check::{Finding, FindingKind, Findings, is_data_error, random_below, shuffle};
use crate::crypto::{BlobId, Entropy};
use crate::error::{Error, Result};
use crate::index::hex32;
use crate::pb;
use crate::reader::{Reader, RestoreOptions};
use crate::store::BlobStore;

const MISMATCH: &str = "does not match";

#[derive(Debug, Clone)]
pub struct SampleOptions {
    /// Files to restore over the whole sample.
    pub files: u32,
    /// Upper bound of the data restored; the free space of `scratch` can
    /// lower it, never raise it.
    pub max_bytes: u64,
    /// An existing folder; each file is restored into a folder of its own
    /// below it, which is removed right after the file is checked.
    pub scratch: PathBuf,
    /// Tree nodes the walk over the snapshots may visit in the whole run;
    /// keeps a snapshot whose directories share subtrees from walking forever.
    pub max_nodes: u64,
}

/// Default of [`SampleOptions::max_nodes`].
pub const MAX_SAMPLE_NODES: u64 = 100_000_000;

#[derive(Debug, Default)]
pub struct SampleReport {
    pub snapshots_sampled: u64,
    pub files_restored: u64,
    pub bytes_restored: u64,
    /// Files whose restore failed or whose content did not match.
    pub hash_mismatches: u64,
    /// Files left out because they did not fit in the size limit.
    pub skipped: u64,
    /// The size limit, or the limit of tree nodes walked, left files out.
    pub stopped_at_limit: bool,
    pub findings: Vec<Finding>,
}

fn reason(e: &Error) -> String {
    match e {
        Error::Corrupt { reason, .. } => reason.clone(),
        other => other.to_string(),
    }
}

fn path_text(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

/// Restores up to `opts.files` random regular files from random `snapshots`
/// with `verify` on, and reports the ones that did not come back right. The
/// data restored stays within the smaller of `opts.max_bytes` and half the
/// free space of the scratch folder; the walk over the snapshots visits at
/// most `opts.max_nodes` tree nodes. `should_stop` is asked for every node
/// walked and between files; `progress` gets the files done and the files
/// chosen.
///
/// The returned future is not `Send`, because the reader's restore takes
/// non-`Send` callbacks: run it with `spawn_blocking` and
/// `Handle::block_on`.
///
/// ponytail: a snapshot with fewer files than its share leaves the sample
/// short instead of passing the remainder on to the others.
pub async fn sample_restore<S: BlobStore>(
    reader: &mut Reader<S>,
    snapshots: &[BlobId],
    opts: &SampleOptions,
    entropy: &mut Entropy,
    progress: &dyn Fn(u64, u64),
    should_stop: &dyn Fn() -> bool,
) -> Result<SampleReport> {
    let mut report = SampleReport::default();
    let mut findings = Findings::default();
    let free = rustix::fs::statvfs(&opts.scratch)?;
    let budget = opts
        .max_bytes
        .min(free.f_bavail.saturating_mul(free.f_frsize) / 2);
    if snapshots.is_empty() || opts.files == 0 {
        return Ok(report);
    }

    let mut order = snapshots.to_vec();
    shuffle(&mut order, entropy);
    order.truncate(opts.files as usize);
    let (share, extra) = (
        opts.files as usize / order.len(),
        opts.files as usize % order.len(),
    );
    let mut chosen: Vec<(BlobId, Vec<u8>, u64)> = Vec::new();
    let mut too_big = 0u64;
    let mut visited = 0u64;
    let mut over_budget = false;
    for (i, id) in order.iter().enumerate() {
        let quota = share + usize::from(i < extra);
        let mut seen = 0u64;
        let mut picked: Vec<(Vec<u8>, u64)> = Vec::new();
        let walked = reader
            .walk_subtree(id, b"", &mut |path, node| {
                if should_stop() {
                    return Err(Error::Interrupted);
                }
                visited += 1;
                if visited > opts.max_nodes {
                    over_budget = true;
                    return Err(Error::Interrupted);
                }
                if node.r#type != pb::NodeType::File as i32 {
                    return Ok(());
                }
                if node.size > budget {
                    too_big += 1;
                    return Ok(());
                }
                seen += 1;
                if picked.len() < quota {
                    picked.push((path.to_vec(), node.size));
                } else {
                    let slot = random_below(entropy, seen) as usize;
                    if slot < quota {
                        picked[slot] = (path.to_vec(), node.size);
                    }
                }
                Ok(())
            })
            .await;
        match walked {
            Ok(()) => chosen.extend(picked.into_iter().map(|(p, size)| (*id, p, size))),
            Err(Error::Interrupted) => {
                report.stopped_at_limit |= over_budget;
                break;
            }
            Err(e) if is_data_error(&e) => {
                findings.add(FindingKind::RestoreFailed, hex32(id), reason(&e));
                report.hash_mismatches += 1;
            }
            Err(e) => return Err(e),
        }
    }
    report.skipped += too_big;
    report.stopped_at_limit |= too_big > 0;

    let total = chosen.len() as u64;
    let mut sampled: HashSet<BlobId> = HashSet::new();
    for (n, (id, path, size)) in chosen.into_iter().enumerate() {
        if should_stop() {
            break;
        }
        if report.bytes_restored + size > budget {
            report.skipped += 1;
            report.stopped_at_limit = true;
            continue;
        }
        let dir = opts.scratch.join(n.to_string());
        std::fs::create_dir(&dir)?;
        let options = RestoreOptions {
            path: path.clone(),
            verify: true,
            skip_owners: true,
            ..Default::default()
        };
        let restored = reader.restore(&id, &dir, &options, &|_| {}).await;
        let cleaned = std::fs::remove_dir_all(&dir);
        match restored {
            Ok(done) => {
                sampled.insert(id);
                report.files_restored += done.files;
                report.bytes_restored += done.bytes;
                for failure in done.errors {
                    let kind = if failure.error.contains(MISMATCH) {
                        FindingKind::RestoreMismatch
                    } else {
                        FindingKind::RestoreFailed
                    };
                    findings.add(kind, path_text(&path), failure.error);
                    report.hash_mismatches += 1;
                }
            }
            Err(e) if is_data_error(&e) => {
                sampled.insert(id);
                findings.add(FindingKind::RestoreFailed, path_text(&path), reason(&e));
                report.hash_mismatches += 1;
            }
            Err(e) => return Err(e),
        }
        cleaned?;
        progress(n as u64 + 1, total);
    }
    report.snapshots_sampled = sampled.len() as u64;
    report.findings = findings.list;
    Ok(report)
}
