//! Server attestations under `locks/attest-*`. An attestation names index
//! shards the server vouches for. It is framed as `frame || MAC || body` where
//! the MAC is keyed with the server key, which only a holder of the master key
//! can derive: an agent holding writer keys can write into `locks/` but cannot
//! forge one. This module never deletes anything.

use std::collections::{BTreeSet, HashSet};

use prost::Message;
use subtle::ConstantTimeEq;

use crate::crypto::{
    BlobId, Entropy, RepoId, attest_mac, open_provenance_body, seal_provenance_body,
};
use crate::error::{Error, Result, within_size};
use crate::fs::now;
use crate::keys::ReadKeys;
use crate::locks::{LOCKS_PREFIX, LockId, lock_id_hex, new_lock_id};
use crate::pb;
use crate::store::{BlobError, BlobStore, put_named};

pub const ATTEST_PREFIX: &str = "locks/attest-";
/// Largest attestation read from a listing.
pub const MAX_ATTEST_SIZE: u64 = 16 << 20;
pub const ATTEST_FRAME_V1: u8 = 1;
/// The repository upgrade completed.
pub const MARKER_LEGACY_SHARDS: u32 = 1;
/// A part of a repository upgrade.
pub const MARKER_UPGRADE_PART: u32 = 2;

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
