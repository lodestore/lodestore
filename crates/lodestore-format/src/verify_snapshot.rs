//! Proving from the bytes that a new snapshot can be restored.
//!
//! Whatever an agent wrote is untrusted until the server has opened the
//! frames: a shard may name blobs that are not where it says, or not there at
//! all. [`verify_shard`] opens every frame a shard describes, and
//! [`check_snapshot_resolves`] accepts a snapshot only when every tree and
//! data blob resolves through shards whose frames were opened. An attestation
//! lets a shard's `supersedes` count; it never replaces opening the frames.

use std::collections::{HashMap, HashSet};

use crate::check::is_data_error;
use crate::crypto::{BLOB_TYPE_DATA, BLOB_TYPE_TREE, BlobId, Key32};
use crate::error::{Error, Result, within_size};
use crate::index::{
    INDEX_PREFIX, Index, MAX_SHARD_SIZE, SupersedesTrust, check_shard, fetch_shard_bytes, hex32,
    index_object_key, open_shard, parse_hex32,
};
use crate::keys::ReadKeys;
use crate::pack::{PACK_MAX_SIZE, PACK_PREFIX_LEN, pack_object_key, parse_entries, parse_prefix};
use crate::pb;
use crate::reader::open_chunk;
use crate::snapshot::{MAX_SNAPSHOT_SIZE, open_snapshot, snapshot_object_key};
use crate::store::{BlobError, BlobStore, read_bounded};
use crate::tree::decode_tree;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShardCheck {
    pub packs: u64,
    pub blobs: u64,
    pub bytes_read: u64,
}

async fn fetch_shard<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    shard_id: &BlobId,
) -> Result<Option<pb::IndexShard>> {
    match fetch_shard_bytes(store, &index_object_key(shard_id), None, MAX_SHARD_SIZE).await? {
        Some(bytes) => open_shard(keys.k_idx(), keys.repo_id(), shard_id, &bytes).map(Some),
        None => Ok(None),
    }
}

/// Opens every frame of every pack the shard names. `None` when the shard
/// object does not exist. A data error (`is_data_error`) means the shard lies;
/// any other error is the store failing and may be retried.
pub async fn verify_shard<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    shard_id: &BlobId,
) -> Result<Option<ShardCheck>> {
    match fetch_shard(store, keys, shard_id).await? {
        Some(shard) => verify_opened(store, keys, shard_id, &shard).await.map(Some),
        None => Ok(None),
    }
}

async fn verify_opened<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    shard_id: &BlobId,
    shard: &pb::IndexShard,
) -> Result<ShardCheck> {
    let mut out = ShardCheck::default();
    for pack in &shard.packs {
        let pack_id: BlobId =
            pack.pack_id.as_slice().try_into().map_err(|_| {
                Error::corrupt(index_object_key(shard_id), "pack id must be 32 bytes")
            })?;
        let name = pack_object_key(&pack_id);
        if pack.pack_size > PACK_MAX_SIZE as u64 {
            return Err(Error::corrupt(
                name,
                "the shard record names a pack larger than a pack can be",
            ));
        }
        let bytes = match store
            .get_range(&name, 0..pack.pack_size.saturating_add(1))
            .await
        {
            Ok(bytes) => bytes,
            Err(BlobError::NotFound) => return Err(Error::corrupt(name, "pack is missing")),
            Err(e) => return Err(e.into()),
        };
        if bytes.len() as u64 != pack.pack_size {
            return Err(Error::corrupt(name, "size differs from the shard record"));
        }
        let prefix = bytes
            .get(..PACK_PREFIX_LEN)
            .ok_or_else(|| Error::corrupt(name.clone(), "shorter than the pack prefix"))?;
        let prefix = parse_prefix(prefix)?;
        if prefix.hpke_enc != pack.hpke_enc || prefix.sealed_pack_key != pack.sealed_pack_key {
            return Err(Error::corrupt(
                name,
                "key material differs from the shard record",
            ));
        }
        let pack_key = prefix.open_key(keys.hpke_private_key(), keys.repo_id())?;
        for e in parse_entries(&pack.entries)? {
            let lies = || {
                Error::corrupt(
                    name.clone(),
                    format!("blob {} lies outside the pack", hex32(&e.blob_id)),
                )
            };
            let start = e.offset as usize;
            let end = start.checked_add(e.enc_len as usize).ok_or_else(lies)?;
            let frame = bytes.get(start..end).ok_or_else(lies)?;
            open_chunk(
                &pack_key,
                keys.repo_id(),
                e.blob_type,
                &e.blob_id,
                frame,
                e.ulen,
                keys.k_id(),
            )?;
            out.blobs += 1;
        }
        out.packs += 1;
        out.bytes_read += bytes.len() as u64;
    }
    Ok(out)
}

fn blob_ref(raw: &[u8], what: &str) -> Result<BlobId> {
    raw.try_into()
        .map_err(|_| Error::corrupt(what, "blob id must be 32 bytes"))
}

async fn read_snapshot<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    id: &BlobId,
) -> Result<pb::Snapshot> {
    let key = snapshot_object_key(id);
    let listed = store.list(&key).await?;
    let size = listed
        .iter()
        .find(|e| e.key == key)
        .map(|e| e.size)
        .ok_or(BlobError::NotFound)?;
    within_size(&key, size, MAX_SNAPSHOT_SIZE, "snapshot")?;
    let bytes = read_bounded(store, &key, Some(size), MAX_SNAPSHOT_SIZE, "snapshot").await?;
    open_snapshot(keys.hpke_private_key(), keys.repo_id(), id, &bytes)
}

/// The index shards a snapshot names, as the agent recorded them.
pub async fn snapshot_index_refs<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    snapshot_id: &BlobId,
) -> Result<Vec<BlobId>> {
    let snapshot = read_snapshot(store, keys, snapshot_id).await?;
    snapshot
        .index_refs
        .iter()
        .map(|r| blob_ref(r, &snapshot_object_key(snapshot_id)))
        .collect()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotCheck {
    pub trees: u64,
    pub data_blobs: u64,
    /// Shards the check opened the frames of itself; the caller records them
    /// as verified.
    pub newly_verified: Vec<BlobId>,
    /// Attested shards whose frames did not open during the check; the caller
    /// stops trusting them for deduplication.
    pub lying: Vec<BlobId>,
}

struct Resolver<'a, S> {
    store: &'a S,
    keys: &'a ReadKeys,
    verified: &'a HashSet<BlobId>,
    attested: &'a HashSet<BlobId>,
    index: Index,
    loaded: HashSet<BlobId>,
    newly_verified: Vec<BlobId>,
    lying: Vec<BlobId>,
    pack_keys: HashMap<BlobId, Key32>,
    // shortcut: after the first miss the blob ids of every listed attested shard are held in
    // memory, about the size of the index the server holds for GC; upgrade to per-miss
    // lookups if snapshots with many unrelated misses show up.
    holders: Option<HashMap<(BlobId, u8), Vec<BlobId>>>,
}

impl<S: BlobStore> Resolver<'_, S> {
    /// Adds a shard to the index: a verified one as it is, an attested one
    /// only after its frames open. A shard that is neither is never loaded.
    /// One that fails with a data error is left out and not tried again: any
    /// writer can plant a shard, and another may still hold the blob.
    async fn load_shard(&mut self, id: &BlobId) -> Result<()> {
        let is_verified = self.verified.contains(id);
        if self.loaded.contains(id) || !(is_verified || self.attested.contains(id)) {
            return Ok(());
        }
        match self.add(id, is_verified).await {
            Err(e) if is_data_error(&e) => {
                self.loaded.insert(*id);
                if !is_verified {
                    self.lying.push(*id);
                }
                Ok(())
            }
            other => other,
        }
    }

    async fn add(&mut self, id: &BlobId, is_verified: bool) -> Result<()> {
        let Some(shard) = fetch_shard(self.store, self.keys, id).await? else {
            return Ok(());
        };
        if !is_verified {
            verify_opened(self.store, self.keys, id, &shard).await?;
        }
        self.index.add_shard(*id, &shard)?;
        if !is_verified {
            self.newly_verified.push(*id);
        }
        self.loaded.insert(*id);
        Ok(())
    }

    async fn build_holders(&mut self) -> Result<()> {
        let mut holders: HashMap<(BlobId, u8), Vec<BlobId>> = HashMap::new();
        for entry in self.store.list(INDEX_PREFIX).await? {
            let Some(id) = entry.key.strip_prefix(INDEX_PREFIX).and_then(parse_hex32) else {
                continue;
            };
            if self.loaded.contains(&id) {
                continue;
            }
            if self.verified.contains(&id) {
                self.load_shard(&id).await?;
            } else if self.attested.contains(&id) {
                let Some(shard) = fetch_shard_lenient(self.store, self.keys, &id).await? else {
                    continue;
                };
                if check_shard(id, &shard).is_err() {
                    continue;
                }
                for pack in &shard.packs {
                    for e in parse_entries(&pack.entries)? {
                        holders
                            .entry((e.blob_id, e.blob_type))
                            .or_default()
                            .push(id);
                    }
                }
            }
        }
        self.holders = Some(holders);
        Ok(())
    }

    /// Makes sure the blob resolves in the index. A miss lists the shards
    /// once: verified ones are added, and an attested one is opened and
    /// verified only if it holds a record of a blob that is missing.
    async fn ensure(&mut self, id: &BlobId, blob_type: u8) -> Result<()> {
        if self.index.get(id, blob_type).is_some() {
            return Ok(());
        }
        if self.holders.is_none() {
            self.build_holders().await?;
        }
        let from = self
            .holders
            .as_ref()
            .and_then(|h| h.get(&(*id, blob_type)))
            .cloned()
            .unwrap_or_default();
        for shard in from {
            self.load_shard(&shard).await?;
        }
        if self.index.get(id, blob_type).is_some() {
            return Ok(());
        }
        Err(Error::corrupt(
            format!("blob {}", hex32(id)),
            "is not in a shard the server verified",
        ))
    }

    async fn read_at(
        &mut self,
        loc: &crate::index::BlobLocation,
        id: &BlobId,
        blob_type: u8,
    ) -> Result<Vec<u8>> {
        let name = || format!("blob {}", hex32(id));
        let pack_name = pack_object_key(&loc.pack_id);
        let missing = |e: BlobError| match e {
            BlobError::NotFound => Error::corrupt(name(), "its pack is missing from the store"),
            e => e.into(),
        };
        let pack_key = match self.pack_keys.get(&loc.pack_id) {
            Some(key) => key.clone(),
            None => {
                let prefix = self
                    .store
                    .get_range(&pack_name, 0..PACK_PREFIX_LEN as u64)
                    .await
                    .map_err(missing)?;
                let key = parse_prefix(&prefix)?
                    .open_key(self.keys.hpke_private_key(), self.keys.repo_id())?;
                self.pack_keys.insert(loc.pack_id, key.clone());
                key
            }
        };
        let start = u64::from(loc.offset);
        let frame = self
            .store
            .get_range(&pack_name, start..start + u64::from(loc.enc_len))
            .await
            .map_err(missing)?;
        if frame.len() != loc.enc_len as usize {
            return Err(Error::corrupt(
                name(),
                "pack is shorter than the index says",
            ));
        }
        open_chunk(
            &pack_key,
            self.keys.repo_id(),
            blob_type,
            id,
            &frame,
            loc.ulen,
            self.keys.k_id(),
        )
    }

    /// The first known copy of the tree that opens.
    async fn read_tree(&mut self, id: &BlobId) -> Result<Vec<u8>> {
        let mut first_err = None;
        for loc in self.index.locations(id, BLOB_TYPE_TREE) {
            match self.read_at(&loc, id, BLOB_TYPE_TREE).await {
                Ok(bytes) => return Ok(bytes),
                Err(e) if is_data_error(&e) => {
                    first_err.get_or_insert(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(first_err.unwrap_or_else(|| {
            Error::corrupt(format!("blob {}", hex32(id)), "has no known location")
        }))
    }
}

/// Like `fetch_shard`, but a shard that does not open is skipped: this only
/// looks for candidates, and an attested shard that is damaged cannot help.
async fn fetch_shard_lenient<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    id: &BlobId,
) -> Result<Option<pb::IndexShard>> {
    match fetch_shard(store, keys, id).await {
        Err(e) if is_data_error(&e) => Ok(None),
        other => other,
    }
}

/// Accepts the snapshot only when its trees and data blobs all resolve in an
/// index built from shards whose frames the server opened: those in
/// `verified`, and those in `attested` that [`verify_shard`] passes during
/// this call (reported in `newly_verified`). Any other shard is never read.
/// Trees are read and opened; data blobs are not read, because their records
/// come from opened frames.
pub async fn check_snapshot_resolves<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    snapshot_id: &BlobId,
    verified: &HashSet<BlobId>,
    attested: &HashSet<BlobId>,
) -> Result<SnapshotCheck> {
    let snapshot = read_snapshot(store, keys, snapshot_id).await?;
    let name = snapshot_object_key(snapshot_id);
    let mut index = Index::default();
    index.set_supersedes_trust(SupersedesTrust::Only(attested.clone()));
    let mut r = Resolver {
        store,
        keys,
        verified,
        attested,
        index,
        loaded: HashSet::new(),
        newly_verified: Vec::new(),
        lying: Vec::new(),
        pack_keys: HashMap::new(),
        holders: None,
    };
    for raw in &snapshot.index_refs {
        r.load_shard(&blob_ref(raw, &name)?).await?;
    }
    let mut out = SnapshotCheck::default();
    let mut seen = HashSet::new();
    let mut stack = vec![blob_ref(&snapshot.root_tree, &name)?];
    while let Some(tree) = stack.pop() {
        if !seen.insert(tree) {
            continue;
        }
        r.ensure(&tree, BLOB_TYPE_TREE).await?;
        let bytes = r.read_tree(&tree).await?;
        out.trees += 1;
        for node in decode_tree(&bytes, None)? {
            for raw in &node.content {
                r.ensure(&blob_ref(raw, "tree")?, BLOB_TYPE_DATA).await?;
                out.data_blobs += 1;
            }
            for raw in &node.subtrees {
                stack.push(blob_ref(raw, "tree")?);
            }
        }
    }
    out.newly_verified = r.newly_verified;
    out.lying = r.lying;
    Ok(out)
}
