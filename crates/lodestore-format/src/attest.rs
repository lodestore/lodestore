//! Server attestations under `locks/attest-*`. An attestation names index
//! shards the server vouches for. It is framed as `frame || MAC || body` where
//! the MAC is keyed with the server key, which only a holder of the master key
//! can derive: an agent holding writer keys can write into `locks/` but cannot
//! forge one. This module never deletes anything.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use prost::Message;
use subtle::ConstantTimeEq;

use crate::crypto::{
    BlobId, Entropy, RepoId, attest_mac, open_provenance_body, seal_provenance_body,
};
use crate::error::{Error, Result, within_size};
use crate::fs::now;
use crate::gc::sweep::sweep_object;
use crate::index::{INDEX_PREFIX, index_object_key, parse_hex32};
use crate::keys::ReadKeys;
use crate::locks::{LOCKS_PREFIX, LockId, lock_id_hex, new_lock_id};
use crate::pb;
use crate::provenance::supersedes;
use crate::snapshot::SNAPSHOT_PREFIX;
use crate::store::{BlobDelete, BlobError, BlobPurge, BlobStore, put_named};

pub const ATTEST_PREFIX: &str = "locks/attest-";
/// Largest attestation read from a listing.
pub const MAX_ATTEST_SIZE: u64 = 16 << 20;
pub const ATTEST_FRAME_V1: u8 = 1;
/// The repository upgrade completed.
pub const MARKER_LEGACY_SHARDS: u32 = 1;
/// A part of a repository upgrade.
pub const MARKER_UPGRADE_PART: u32 = 2;
/// Most shards one attestation names: at 34 bytes each it stays well under
/// [`MAX_ATTEST_SIZE`].
pub const MAX_ATTEST_SHARDS: usize = 262_144;

const MAC_LEN: usize = 32;

pub fn attest_key(id: &LockId) -> String {
    format!("{ATTEST_PREFIX}{}", lock_id_hex(id))
}

pub fn seal_attestation(
    k_srv: &[u8; 32],
    repo_id: &RepoId,
    attestation: &pb::Attestation,
) -> Vec<u8> {
    let body = attestation.encode_to_vec();
    let mac = attest_mac(k_srv, repo_id, ATTEST_FRAME_V1, &body);
    let mut out = Vec::with_capacity(1 + MAC_LEN + body.len());
    out.push(ATTEST_FRAME_V1);
    out.extend_from_slice(&mac);
    out.extend_from_slice(&body);
    out
}

/// Splits the frame and decodes the body. Does not look at the MAC.
fn parse<'a>(key: &str, bytes: &'a [u8]) -> Result<(&'a [u8; MAC_LEN], pb::Attestation, &'a [u8])> {
    let short = || Error::corrupt(key, "attestation is too short");
    let (frame, rest) = bytes.split_first().ok_or_else(short)?;
    let (mac, body) = rest.split_first_chunk::<MAC_LEN>().ok_or_else(short)?;
    if *frame != ATTEST_FRAME_V1 {
        return Err(Error::corrupt(key, "unknown attestation frame"));
    }
    let attestation = pb::Attestation::decode(body)
        .map_err(|e| Error::corrupt(key, format!("undecodable: {e}")))?;
    let id: Option<LockId> = attestation.attest_id.as_slice().try_into().ok();
    if id.map(|id| attest_key(&id)).as_deref() != Some(key) {
        return Err(Error::corrupt(
            key,
            "attestation id does not match its name",
        ));
    }
    if attestation.shard_ids.iter().any(|s| s.len() != 32) {
        return Err(Error::corrupt(key, "shard id must be 32 bytes"));
    }
    Ok((mac, attestation, body))
}

/// Opens an attestation: the MAC must verify under `k_srv`.
pub fn open_attestation(
    k_srv: &[u8; 32],
    repo_id: &RepoId,
    key: &str,
    bytes: &[u8],
) -> Result<pb::Attestation> {
    let (mac, attestation, body) = parse(key, bytes)?;
    let expected = attest_mac(k_srv, repo_id, ATTEST_FRAME_V1, body);
    if !bool::from(expected.ct_eq(mac)) {
        return Err(Error::corrupt(key, "authentication failed"));
    }
    Ok(attestation)
}

/// Decodes an attestation without checking its MAC. The result is untrusted:
/// it may only feed an agent-side deduplication filter, never a decision of
/// the collector, the server or a reader.
pub fn peek_attestation(key: &str, bytes: &[u8]) -> Result<pb::Attestation> {
    parse(key, bytes).map(|(_, attestation, _)| attestation)
}

/// Writes a new attestation under a random `locks/attest-*` name and returns
/// its key.
pub async fn put_attestation<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    shard_ids: &[BlobId],
    markers: &[u32],
    sealed_provenance: Vec<Vec<u8>>,
    entropy: &mut Entropy,
) -> Result<String> {
    let mut shards: Vec<Vec<u8>> = shard_ids.iter().map(|s| s.to_vec()).collect();
    shards.sort();
    shards.dedup();
    let mut markers = markers.to_vec();
    markers.sort_unstable();
    markers.dedup();
    let id = new_lock_id(entropy);
    let attestation = pb::Attestation {
        attest_id: id.to_vec(),
        created: Some(now()),
        shard_ids: shards,
        markers,
        sealed_provenance,
    };
    let bytes = seal_attestation(keys.k_srv(), keys.repo_id(), &attestation);
    if bytes.len() as u64 > MAX_ATTEST_SIZE {
        return Err(Error::InvalidInput(format!(
            "attestation is {} bytes, more than {MAX_ATTEST_SIZE}",
            bytes.len()
        )));
    }
    let key = attest_key(&id);
    put_named(store, &key, bytes).await?;
    Ok(key)
}

/// Everything the MAC-verified attestations of a repository say.
#[derive(Debug, Clone, Default)]
pub struct AttestedSet {
    pub shards: HashSet<BlobId>,
    pub markers: BTreeSet<u32>,
    pub sealed_provenance: Vec<Vec<u8>>,
    pub objects: Vec<String>,
    /// Verified objects whose markers are exactly `[MARKER_UPGRADE_PART]`.
    pub upgrade_parts: usize,
    /// `locks/attest-*` objects that did not verify, with the reason. They are
    /// ignored; a planted one must not stop the server.
    pub rejected: Vec<(String, String)>,
    /// An object vanished between the listing and its read.
    pub incomplete: bool,
}

fn shard_ids(attestation: &pb::Attestation) -> impl Iterator<Item = BlobId> + '_ {
    attestation
        .shard_ids
        .iter()
        .filter_map(|s| s.as_slice().try_into().ok())
}

/// Reads every `locks/attest-*` object and keeps those whose MAC verifies
/// under the server key.
pub async fn read_attestations<S: BlobStore>(store: &S, keys: &ReadKeys) -> Result<AttestedSet> {
    let mut out = AttestedSet::default();
    for entry in store.list(LOCKS_PREFIX).await? {
        if !entry.key.starts_with(ATTEST_PREFIX) {
            continue;
        }
        if let Err(e) = within_size(&entry.key, entry.size, MAX_ATTEST_SIZE, "attestation") {
            out.rejected.push((entry.key, e.to_string()));
            continue;
        }
        let bytes = match store.get(&entry.key).await {
            Ok(bytes) => bytes,
            Err(BlobError::NotFound) => {
                out.incomplete = true;
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        match open_attestation(keys.k_srv(), keys.repo_id(), &entry.key, &bytes) {
            Ok(attestation) => {
                out.shards.extend(shard_ids(&attestation));
                out.markers.extend(attestation.markers.iter().copied());
                if attestation.markers == [MARKER_UPGRADE_PART] {
                    out.upgrade_parts += 1;
                }
                out.sealed_provenance.extend(attestation.sealed_provenance);
                out.objects.push(entry.key);
            }
            Err(e @ (Error::Corrupt { .. } | Error::UnsupportedVersion { .. })) => {
                out.rejected.push((entry.key, e.to_string()));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// What the attestations claim, with no MAC check. Deliberately a different
/// type from [`AttestedSet`]: it is only fit for an agent-side deduplication
/// filter, never for a collector, a server check or a reader.
#[derive(Debug, Clone, Default)]
pub struct UnverifiedAttestations {
    pub shards: HashSet<BlobId>,
    pub markers: BTreeSet<u32>,
}

/// Reads the `locks/attest-*` objects without a server key. A store that
/// refuses the listing, a vanished object and a malformed one all count as
/// "nothing attested".
pub async fn peek_attestations<S: BlobStore>(store: &S) -> Result<UnverifiedAttestations> {
    let mut out = UnverifiedAttestations::default();
    let listed = match store.list(LOCKS_PREFIX).await {
        Ok(listed) => listed,
        Err(BlobError::Denied { .. }) => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    for entry in listed {
        if !entry.key.starts_with(ATTEST_PREFIX) || entry.size > MAX_ATTEST_SIZE {
            continue;
        }
        let bytes = match store.get(&entry.key).await {
            Ok(bytes) => bytes,
            Err(BlobError::NotFound | BlobError::Denied { .. }) => continue,
            Err(e) => return Err(e.into()),
        };
        if let Ok(attestation) = peek_attestation(&entry.key, &bytes) {
            out.shards.extend(shard_ids(&attestation));
            out.markers.extend(attestation.markers.iter().copied());
        }
    }
    Ok(out)
}

/// Seals a provenance record for `Attestation.sealed_provenance`.
pub fn seal_provenance(
    keys: &ReadKeys,
    record: &pb::ProvenanceRecord,
    entropy: &mut Entropy,
) -> Result<Vec<u8>> {
    seal_provenance_body(
        keys.k_srv(),
        keys.repo_id(),
        &record.encode_to_vec(),
        entropy,
    )
}

pub fn open_provenance(keys: &ReadKeys, sealed: &[u8]) -> Result<pb::ProvenanceRecord> {
    let plain = open_provenance_body(keys.k_srv(), keys.repo_id(), sealed)
        .map_err(|_| Error::corrupt("provenance", "authentication failed"))?;
    let record = pb::ProvenanceRecord::decode(plain.as_slice())
        .map_err(|e| Error::corrupt("provenance", format!("undecodable: {e}")))?;
    if record.snapshot_id.len() != 32 {
        return Err(Error::corrupt("provenance", "snapshot id must be 32 bytes"));
    }
    Ok(record)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpgradeOutcome {
    pub already: bool,
    /// Shards signed by this call.
    pub attested: usize,
}

/// Shard ids named in the `index/` listing, from the names alone.
async fn listed_shards<S: BlobStore>(store: &S) -> Result<BTreeSet<BlobId>> {
    Ok(store
        .list(INDEX_PREFIX)
        .await?
        .into_iter()
        .filter_map(|entry| {
            let id = parse_hex32(entry.key.strip_prefix(INDEX_PREFIX)?)?;
            (index_object_key(&id) == entry.key).then_some(id)
        })
        .collect())
}

/// Signs the shards of a repository written before attestations existed, once.
/// The state lives in the bucket: parts carry `[MARKER_UPGRADE_PART]`, and the
/// last object, written only after every part, carries `MARKER_LEGACY_SHARDS`.
/// Server only: it needs the master key.
pub async fn ensure_upgraded<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    entropy: &mut Entropy,
) -> Result<UpgradeOutcome> {
    upgrade(store, keys, entropy, MAX_ATTEST_SHARDS).await
}

async fn upgrade<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    entropy: &mut Entropy,
    chunk: usize,
) -> Result<UpgradeOutcome> {
    let set = read_attestations(store, keys).await?;
    if set.incomplete {
        return Err(Error::InvalidInput(
            "attestations changed while reading; try again".into(),
        ));
    }
    if set.markers.contains(&MARKER_LEGACY_SHARDS) {
        return Ok(UpgradeOutcome {
            already: true,
            attested: 0,
        });
    }
    if set.objects.len() > set.upgrade_parts {
        return Err(Error::InvalidInput(
            "the repository has server attestations but no upgrade marker; \
             refusing to sign its shards again"
                .into(),
        ));
    }
    let todo: Vec<BlobId> = listed_shards(store)
        .await?
        .into_iter()
        .filter(|id| !set.shards.contains(id))
        .collect();
    for part in todo.chunks(chunk) {
        put_attestation(
            store,
            keys,
            part,
            &[MARKER_UPGRADE_PART],
            Vec::new(),
            entropy,
        )
        .await?;
    }
    put_attestation(
        store,
        keys,
        &[],
        &[MARKER_LEGACY_SHARDS],
        Vec::new(),
        entropy,
    )
    .await?;
    Ok(UpgradeOutcome {
        already: false,
        attested: todo.len(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsolidateOutcome {
    /// Verified attestation objects read.
    pub read: usize,
    pub written: Vec<String>,
    pub deleted: usize,
    /// Sealed provenance records not carried over: those that do not open
    /// and those of snapshots no longer in `snapshots/`.
    pub provenance_dropped: usize,
}

/// Most sealed provenance records one attestation carries.
pub const MAX_ATTEST_PROVENANCE: usize = 32_768;

/// Most sealed provenance bytes one attestation carries; a part also holds
/// up to [`MAX_ATTEST_SHARDS`] shards, so the two stay under [`MAX_ATTEST_SIZE`].
const MAX_ATTEST_PROVENANCE_BYTES: usize = 6 << 20;

/// The sealed records of snapshots that are still listed, one per snapshot,
/// and how many records were left out as unreadable or orphaned.
async fn live_provenance<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    sealed: &[Vec<u8>],
) -> Result<(Vec<Vec<u8>>, usize)> {
    let live: HashSet<BlobId> = store
        .list(SNAPSHOT_PREFIX)
        .await?
        .into_iter()
        .filter_map(|e| parse_hex32(e.key.strip_prefix(SNAPSHOT_PREFIX)?))
        .collect();
    let mut kept: BTreeMap<BlobId, (pb::ProvenanceRecord, &Vec<u8>)> = BTreeMap::new();
    let mut dropped = 0;
    for bytes in sealed {
        let Ok(record) = open_provenance(keys, bytes) else {
            dropped += 1;
            continue;
        };
        let Ok(id) = BlobId::try_from(record.snapshot_id.as_slice()) else {
            dropped += 1;
            continue;
        };
        if !live.contains(&id) {
            dropped += 1;
            continue;
        }
        match kept.get(&id) {
            Some((old, _)) if !supersedes(&record, old) => {}
            _ => {
                kept.insert(id, (record, bytes));
            }
        }
    }
    Ok((
        kept.into_values().map(|(_, b)| b.clone()).collect(),
        dropped,
    ))
}

/// Cuts `records` into parts of at most [`MAX_ATTEST_PROVENANCE`] records and
/// [`MAX_ATTEST_PROVENANCE_BYTES`] bytes.
fn provenance_parts(records: Vec<Vec<u8>>, max_records: usize) -> Vec<Vec<Vec<u8>>> {
    let mut parts: Vec<Vec<Vec<u8>>> = Vec::new();
    let mut bytes = 0usize;
    for record in records {
        let size = record.len().saturating_add(8);
        match parts.last_mut() {
            Some(part)
                if part.len() < max_records
                    && bytes.saturating_add(size) <= MAX_ATTEST_PROVENANCE_BYTES =>
            {
                bytes += size;
                part.push(record);
            }
            _ => {
                bytes = size;
                parts.push(vec![record]);
            }
        }
    }
    parts
}

/// Replaces the attestation objects with a summary: the shards still in
/// `index/` and not in `revoked`, every marker in every part, and the sealed
/// provenance of the snapshots still in `snapshots/`, one record per snapshot,
/// cut into parts that fit the size limit. The summary is written before
/// anything is deleted, and only objects this call read are deleted. Does
/// nothing until the upgrade has completed. Server only.
pub async fn consolidate_attestations<S: BlobStore + BlobDelete + BlobPurge>(
    store: &S,
    keys: &ReadKeys,
    dry_run: bool,
    revoked: &HashSet<BlobId>,
    entropy: &mut Entropy,
) -> Result<ConsolidateOutcome> {
    consolidate(
        store,
        keys,
        dry_run,
        revoked,
        entropy,
        MAX_ATTEST_PROVENANCE,
    )
    .await
}

async fn consolidate<S: BlobStore + BlobDelete + BlobPurge>(
    store: &S,
    keys: &ReadKeys,
    dry_run: bool,
    revoked: &HashSet<BlobId>,
    entropy: &mut Entropy,
    max_records: usize,
) -> Result<ConsolidateOutcome> {
    let set = read_attestations(store, keys).await?;
    if set.incomplete {
        return Err(Error::InvalidInput(
            "attestations changed while reading; try again".into(),
        ));
    }
    let mut out = ConsolidateOutcome {
        read: set.objects.len(),
        written: Vec::new(),
        deleted: 0,
        provenance_dropped: 0,
    };
    let drops_revoked = revoked.iter().any(|id| set.shards.contains(id));
    if (set.objects.len() <= 1 && !drops_revoked)
        || !set.markers.contains(&MARKER_LEGACY_SHARDS)
        || dry_run
    {
        return Ok(out);
    }
    let live = listed_shards(store).await?;
    let shards: Vec<BlobId> = live
        .into_iter()
        .filter(|id| set.shards.contains(id) && !revoked.contains(id))
        .collect();
    // Listed after the attestations were read: a record is written only after
    // its snapshot object, so no record of a snapshot that exists is missed.
    let (records, dropped) = live_provenance(store, keys, &set.sealed_provenance).await?;
    out.provenance_dropped = dropped;
    let markers: Vec<u32> = set.markers.iter().copied().collect();
    let shard_parts: Vec<&[BlobId]> = shards.chunks(MAX_ATTEST_SHARDS).collect();
    let mut prov_parts = provenance_parts(records, max_records).into_iter();
    let count = shard_parts.len().max(prov_parts.len()).max(1);
    for i in 0..count {
        let key = put_attestation(
            store,
            keys,
            shard_parts.get(i).copied().unwrap_or(&[]),
            &markers,
            prov_parts.next().unwrap_or_default(),
            entropy,
        )
        .await?;
        out.written.push(key);
    }
    let mut failed = Vec::new();
    for key in &set.objects {
        let swept = sweep_object(store, key, 0).await;
        out.deleted += usize::from(swept.failed.is_empty());
        failed.extend(swept.failed);
    }
    if let Some((key, reason)) = failed.into_iter().next() {
        return Err(Error::InvalidInput(format!(
            "could not delete {key}: {reason}"
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use rand_chacha::ChaCha20Rng;
    use rand_chacha::rand_core::SeedableRng;

    use super::*;
    use crate::index::hex32;
    use crate::keys::MasterKey;
    use crate::store::LocalFsStore;

    #[test]
    fn attest_provenance_parts_respect_both_limits() {
        let tiny = vec![vec![0u8; 1]; MAX_ATTEST_PROVENANCE + 1];
        let parts = provenance_parts(tiny, MAX_ATTEST_PROVENANCE);
        assert_eq!(
            parts.iter().map(Vec::len).collect::<Vec<_>>(),
            [MAX_ATTEST_PROVENANCE, 1]
        );

        let big = vec![vec![0u8; MAX_ATTEST_PROVENANCE_BYTES / 2]; 3];
        let parts = provenance_parts(big, MAX_ATTEST_PROVENANCE);
        assert_eq!(parts.iter().map(Vec::len).collect::<Vec<_>>(), [1, 1, 1]);
        assert!(provenance_parts(Vec::new(), 3).is_empty());
    }

    #[tokio::test]
    async fn attest_consolidate_chunks_provenance() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalFsStore::new(tmp.path().join("repo"));
        let keys = ReadKeys::derive(MasterKey::from_bytes([1u8; 32]), [2u8; 16]);
        let mut entropy = Entropy::from_rng(ChaCha20Rng::from_seed([7u8; 32]));
        let marker = [MARKER_LEGACY_SHARDS];
        put_attestation(&store, &keys, &[], &marker, Vec::new(), &mut entropy)
            .await
            .unwrap();
        let mut sealed = Vec::new();
        for n in 1u8..=7 {
            let id = [n; 32];
            let name = format!("{SNAPSHOT_PREFIX}{}", hex32(&id));
            store
                .put_if_absent(&name, bytes::Bytes::from_static(b"x"))
                .await
                .unwrap();
            let record = pb::ProvenanceRecord {
                snapshot_id: id.to_vec(),
                host_id: "h".into(),
                job_id: "j".into(),
                ..Default::default()
            };
            sealed.push(seal_provenance(&keys, &record, &mut entropy).unwrap());
        }
        put_attestation(&store, &keys, &[], &[], sealed, &mut entropy)
            .await
            .unwrap();

        let out = consolidate(&store, &keys, false, &HashSet::new(), &mut entropy, 3)
            .await
            .unwrap();
        assert_eq!((out.written.len(), out.provenance_dropped), (3, 0));
        let mut sizes = Vec::new();
        for entry in store.list(LOCKS_PREFIX).await.unwrap() {
            assert!(entry.size <= MAX_ATTEST_SIZE);
            let bytes = store.get(&entry.key).await.unwrap();
            let a = open_attestation(keys.k_srv(), keys.repo_id(), &entry.key, &bytes).unwrap();
            assert_eq!(a.markers, marker);
            sizes.push(a.sealed_provenance.len());
        }
        sizes.sort_unstable();
        assert_eq!(sizes, [1, 3, 3]);
    }

    #[tokio::test]
    async fn attest_upgrade_chunks_large_repositories() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalFsStore::new(tmp.path().join("repo"));
        let keys = ReadKeys::derive(MasterKey::from_bytes([1u8; 32]), [2u8; 16]);
        let mut entropy = Entropy::from_rng(ChaCha20Rng::from_seed([7u8; 32]));
        let ids: BTreeSet<BlobId> = (1u8..=5).map(|i| [i; 32]).collect();
        for id in &ids {
            store
                .put_if_absent(&index_object_key(id), bytes::Bytes::from_static(b"x"))
                .await
                .unwrap();
        }
        let out = upgrade(&store, &keys, &mut entropy, 2).await.unwrap();
        assert_eq!(out.attested, 5);
        let mut parts = Vec::new();
        let mut done = Vec::new();
        for entry in store.list(LOCKS_PREFIX).await.unwrap() {
            let bytes = store.get(&entry.key).await.unwrap();
            let a = open_attestation(keys.k_srv(), keys.repo_id(), &entry.key, &bytes).unwrap();
            if a.markers == [MARKER_UPGRADE_PART] {
                assert!(a.shard_ids.len() <= 2);
                parts.push(a.shard_ids);
            } else {
                assert_eq!(a.markers, [MARKER_LEGACY_SHARDS]);
                assert!(a.shard_ids.is_empty());
                done.push(a);
            }
        }
        assert_eq!((parts.len(), done.len()), (3, 1));
        let union: BTreeSet<String> = parts
            .iter()
            .flatten()
            .map(|s| hex32(s.as_slice().try_into().unwrap()))
            .collect();
        assert_eq!(union, ids.iter().map(hex32).collect());
    }
}
