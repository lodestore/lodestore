//! Which snapshots the server or the password holder has signed, and which
//! snapshot is `latest`. A snapshot is verified when a MAC-valid attestation
//! holds a sealed provenance record that opens and names it. The manifest is
//! written by the host being backed up, so its time and names never decide
//! the order of a verified repository.

use std::collections::HashMap;

use crate::attest::{AttestedSet, open_provenance};
use crate::crypto::BlobId;
use crate::error::{Error, Result};
use crate::keys::ReadKeys;
use crate::pb;
use crate::snapshot::SnapshotEntry;

pub const FUTURE_TOLERANCE_MS: i64 = 600_000;

pub const NO_VERIFIED_SNAPSHOT: &str = "No snapshot in this repository is signed by the server or by the password holder. Name one by id; `lode snapshots` lists them.";
pub const ATTESTATIONS_MISSING: &str = "This repository was upgraded for server signatures, but no signature can be read now; they may have been deleted. Name a snapshot by id; `lode snapshots` lists them.";
pub const ALL_STORED_IN_FUTURE: &str = "Every snapshot in this repository was stored with a time in the future. Name one by id; `lode snapshots` lists them.";

/// Who sealed a provenance record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signer {
    Server,
    /// Empty `host_id` and `job_id`: the holder of the repository password.
    Owner,
}

fn signer_of(record: &pb::ProvenanceRecord) -> Signer {
    if record.host_id.is_empty() && record.job_id.is_empty() {
        Signer::Owner
    } else {
        Signer::Server
    }
}

fn verified_at(record: &pb::ProvenanceRecord) -> (i64, u32) {
    record
        .verified_at
        .as_ref()
        .map_or((0, 0), |t| (t.seconds, t.nanos))
}

fn verified_at_ms(record: &pb::ProvenanceRecord) -> i64 {
    let (seconds, nanos) = verified_at(record);
    seconds
        .saturating_mul(1000)
        .saturating_add(i64::from(nanos / 1_000_000))
}

/// True when `new` should replace `old`, two records of one snapshot: a
/// server record beats the owner's, then the earlier `verified_at` wins, and
/// on a tie the one seen first stays.
pub(crate) fn supersedes(new: &pb::ProvenanceRecord, old: &pb::ProvenanceRecord) -> bool {
    match (signer_of(new), signer_of(old)) {
        (Signer::Server, Signer::Owner) => true,
        (Signer::Owner, Signer::Server) => false,
        _ => verified_at(new) < verified_at(old),
    }
}

/// The provenance records of a repository that opened.
#[derive(Debug, Clone, Default)]
pub struct ProvenanceSet {
    records: HashMap<BlobId, pb::ProvenanceRecord>,
    attested_any: bool,
    unreadable: usize,
}

impl ProvenanceSet {
    /// Opens every sealed record of the MAC-verified attestations; one that
    /// does not open is counted and skipped.
    pub fn from_attested(set: &AttestedSet, keys: &ReadKeys) -> Self {
        let mut out = ProvenanceSet {
            attested_any: !set.objects.is_empty(),
            ..Default::default()
        };
        for sealed in &set.sealed_provenance {
            let Ok(record) = open_provenance(keys, sealed) else {
                out.unreadable += 1;
                continue;
            };
            let Ok(id) = BlobId::try_from(record.snapshot_id.as_slice()) else {
                out.unreadable += 1;
                continue;
            };
            match out.records.get(&id) {
                Some(old) if !supersedes(&record, old) => {}
                _ => {
                    out.records.insert(id, record);
                }
            }
        }
        out
    }

    pub fn get(&self, id: &BlobId) -> Option<&pb::ProvenanceRecord> {
        self.records.get(id)
    }

    pub fn contains(&self, id: &BlobId) -> bool {
        self.records.contains_key(id)
    }

    pub fn signer(&self, id: &BlobId) -> Option<Signer> {
        self.records.get(id).map(signer_of)
    }

    /// At least one MAC-valid attestation exists. Planted objects that fail
    /// the MAC do not count.
    pub fn attested_any(&self) -> bool {
        self.attested_any
    }

    /// Sealed records that did not open.
    pub fn unreadable(&self) -> usize {
        self.unreadable
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatestMode {
    /// Only signed snapshots compete.
    Strict,
    /// No attestation exists: the order comes from the store's clock alone.
    Legacy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LatestWarning {
    /// Nothing in the repository is signed.
    Legacy,
    /// Manifests that claim a time ahead of the clock.
    FutureTime {
        count: usize,
        newest: BlobId,
    },
    /// Snapshots the store says it wrote ahead of the clock; not chosen.
    FutureStored {
        count: usize,
        newest: BlobId,
    },
    /// Unsigned snapshots stored after the chosen one.
    NewerUnverified {
        count: usize,
        newest: BlobId,
    },
    UnreadableProvenance {
        count: usize,
    },
}

#[derive(Debug, Clone)]
pub struct LatestPick {
    pub entry: SnapshotEntry,
    pub mode: LatestMode,
    pub warnings: Vec<LatestWarning>,
}

fn manifest_ms(e: &SnapshotEntry) -> i64 {
    let t = e.snapshot.time.unwrap_or_default();
    t.seconds
        .saturating_mul(1000)
        .saturating_add(i64::from(t.nanos / 1_000_000))
}

/// The order of an unsigned snapshot: the store's clock, else the manifest's.
fn stored_key(e: &SnapshotEntry) -> (i64, BlobId) {
    (e.modified_ms.unwrap_or_else(|| manifest_ms(e)), e.id)
}

/// How many of `entries` there are and the id of the one with the largest key.
fn newest_of<'a>(
    entries: impl Iterator<Item = &'a SnapshotEntry>,
    key: impl Fn(&SnapshotEntry) -> (i64, BlobId),
) -> Option<(usize, BlobId)> {
    let mut count = 0;
    let mut best: Option<(i64, BlobId)> = None;
    for e in entries {
        count += 1;
        best = best.max(Some(key(e)));
    }
    best.map(|(_, id)| (count, id))
}

/// Picks `latest`. `expect_upgraded` is true for a caller that knows the
/// repository carried server signatures before; with no MAC-valid attestation
/// left it then refuses instead of falling back to the legacy order.
pub fn pick_latest(
    entries: Vec<SnapshotEntry>,
    provenance: &ProvenanceSet,
    now_ms: i64,
    expect_upgraded: bool,
) -> Result<LatestPick> {
    if entries.is_empty() {
        return Err(Error::InvalidInput(
            "the repository has no snapshots".into(),
        ));
    }
    if expect_upgraded && !provenance.attested_any() {
        return Err(Error::InvalidInput(ATTESTATIONS_MISSING.into()));
    }
    let limit = now_ms.saturating_add(FUTURE_TOLERANCE_MS);
    let mut warnings = Vec::new();

    let (mode, chosen) = if provenance.attested_any() {
        let key = |e: &SnapshotEntry| {
            let signed_at = provenance.get(&e.id).map_or(0, verified_at_ms);
            (e.modified_ms.unwrap_or(signed_at), e.id)
        };
        let chosen = entries
            .iter()
            .filter(|e| provenance.contains(&e.id))
            .max_by_key(|e| key(e))
            .ok_or_else(|| Error::InvalidInput(NO_VERIFIED_SNAPSHOT.into()))?;
        let bar = key(chosen);
        let newer = entries
            .iter()
            .filter(|e| !provenance.contains(&e.id) && stored_key(e) > bar);
        if let Some((count, newest)) = newest_of(newer, stored_key) {
            warnings.push(LatestWarning::NewerUnverified { count, newest });
        }
        (LatestMode::Strict, chosen)
    } else {
        warnings.push(LatestWarning::Legacy);
        let (kept, future): (Vec<&SnapshotEntry>, Vec<&SnapshotEntry>) =
            entries.iter().partition(|e| stored_key(e).0 <= limit);
        if let Some((count, newest)) = newest_of(future.into_iter(), stored_key) {
            warnings.push(LatestWarning::FutureStored { count, newest });
        }
        let chosen = kept
            .into_iter()
            .max_by_key(|e| stored_key(e))
            .ok_or_else(|| Error::InvalidInput(ALL_STORED_IN_FUTURE.into()))?;
        (LatestMode::Legacy, chosen)
    };

    let ahead = entries.iter().filter(|e| manifest_ms(e) > limit);
    if let Some((count, newest)) = newest_of(ahead, |e| (manifest_ms(e), e.id)) {
        warnings.push(LatestWarning::FutureTime { count, newest });
    }
    if provenance.unreadable() > 0 {
        warnings.push(LatestWarning::UnreadableProvenance {
            count: provenance.unreadable(),
        });
    }
    Ok(LatestPick {
        entry: chosen.clone(),
        mode,
        warnings,
    })
}
