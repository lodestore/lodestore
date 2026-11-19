//! Which snapshots the server or the password holder has signed, and which
//! snapshot is `latest`. A snapshot is verified when a MAC-valid attestation
//! holds a sealed provenance record that opens and names it. The manifest is
//! written by the host being backed up, so its time and names never decide
//! the order of a verified repository.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::attest::{AttestedSet, open_provenance};
use crate::crypto::BlobId;
use crate::error::{Error, Result};
use crate::keys::ReadKeys;
use crate::pb;
use crate::snapshot::SnapshotEntry;
use crate::source::{self, SourceKey};

pub const FUTURE_TOLERANCE_MS: i64 = 600_000;

pub const NO_VERIFIED_SNAPSHOT: &str = "No snapshot in this repository is signed by the server or by the password holder. Name one by id; `lode snapshots` lists them.";
pub const ATTESTATIONS_MISSING: &str = "This repository was upgraded for server signatures, but no signature can be read now; they may have been deleted. Name a snapshot by id; `lode snapshots` lists them.";
pub const ALL_STORED_IN_FUTURE: &str = "Every snapshot in this repository was stored with a time in the future. Name one by id; `lode snapshots` lists them.";
pub const ALL_SIGNED_STORED_IN_FUTURE: &str = "Every signed snapshot in this repository was stored with a time in the future. Name one by id; `lode snapshots` lists them.";

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

/// The record the password holder signs a snapshot with: no host and no job.
pub fn owner_record(
    snapshot_id: &BlobId,
    hostname: &[u8],
    source: Option<pb::Source>,
    at: SystemTime,
) -> pb::ProvenanceRecord {
    let at = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    pb::ProvenanceRecord {
        snapshot_id: snapshot_id.to_vec(),
        host_id: String::new(),
        hostname: String::from_utf8_lossy(hostname).into_owned(),
        job_id: String::new(),
        verified_at: Some(pb::Timestamp {
            seconds: i64::try_from(at.as_secs()).unwrap_or(i64::MAX),
            nanos: at.subsec_nanos(),
        }),
        source,
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

    /// Some signed record, of any source, carries this `host_id`.
    pub fn names_host(&self, host: &str) -> bool {
        self.records.values().any(|r| r.host_id == host)
    }

    /// The source of a snapshot, from its signed record only; the manifest's
    /// own claim never counts. No record, or a record without the field, is
    /// `files`. A record whose source this build cannot read does not open
    /// (`open_provenance`), so its snapshot counts as unsigned.
    pub fn source_of(&self, id: &BlobId) -> SourceKey {
        self.records
            .get(id)
            .and_then(|r| source::decode(r.source.as_ref()).ok())
            .map_or_else(SourceKey::files, |(key, _)| key)
    }

    /// What a person calls the instance of a signed snapshot, if the record
    /// says enough.
    pub fn label_of(&self, id: &BlobId) -> Option<String> {
        let (key, meta) = source::decode(self.records.get(id)?.source.as_ref()).ok()?;
        source::label(&key, &meta)
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

    /// The order of a signed snapshot: the store's clock, else the time its
    /// record was signed. The manifest's own time never counts.
    pub fn stored_order(&self, e: &SnapshotEntry) -> (i64, BlobId) {
        let signed_at = self.records.get(&e.id).map_or(0, verified_at_ms);
        (e.modified_ms.unwrap_or(signed_at), e.id)
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
    /// `--host` matched no host id and was taken as the name of one host;
    /// `current` is the name on its newest record. Cut to 64 characters.
    HostByName {
        name: String,
        host_id: String,
        current: String,
    },
    /// `--host` matched a host id and is also the name of other hosts with
    /// snapshots of the instance; the id won. `hosts` lists those hosts.
    HostIdAlsoName {
        host_id: String,
        hosts: String,
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

/// The snapshots `latest` chooses among.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LatestScope {
    /// Snapshots of files only.
    Files,
    /// The snapshots of one source instance. Refused when their signed
    /// records name more than one host: clones and copies share a key, so the
    /// caller names the host.
    Instance(SourceKey),
    /// The snapshots of one source instance that one host made. The host is
    /// named by its id. A string in the form of a host id (a UUID) is only
    /// ever an id; any other string is looked up as a name when no signed
    /// record carries it as an id, and must name exactly one host.
    InstanceOnHost(SourceKey, String),
}

impl LatestScope {
    /// A WAL batch is not a point in time and is never `latest`.
    fn admits(&self, source: &SourceKey) -> bool {
        match self {
            LatestScope::Files => source.is_files(),
            LatestScope::Instance(key) | LatestScope::InstanceOnHost(key, _) => {
                source == key && !key.kind().is_wal_batch()
            }
        }
    }
}

/// Host names and ids come from agents; they are shown cut short and escaped.
fn clip(text: &str) -> String {
    text.chars().take(64).collect()
}

type Hosts = BTreeMap<String, ((i64, BlobId), String)>;

/// The host a record stands for: its `host_id`, or for an owner record,
/// which has none, its hostname.
fn host_key(record: &pb::ProvenanceRecord) -> &str {
    if record.host_id.is_empty() {
        &record.hostname
    } else {
        &record.host_id
    }
}

/// Each host of `entries` ([`host_key`]) with the hostname of its newest record.
fn hosts_of(provenance: &ProvenanceSet, entries: &[&SnapshotEntry]) -> Hosts {
    let mut hosts = Hosts::new();
    for e in entries {
        let Some(record) = provenance.get(&e.id) else {
            continue;
        };
        let order = provenance.stored_order(e);
        let newest = (order, record.hostname.clone());
        hosts
            .entry(host_key(record).to_owned())
            .and_modify(|seen| *seen = seen.clone().max(newest.clone()))
            .or_insert(newest);
    }
    hosts
}

/// The canonical text form of a UUID, in either case. Host ids have it, so
/// such a string is never taken as a host name.
fn is_host_id(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

fn age(now_ms: i64, at_ms: i64) -> String {
    let s = now_ms.saturating_sub(at_ms) / 1000;
    match s {
        ..0 => "stored in the future".into(),
        0..120 => format!("{s}s ago"),
        120..3600 => format!("{}m ago", s / 60),
        3600..172_800 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}

/// `"hostname" ("host_id") newest <snapshot> <age>` per host, newest first,
/// at most five shown.
fn host_list<'a>(
    hosts: impl Iterator<Item = (&'a String, &'a ((i64, BlobId), String))>,
    now_ms: i64,
) -> String {
    const SHOWN: usize = 5;
    let mut items: Vec<(&(i64, BlobId), String, String)> = hosts
        .map(|(id, (order, name))| (order, clip(name), clip(id)))
        .collect();
    items.sort_by(|a, b| b.0.cmp(a.0).then_with(|| (&a.1, &a.2).cmp(&(&b.1, &b.2))));
    let mut out: Vec<String> = items
        .iter()
        .take(SHOWN)
        .map(|((at, snap), name, id)| {
            let short = crate::index::hex32(snap);
            let short = short.get(..8).unwrap_or_default();
            format!("{name:?} ({id:?}) newest {short} {}", age(now_ms, *at))
        })
        .collect();
    if items.len() > SHOWN {
        out.push(format!("and {} more", items.len() - SHOWN));
    }
    out.join(", ")
}

/// Picks `latest`. Only files snapshots count; another source needs
/// [`pick_latest_in`].
pub fn pick_latest(
    entries: Vec<SnapshotEntry>,
    provenance: &ProvenanceSet,
    now_ms: i64,
    expect_upgraded: bool,
) -> Result<LatestPick> {
    pick_latest_in(
        entries,
        provenance,
        now_ms,
        expect_upgraded,
        &LatestScope::Files,
    )
}

/// Picks `latest` inside `scope`. `expect_upgraded` is true for a caller that
/// knows the repository carried server signatures before; with no MAC-valid
/// attestation left it then refuses instead of falling back to the legacy
/// order. Without signatures nothing names a source, so an instance finds
/// nothing and `files` skips the snapshots whose manifest claims another one.
pub fn pick_latest_in(
    entries: Vec<SnapshotEntry>,
    provenance: &ProvenanceSet,
    now_ms: i64,
    expect_upgraded: bool,
    scope: &LatestScope,
) -> Result<LatestPick> {
    if entries.is_empty() {
        return Err(Error::InvalidInput(
            "the repository has no snapshots".into(),
        ));
    }
    if expect_upgraded && !provenance.attested_any() {
        return Err(Error::InvalidInput(ATTESTATIONS_MISSING.into()));
    }
    let entries = if provenance.attested_any() {
        entries
    } else {
        if !matches!(scope, LatestScope::Files) {
            return Err(Error::InvalidInput(NO_VERIFIED_SNAPSHOT.into()));
        }
        let claims = |e: &SnapshotEntry| {
            e.snapshot
                .source
                .as_ref()
                .is_some_and(|s| !s.key.is_empty())
        };
        let entries: Vec<_> = entries.into_iter().filter(|e| !claims(e)).collect();
        if entries.is_empty() {
            return Err(Error::InvalidInput(
                "the repository has no snapshots".into(),
            ));
        }
        entries
    };
    let limit = now_ms.saturating_add(FUTURE_TOLERANCE_MS);
    let mut warnings = Vec::new();

    let (mode, chosen) = if provenance.attested_any() {
        let key = |e: &SnapshotEntry| provenance.stored_order(e);
        let mut admitted: Vec<&SnapshotEntry> = entries
            .iter()
            .filter(|e| provenance.contains(&e.id) && scope.admits(&provenance.source_of(&e.id)))
            .collect();
        let host_of = |e: &SnapshotEntry| provenance.get(&e.id).map(host_key);
        match scope {
            LatestScope::Files => {}
            LatestScope::Instance(source) => {
                let hosts = hosts_of(provenance, &admitted);
                if hosts.len() > 1 {
                    return Err(Error::InvalidInput(format!(
                        "the snapshots of {} come from more than one host: {}; add --host with a host id to choose one",
                        source.as_str(),
                        host_list(hosts.iter(), now_ms)
                    )));
                }
            }
            LatestScope::InstanceOnHost(source, host) => {
                let hosts = hosts_of(provenance, &admitted);
                let id_form = is_host_id(host);
                let named: BTreeSet<&str> = admitted
                    .iter()
                    .filter_map(|e| provenance.get(&e.id))
                    .filter(|r| r.hostname == *host && host_key(r) != host)
                    .map(host_key)
                    .collect();
                let others = || {
                    host_list(
                        hosts.iter().filter(|(id, _)| named.contains(id.as_str())),
                        now_ms,
                    )
                };
                let chosen = if id_form || provenance.names_host(host) || hosts.contains_key(host) {
                    if !named.is_empty() {
                        warnings.push(LatestWarning::HostIdAlsoName {
                            host_id: clip(host),
                            hosts: others(),
                        });
                    }
                    Some(host.clone())
                } else {
                    if named.len() > 1 {
                        return Err(Error::InvalidInput(format!(
                            "{:?} names more than one host with snapshots of {}: {}; add --host with the host id",
                            clip(host),
                            source.as_str(),
                            others()
                        )));
                    }
                    named.first().map(|id| {
                        let current = hosts.get(*id).map_or("", |(_, name)| name.as_str());
                        warnings.push(LatestWarning::HostByName {
                            name: clip(host),
                            host_id: clip(id),
                            current: clip(current),
                        });
                        (*id).to_owned()
                    })
                };
                admitted.retain(|e| chosen.is_some() && host_of(e) == chosen.as_deref());
                if admitted.is_empty() {
                    let with = if hosts.is_empty() {
                        "none".to_owned()
                    } else {
                        host_list(hosts.iter(), now_ms)
                    };
                    let only_id = if id_form {
                        " (a host id is never taken as a host name)"
                    } else {
                        ""
                    };
                    return Err(Error::InvalidInput(format!(
                        "No signed snapshot of {} comes from host {:?}{only_id}. Hosts with signed snapshots of it: {with}",
                        source.as_str(),
                        clip(host),
                    )));
                }
            }
        }
        let (kept, future): (Vec<&SnapshotEntry>, Vec<&SnapshotEntry>) =
            admitted.into_iter().partition(|e| key(e).0 <= limit);
        if kept.is_empty() && !future.is_empty() {
            return Err(Error::InvalidInput(ALL_SIGNED_STORED_IN_FUTURE.into()));
        }
        if let Some((count, newest)) = newest_of(future.into_iter(), key) {
            warnings.push(LatestWarning::FutureStored { count, newest });
        }
        let chosen = kept
            .into_iter()
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

#[cfg(test)]
mod tests {
    use super::age;

    #[test]
    fn age_names_each_range() {
        let now = 1_000_000_000;
        assert_eq!(age(now, now + 1000), "stored in the future");
        assert_eq!(age(now, now), "0s ago");
        assert_eq!(age(now, now - 119_000), "119s ago");
        assert_eq!(age(now, now - 120_000), "2m ago");
        assert_eq!(age(now, now - 3_599_000), "59m ago");
        assert_eq!(age(now, now - 3_600_000), "1h ago");
        assert_eq!(age(now, now - 172_800_000), "2d ago");
    }
}
