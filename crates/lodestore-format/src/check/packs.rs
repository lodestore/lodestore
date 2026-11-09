//! The pack reading check: download packs, compare the file with its name
//! and verify every blob in it.

use std::collections::{HashMap, HashSet};

use crate::check::{Finding, FindingKind, Findings, is_data_error};
use crate::crypto::BlobId;
use crate::error::{Error, Result, within_size};
use crate::gc::mark::load_shards;
use crate::index::hex32;
use crate::keys::ReadKeys;
use crate::pack::{PACK_MAX_SIZE, pack_object_key, parse_entries, read_pack_header};
use crate::reader::open_chunk;
use crate::store::{BlobError, BlobStore, read_bounded};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackOutcome {
    pub pack_id: BlobId,
    /// Size of the downloaded file; 0 when the pack was not there.
    pub bytes: u64,
    /// Blobs that passed.
    pub blobs: u64,
    pub damaged: u64,
    pub missing: bool,
}

#[derive(Debug, Default)]
pub struct PacksReport {
    pub packs_read: u64,
    pub bytes_read: u64,
    pub blobs_verified: u64,
    pub damaged_blobs: u64,
    /// One entry per pack the run took on, in the order given.
    pub outcomes: Vec<PackOutcome>,
    pub findings: Vec<Finding>,
    pub findings_total: u64,
    /// Packs that could not be read for a reason that says nothing about
    /// their data, such as a refused or failed request. They have no outcome;
    /// the run goes on with the next pack, and stops after a few.
    pub read_errors: Vec<(String, String)>,
}

type EntryKey = (BlobId, u8, u32, u32, u32);

/// What the index shards say about one pack.
#[derive(Default)]
struct Indexed {
    sizes: HashSet<u64>,
    entries: HashSet<EntryKey>,
}

/// The shards' records of the `chosen` packs. A shard that does not open or
/// has bad entries is the structure check's to report.
async fn indexed_view<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    chosen: &[(BlobId, u64)],
) -> Result<HashMap<BlobId, Indexed>> {
    let wanted: HashSet<BlobId> = chosen.iter().map(|(id, _)| *id).collect();
    let mut view: HashMap<BlobId, Indexed> = HashMap::new();
    for (_, shard) in load_shards(store, keys).await?.shards {
        for p in &shard.packs {
            let Ok(pack_id) = BlobId::try_from(p.pack_id.as_slice()) else {
                continue;
            };
            if !wanted.contains(&pack_id) {
                continue;
            }
            let Ok(entries) = parse_entries(&p.entries) else {
                continue;
            };
            let slot = view.entry(pack_id).or_default();
            slot.sizes.insert(p.pack_size);
            slot.entries.extend(
                entries
                    .iter()
                    .map(|e| (e.blob_id, e.blob_type, e.offset, e.enc_len, e.ulen)),
            );
        }
    }
    Ok(view)
}

const MAX_READ_ERRORS: usize = 3;

fn reason(e: &Error) -> String {
    match e {
        Error::Corrupt { reason, .. } => reason.clone(),
        other => other.to_string(),
    }
}

/// How many of `packs`, in order, fit in `max_bytes`; at least one.
fn within_limit(packs: &[(BlobId, u64)], max_bytes: u64) -> usize {
    let mut total = 0u64;
    let mut taken = 0;
    for (_, size) in packs {
        total = total.saturating_add(*size);
        if taken > 0 && total > max_bytes {
            break;
        }
        taken += 1;
    }
    taken
}

/// Reads the `packs` in the order given, as many as `max_bytes` allows (one
/// at least), each with a single GET. `progress` gets the packs read, the
/// packs the run will read and the bytes read so far. Packs are read one
/// after another. A pack that fails to be read is left out and noted in
/// `read_errors`.
///
/// ponytail: sequential; read two to four packs at once if a run on a slow
/// bucket takes too long.
pub async fn check_packs<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    packs: &[(BlobId, u64)],
    max_bytes: u64,
    progress: &(dyn Fn(u64, u64, u64) + Sync),
) -> Result<PacksReport> {
    let chosen = &packs[..within_limit(packs, max_bytes)];
    let total = chosen.len() as u64;
    let mut report = PacksReport::default();
    let mut findings = Findings::default();
    let indexed = indexed_view(store, keys, chosen).await?;
    progress(0, total, 0);
    for (pack_id, size) in chosen {
        let outcome = match check_pack(
            store,
            keys,
            pack_id,
            *size,
            indexed.get(pack_id),
            &mut findings,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(e) => {
                report
                    .read_errors
                    .push((pack_object_key(pack_id), e.to_string()));
                if report.read_errors.len() >= MAX_READ_ERRORS {
                    break;
                }
                continue;
            }
        };
        if !outcome.missing {
            report.packs_read += 1;
        }
        report.bytes_read += outcome.bytes;
        report.blobs_verified += outcome.blobs;
        report.damaged_blobs += outcome.damaged;
        report.outcomes.push(outcome);
        progress(report.outcomes.len() as u64, total, report.bytes_read);
    }
    report.findings = findings.list;
    report.findings_total = findings.total;
    Ok(report)
}

async fn check_pack<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    pack_id: &BlobId,
    listed_size: u64,
    indexed: Option<&Indexed>,
    findings: &mut Findings,
) -> Result<PackOutcome> {
    let key = pack_object_key(pack_id);
    let mut outcome = PackOutcome {
        pack_id: *pack_id,
        bytes: 0,
        blobs: 0,
        damaged: 0,
        missing: false,
    };
    if let Err(e) = within_size(&key, listed_size, PACK_MAX_SIZE as u64, "pack") {
        findings.add(FindingKind::UnreadablePack, key, reason(&e));
        return Ok(outcome);
    }
    let bytes =
        match read_bounded(store, &key, Some(listed_size), PACK_MAX_SIZE as u64, "pack").await {
            Ok(bytes) => bytes,
            Err(Error::Store(BlobError::NotFound)) => {
                outcome.missing = true;
                findings.add(
                    FindingKind::MissingPack,
                    key,
                    format!("not in the bucket ({listed_size} bytes were listed)"),
                );
                return Ok(outcome);
            }
            Err(e @ Error::Corrupt { .. }) => {
                findings.add(FindingKind::UnreadablePack, key, reason(&e));
                return Ok(outcome);
            }
            Err(e) => return Err(e),
        };
    outcome.bytes = bytes.len() as u64;
    if blake3::hash(&bytes).as_bytes() != pack_id {
        findings.add(
            FindingKind::PackMismatch,
            key.clone(),
            "the file does not match its name",
        );
    }
    if let Some(ix) = indexed {
        for size in ix.sizes.iter().filter(|s| **s != outcome.bytes) {
            findings.add(
                FindingKind::PackMismatch,
                key.clone(),
                format!(
                    "the index records {size} bytes, the pack has {}",
                    outcome.bytes
                ),
            );
        }
    }
    let header = match read_pack_header(store, &key, outcome.bytes, keys).await {
        Ok(header) => header,
        Err(e) if is_data_error(&e) => {
            findings.add(FindingKind::UnreadablePack, key, reason(&e));
            return Ok(outcome);
        }
        Err(e) => return Err(e),
    };
    let pack_key = match header
        .prefix
        .open_key(keys.hpke_private_key(), keys.repo_id())
    {
        Ok(pack_key) => pack_key,
        Err(e) => {
            findings.add(FindingKind::UnreadablePack, key, reason(&e));
            return Ok(outcome);
        }
    };
    if let Some(ix) = indexed {
        let held: HashSet<EntryKey> = header
            .entries
            .iter()
            .map(|e| (e.blob_id, e.blob_type, e.offset, e.enc_len, e.ulen))
            .collect();
        let off = ix.entries.iter().filter(|e| !held.contains(*e)).count();
        if off > 0 {
            findings.add(
                FindingKind::PackMismatch,
                key.clone(),
                format!("{off} entries of the index are not where the pack header puts them"),
            );
        }
    }
    for e in &header.entries {
        let start = e.offset as usize;
        let frame = bytes.get(start..start + e.enc_len as usize);
        let opened = match frame {
            Some(frame) => open_chunk(
                &pack_key,
                keys.repo_id(),
                e.blob_type,
                &e.blob_id,
                frame,
                e.ulen,
                keys.k_id(),
            )
            .map(drop),
            None => Err(Error::corrupt("pack", "frame lies outside the file")),
        };
        match opened {
            Ok(()) => outcome.blobs += 1,
            Err(err) if is_data_error(&err) => {
                outcome.damaged += 1;
                findings.add(
                    FindingKind::DamagedBlob,
                    key.clone(),
                    format!("blob {}: {}", hex32(&e.blob_id), reason(&err)),
                );
            }
            Err(err) => return Err(err),
        }
    }
    Ok(outcome)
}
