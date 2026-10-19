//! Objects under `locks/` that coordinate garbage collection with writers.
//! Names are random ids, not content hashes; bodies are sealed under the
//! index key, so only a holder of writer keys can produce or read them. This
//! module never deletes anything.

use std::collections::HashSet;
use std::time::Duration;

use bytes::Bytes;
use data_encoding::HEXLOWER;
use prost::Message;

use crate::crypto::{
    BlobId, Entropy, RepoId, open_gc_manifest_body, open_writer_marker_body, seal_gc_manifest_body,
    seal_writer_marker_body,
};
use crate::error::{Error, Result};
use crate::fs::now;
use crate::pb;
use crate::store::{BlobError, BlobStore, put_named};

pub const LOCKS_PREFIX: &str = "locks/";
pub const MIN_PAUSE: Duration = Duration::from_hours(24);
pub const DEFAULT_MAX_RUNTIME: Duration = Duration::from_hours(12);
pub const RUNTIME_MARGIN: Duration = Duration::from_hours(1);
pub const UNMARKED_WRITER_LIMIT: Duration = Duration::from_hours(23);

const GC_PREFIX: &str = "locks/gc-";
const WRITER_PREFIX: &str = "locks/writer-";
const DONE_SUFFIX: &str = ".done";
const VERSION: u32 = 1;

pub type LockId = [u8; 16];

pub fn new_lock_id(entropy: &mut Entropy) -> LockId {
    entropy.bytes::<16>()
}

pub fn lock_id_hex(id: &LockId) -> String {
    HEXLOWER.encode(id)
}

fn parse_lock_id(hex: &str) -> Option<LockId> {
    HEXLOWER.decode(hex.as_bytes()).ok()?.try_into().ok()
}

pub fn gc_manifest_key(id: &LockId) -> String {
    format!("{GC_PREFIX}{}", lock_id_hex(id))
}

pub fn writer_marker_key(id: &LockId) -> String {
    format!("{WRITER_PREFIX}{}", lock_id_hex(id))
}

pub fn writer_done_key(id: &LockId) -> String {
    format!("{}{DONE_SUFFIX}", writer_marker_key(id))
}

pub fn seal_gc_manifest(
    k_idx: &[u8; 32],
    repo_id: &RepoId,
    manifest: &pb::GcManifest,
    entropy: &mut Entropy,
) -> Result<Vec<u8>> {
    seal_gc_manifest_body(k_idx, repo_id, &manifest.encode_to_vec(), entropy)
}

pub fn open_gc_manifest(
    k_idx: &[u8; 32],
    repo_id: &RepoId,
    key: &str,
    bytes: &[u8],
) -> Result<pb::GcManifest> {
    let plain = open_gc_manifest_body(k_idx, repo_id, bytes)
        .map_err(|_| Error::corrupt(key, "authentication failed"))?;
    let manifest = pb::GcManifest::decode(plain.as_slice())
        .map_err(|e| Error::corrupt(key, format!("undecodable: {e}")))?;
    if manifest.version != VERSION {
        return Err(Error::corrupt(
            key,
            format!("unknown manifest version {}", manifest.version),
        ));
    }
    let named = key.strip_prefix(GC_PREFIX).and_then(parse_lock_id);
    let id: Option<LockId> = manifest.gc_id.as_slice().try_into().ok();
    if id.is_none() || id != named {
        return Err(Error::corrupt(key, "manifest id does not match its name"));
    }
    if manifest.candidate_packs.iter().any(|p| p.len() != 32) {
        return Err(Error::corrupt(key, "pack id must be 32 bytes"));
    }
    Ok(manifest)
}

pub fn seal_writer_marker(
    k_idx: &[u8; 32],
    repo_id: &RepoId,
    marker: &pb::WriterMarker,
    entropy: &mut Entropy,
) -> Result<Vec<u8>> {
    seal_writer_marker_body(k_idx, repo_id, &marker.encode_to_vec(), entropy)
}

pub fn open_writer_marker(
    k_idx: &[u8; 32],
    repo_id: &RepoId,
    key: &str,
    bytes: &[u8],
) -> Result<pb::WriterMarker> {
    let plain = open_writer_marker_body(k_idx, repo_id, bytes)
        .map_err(|_| Error::corrupt(key, "authentication failed"))?;
    let marker = pb::WriterMarker::decode(plain.as_slice())
        .map_err(|e| Error::corrupt(key, format!("undecodable: {e}")))?;
    if marker.version != VERSION {
        return Err(Error::corrupt(
            key,
            format!("unknown marker version {}", marker.version),
        ));
    }
    let named = key.strip_prefix(WRITER_PREFIX).and_then(parse_lock_id);
    let id: Option<LockId> = marker.writer_id.as_slice().try_into().ok();
    if id.is_none() || id != named {
        return Err(Error::corrupt(key, "marker id does not match its name"));
    }
    Ok(marker)
}

/// Publishes the candidate list under `locks/gc-<gc_id>`. An existing object
/// under the same id means a restarted publication and counts as success.
pub async fn publish_gc_manifest<S: BlobStore>(
    store: &S,
    k_idx: &[u8; 32],
    repo_id: &RepoId,
    gc_id: &LockId,
    candidate_packs: &[BlobId],
    entropy: &mut Entropy,
) -> Result<()> {
    let mut packs: Vec<Vec<u8>> = candidate_packs.iter().map(|p| p.to_vec()).collect();
    packs.sort();
    packs.dedup();
    let manifest = pb::GcManifest {
        version: VERSION,
        gc_id: gc_id.to_vec(),
        published: Some(now()),
        candidate_packs: packs,
    };
    let bytes = seal_gc_manifest(k_idx, repo_id, &manifest, entropy)?;
    put_named(store, &gc_manifest_key(gc_id), bytes).await
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcLock {
    pub key: String,
    pub gc_id: LockId,
    pub candidate_packs: Vec<BlobId>,
    pub modified_ms: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct GcLocks {
    pub manifests: Vec<GcLock>,
    /// The store refused LIST or GET on `locks/`; the manifests may be
    /// incomplete.
    pub denied: bool,
}

impl GcLocks {
    pub fn candidate_packs(&self) -> HashSet<BlobId> {
        self.manifests
            .iter()
            .flat_map(|m| m.candidate_packs.iter().copied())
            .collect()
    }
}

/// Reads every `locks/gc-*` manifest. One that does not open is an error:
/// ignoring it would let a writer rely on packs a collector may delete. A
/// refusal by the store is not: it is reported in [`GcLocks::denied`] and the
/// caller decides.
pub async fn read_gc_locks<S: BlobStore>(
    store: &S,
    k_idx: &[u8; 32],
    repo_id: &RepoId,
) -> Result<GcLocks> {
    let mut out = GcLocks::default();
    let listed = match store.list(LOCKS_PREFIX).await {
        Ok(listed) => listed,
        Err(BlobError::Denied { .. }) => {
            out.denied = true;
            return Ok(out);
        }
        Err(e) => return Err(e.into()),
    };
    for entry in listed {
        if !entry.key.starts_with(GC_PREFIX) {
            continue;
        }
        let bytes = match store.get(&entry.key).await {
            Ok(bytes) => bytes,
            Err(BlobError::NotFound) => continue,
            Err(BlobError::Denied { .. }) => {
                out.denied = true;
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let manifest = open_gc_manifest(k_idx, repo_id, &entry.key, &bytes)?;
        out.manifests.push(GcLock {
            gc_id: manifest
                .gc_id
                .as_slice()
                .try_into()
                .expect("checked on open"),
            candidate_packs: manifest
                .candidate_packs
                .iter()
                .map(|p| p.as_slice().try_into().expect("checked on open"))
                .collect(),
            key: entry.key,
            modified_ms: entry.modified_ms,
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq)]
pub struct WriterLock {
    pub id: LockId,
    pub key: String,
    pub modified_ms: Option<i64>,
    pub done: bool,
    /// `None` when the marker does not open; it still counts as open.
    pub marker: Option<pb::WriterMarker>,
}

/// Lists the writer markers with their completion state. Unlike
/// [`read_gc_locks`] a refusal by the store is an error: the caller is the
/// collector, which must not delete on a partial view.
pub async fn read_writer_locks<S: BlobStore>(
    store: &S,
    k_idx: &[u8; 32],
    repo_id: &RepoId,
) -> Result<Vec<WriterLock>> {
    let listed = store.list(LOCKS_PREFIX).await?;
    let done: HashSet<LockId> = listed
        .iter()
        .filter_map(|e| {
            e.key
                .strip_prefix(WRITER_PREFIX)?
                .strip_suffix(DONE_SUFFIX)
                .and_then(parse_lock_id)
        })
        .collect();
    let mut out = Vec::new();
    for entry in listed {
        let Some(id) = entry
            .key
            .strip_prefix(WRITER_PREFIX)
            .and_then(parse_lock_id)
        else {
            continue;
        };
        let marker = match store.get(&entry.key).await {
            Ok(bytes) => open_writer_marker(k_idx, repo_id, &entry.key, &bytes).ok(),
            Err(BlobError::NotFound) => continue,
            Err(e) => return Err(e.into()),
        };
        out.push(WriterLock {
            id,
            done: done.contains(&id),
            key: entry.key,
            modified_ms: entry.modified_ms,
            marker,
        });
    }
    Ok(out)
}

/// Announces a writer: until [`close_writer_marker`] a collector must not
/// delete anything.
pub async fn put_writer_marker<S: BlobStore>(
    store: &S,
    k_idx: &[u8; 32],
    repo_id: &RepoId,
    hostname: &[u8],
    entropy: &mut Entropy,
) -> Result<LockId> {
    let id = new_lock_id(entropy);
    let marker = pb::WriterMarker {
        version: VERSION,
        writer_id: id.to_vec(),
        hostname: hostname.to_vec(),
        started: Some(now()),
    };
    let bytes = seal_writer_marker(k_idx, repo_id, &marker, entropy)?;
    store
        .put_if_absent(&writer_marker_key(&id), Bytes::from(bytes))
        .await?;
    Ok(id)
}

pub async fn close_writer_marker<S: BlobStore>(store: &S, id: &LockId) -> Result<()> {
    put_named(store, &writer_done_key(id), Vec::new()).await
}
