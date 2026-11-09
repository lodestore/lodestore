//! Integrity checks of a repository, run with read keys.
//!
//! Each check reads the bucket and reports what it found. A problem with the
//! data is a [`Finding`] in the report, never an `Err`: `Err` means the check
//! could not finish, for instance because the bucket is unreachable or the
//! keys do not fit.

use crate::crypto::Entropy;
use crate::error::Error;

pub mod packs;
pub mod sample;
pub mod structure;

/// Findings, and notes, kept in a report; the rest is only counted in
/// `findings_total` and `notes_total`.
pub const MAX_FINDINGS: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FindingKind {
    MissingPack,
    UnknownBlob,
    UnreadableSnapshot,
    UnreadableTree,
    UnreadableIndex,
    PackMismatch,
    DamagedBlob,
    UnreadablePack,
    UnreadableConfig,
    RestoreMismatch,
    RestoreFailed,
}

impl FindingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FindingKind::MissingPack => "missing_pack",
            FindingKind::UnknownBlob => "unknown_blob",
            FindingKind::UnreadableSnapshot => "unreadable_snapshot",
            FindingKind::UnreadableTree => "unreadable_tree",
            FindingKind::UnreadableIndex => "unreadable_index",
            FindingKind::PackMismatch => "pack_mismatch",
            FindingKind::DamagedBlob => "damaged_blob",
            FindingKind::UnreadablePack => "unreadable_pack",
            FindingKind::UnreadableConfig => "unreadable_config",
            FindingKind::RestoreMismatch => "restore_mismatch",
            FindingKind::RestoreFailed => "restore_failed",
        }
    }
}

/// Something wrong in the repository: the check fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub kind: FindingKind,
    /// The object key or path the finding is about.
    pub key: String,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NoteKind {
    UnindexedPack,
    PendingCleanup,
    StaleCleanup,
    IgnoredObject,
    LocksUnreadable,
}

impl NoteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            NoteKind::UnindexedPack => "unindexed_pack",
            NoteKind::PendingCleanup => "pending_cleanup",
            NoteKind::StaleCleanup => "stale_cleanup",
            NoteKind::IgnoredObject => "ignored_object",
            NoteKind::LocksUnreadable => "locks_unreadable",
        }
    }
}

/// Worth knowing, not wrong: a normal state of a repository in use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub kind: NoteKind,
    pub key: String,
    pub detail: String,
}

/// Findings of one check, capped at [`MAX_FINDINGS`].
#[derive(Debug, Default)]
pub(crate) struct Findings {
    pub list: Vec<Finding>,
    pub total: u64,
}

impl Findings {
    pub fn add(&mut self, kind: FindingKind, key: impl Into<String>, detail: impl Into<String>) {
        self.total += 1;
        if self.list.len() < MAX_FINDINGS {
            self.list.push(Finding {
                kind,
                key: key.into(),
                detail: detail.into(),
            });
        }
    }
}

/// Notes of one check, capped at [`MAX_FINDINGS`] like the findings.
#[derive(Debug, Default)]
pub(crate) struct Notes {
    pub list: Vec<Note>,
    pub total: u64,
}

impl Notes {
    pub fn add(&mut self, kind: NoteKind, key: impl Into<String>, detail: impl Into<String>) {
        self.total += 1;
        if self.list.len() < MAX_FINDINGS {
            self.list.push(Note {
                kind,
                key: key.into(),
                detail: detail.into(),
            });
        }
    }
}

/// Whether `e` says the data read is bad, as opposed to the read failing.
pub fn is_data_error(e: &Error) -> bool {
    matches!(
        e,
        Error::Corrupt { .. } | Error::Crypto | Error::UnsupportedVersion { .. }
    )
}

/// A uniform number in `0..n`; `n` must not be 0.
pub fn random_below(entropy: &mut Entropy, n: u64) -> u64 {
    let zone = u64::MAX / n * n;
    loop {
        let x = u64::from_le_bytes(entropy.bytes::<8>());
        if x < zone {
            return x % n;
        }
    }
}

pub fn shuffle<T>(items: &mut [T], entropy: &mut Entropy) {
    for i in (1..items.len()).rev() {
        let j = random_below(entropy, i as u64 + 1) as usize;
        items.swap(i, j);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shuffle_keeps_every_item() {
        let mut entropy = Entropy::os();
        let mut items: Vec<u32> = (0..100).collect();
        shuffle(&mut items, &mut entropy);
        assert_ne!(items, (0..100).collect::<Vec<_>>());
        items.sort_unstable();
        assert_eq!(items, (0..100).collect::<Vec<_>>());
        assert!((0..1000).all(|_| random_below(&mut entropy, 7) < 7));
    }
}
