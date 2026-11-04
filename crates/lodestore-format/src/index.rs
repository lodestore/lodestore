//! Index shards: immutable, AEAD-sealed lists of the blobs of one or more
//! packs. The in-memory index maps a blob id to its location.

use std::collections::{HashMap, HashSet};

use prost::Message;

use crate::attest::{MARKER_LEGACY_SHARDS, read_attestations};
use crate::crypto::{
    BlobId, Entropy, HPKE_ENC_LEN, RepoId, SEALED_KEY_LEN, open_index_shard, seal_index_shard,
};
use crate::error::{Error, Result};
use crate::keys::ReadKeys;
use crate::locks::read_gc_locks;
use crate::pack::{
    ENTRY_LEN, PACK_PREFIX_LEN, frame_len_in_range, max_ulen, pack_object_key, parse_entries,
    read_pack_header,
};
use crate::pb;
use crate::store::{BlobError, BlobStore, put_named};

pub(crate) const INDEX_PREFIX: &str = "index/";
pub(crate) const PACKS_PREFIX: &str = "packs/";
/// Pack records per shard written by a rebuild.
pub(crate) const REBUILD_SHARD_PACKS: usize = 1024;

pub fn hex32(id: &[u8; 32]) -> String {
    blake3::Hash::from_bytes(*id).to_hex().to_string()
}

pub fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    blake3::Hash::from_hex(s).ok().map(|h| *h.as_bytes())
}

pub fn index_object_key(shard_id: &BlobId) -> String {
    format!("{INDEX_PREFIX}{}", hex32(shard_id))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobLocation {
    pub pack_id: BlobId,
    pub offset: u32,
    pub enc_len: u32,
    pub ulen: u32,
    pub blob_type: u8,
    pub shard: BlobId,
}

/// Seals a shard. Returns `(shard_id, bytes)`; the object is `index/<hex id>`.
pub fn seal_shard(
    k_idx: &[u8; 32],
    repo_id: &RepoId,
    packs: Vec<pb::PackRecord>,
    entropy: &mut Entropy,
) -> Result<(BlobId, Vec<u8>)> {
    seal_shard_superseding(k_idx, repo_id, packs, Vec::new(), entropy)
}

/// Like [`seal_shard`], for a shard that replaces the listed shards: a reader
/// drops their records, whatever order it loads them in.
pub fn seal_shard_superseding(
    k_idx: &[u8; 32],
    repo_id: &RepoId,
    packs: Vec<pb::PackRecord>,
    mut supersedes: Vec<BlobId>,
    entropy: &mut Entropy,
) -> Result<(BlobId, Vec<u8>)> {
    supersedes.sort();
    supersedes.dedup();
    let shard = pb::IndexShard {
        packs,
        supersedes: supersedes.iter().map(|id| id.to_vec()).collect(),
    };
    let bytes = seal_index_shard(k_idx, repo_id, &shard.encode_to_vec(), entropy)?;
    Ok((*blake3::hash(&bytes).as_bytes(), bytes))
}

/// Opens a shard after checking that its name is the hash of its bytes.
pub fn open_shard(
    k_idx: &[u8; 32],
    repo_id: &RepoId,
    shard_id: &BlobId,
    bytes: &[u8],
) -> Result<pb::IndexShard> {
    let name = index_object_key(shard_id);
    if blake3::hash(bytes).as_bytes() != shard_id {
        return Err(Error::corrupt(
            name,
            "object name does not match its content",
        ));
    }
    let plain = open_index_shard(k_idx, repo_id, bytes)
        .map_err(|_| Error::corrupt(name.clone(), "authentication failed"))?;
    pb::IndexShard::decode(plain.as_slice())
        .map_err(|e| Error::corrupt(name, format!("undecodable: {e}")))
}

// ponytail: the whole index lives in one HashMap (~100M blobs is the ceiling,
// about 10 GB of RAM); the 45-byte sorted records allow binary search over an
// mmap later without a format change.
/// Whose `supersedes` lists an [`Index`] honours.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SupersedesTrust {
    /// Every shard's, as written; the format-level resolution rule.
    #[default]
    All,
    /// Only the listed shards' (those with a valid attestation). Any other
    /// shard may come from a holder of write keys and says nothing about
    /// which records are stale.
    Only(HashSet<BlobId>),
}

#[derive(Default)]
pub struct Index {
    map: HashMap<(BlobId, u8), BlobLocation>,
    /// Copies of blobs in `map` held by other packs, from shards that were
    /// not superseded when they were added; one takes over when the chosen
    /// location is superseded or its pack is gone.
    others: HashMap<(BlobId, u8), Vec<BlobLocation>>,
    stale: HashMap<(BlobId, u8), Vec<BlobLocation>>,
    shards: HashSet<BlobId>,
    packs: HashMap<BlobId, u64>,
    skipped: Vec<(String, String)>,
    excluded: HashSet<BlobId>,
    superseded: HashSet<BlobId>,
    trust: SupersedesTrust,
}

impl Index {
    /// A data chunk and a tree blob with equal plaintext share an id, so the
    /// type is part of the key.
    pub fn get(&self, id: &BlobId, blob_type: u8) -> Option<&BlobLocation> {
        self.map.get(&(*id, blob_type))
    }

    /// Every known location of a blob, the chosen one first. Reading tries
    /// them in order: a shard from a write-key holder can name a pack with
    /// wrong offsets, and the real record is among the others.
    pub fn locations(&self, id: &BlobId, blob_type: u8) -> Vec<BlobLocation> {
        let key = (*id, blob_type);
        let Some(chosen) = self.map.get(&key) else {
            return Vec::new();
        };
        let mut all = vec![*chosen];
        if let Some(others) = self.others.get(&key) {
            all.extend_from_slice(others);
        }
        if let Some(stale) = self.stale.get(&key) {
            all.extend_from_slice(stale);
        }
        all
    }

    /// Chooses whose `supersedes` count. Call it before the first
    /// [`Index::add_shard`].
    pub fn set_supersedes_trust(&mut self, trust: SupersedesTrust) {
        debug_assert!(self.shards.is_empty(), "trust set after shards were added");
        self.trust = trust;
    }

    /// `(object key, reason)` of every listed shard [`Index::load_all`] could
    /// not use. Losing one costs deduplication and the packs it described are
    /// found again by a rebuild.
    pub fn skipped(&self) -> &[(String, String)] {
        &self.skipped
    }

    pub fn has_shard(&self, shard_id: &BlobId) -> bool {
        self.shards.contains(shard_id)
    }

    pub fn has_pack(&self, pack_id: &BlobId) -> bool {
        self.packs.contains_key(pack_id)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Packs whose records are ignored by every shard added afterwards, so
    /// their blobs count as unknown (or are found at another location). Call
    /// it before loading shards.
    pub fn exclude_packs(&mut self, packs: HashSet<BlobId>) {
        self.excluded = packs;
    }

    /// A shard listed in the `supersedes` of another shard loses to every
    /// shard that is not superseded: its record of a blob is used only while
    /// no such shard places the blob anywhere. Nothing a shard says makes a
    /// blob unresolvable, so a shard from a write-only key or a compaction
    /// cut short cannot hide data. Which location wins does not depend on the
    /// order shards are added in, except between equally ranked copies.
    pub fn add_shard(&mut self, shard_id: BlobId, shard: &pb::IndexShard) -> Result<()> {
        let bad = |reason: &str| Error::corrupt(index_object_key(&shard_id), reason.to_owned());
        let supersedes = shard
            .supersedes
            .iter()
            .map(|s| {
                BlobId::try_from(s.as_slice())
                    .map_err(|_| bad("superseded shard id must be 32 bytes"))
            })
            .collect::<Result<Vec<_>>>()?;
        let supersedes = match &self.trust {
            SupersedesTrust::Only(trusted) if !trusted.contains(&shard_id) => Vec::new(),
            _ => supersedes,
        };
        let mut staged = Vec::new();
        let mut pack_sizes = Vec::new();
        for pack in &shard.packs {
            let pack_id: BlobId = pack
                .pack_id
                .as_slice()
                .try_into()
                .map_err(|_| bad("pack id must be 32 bytes"))?;
            let excluded = self.excluded.contains(&pack_id);
            if !excluded {
                pack_sizes.push((pack_id, pack.pack_size));
            }
            if pack.hpke_enc.len() != HPKE_ENC_LEN || pack.sealed_pack_key.len() != SEALED_KEY_LEN {
                return Err(bad("sealed pack key has the wrong size"));
            }
            for e in parse_entries(&pack.entries)? {
                let end = u64::from(e.offset) + u64::from(e.enc_len);
                if (e.offset as usize) < PACK_PREFIX_LEN
                    || end > pack.pack_size
                    || e.ulen as usize > max_ulen(e.blob_type)
                    || !frame_len_in_range(e.blob_type, e.enc_len)
                {
                    return Err(bad("entry lies outside its pack"));
                }
                if excluded {
                    continue;
                }
                staged.push((
                    e.blob_id,
                    BlobLocation {
                        pack_id,
                        offset: e.offset,
                        enc_len: e.enc_len,
                        ulen: e.ulen,
                        blob_type: e.blob_type,
                        shard: shard_id,
                    },
                ));
            }
        }
        self.shards.insert(shard_id);
        let newly: HashSet<BlobId> = supersedes
            .into_iter()
            .filter(|id| self.superseded.insert(*id))
            .collect();
        if !newly.is_empty() {
            self.prefer_current(&newly);
        }
        for (id, loc) in staged {
            self.insert((id, loc.blob_type), loc);
        }
        self.packs.extend(pack_sizes);
        Ok(())
    }

    fn is_current(&self, loc: &BlobLocation) -> bool {
        !self.superseded.contains(&loc.shard)
    }

    /// A copy from a superseded shard is only kept while nothing better is
    /// known; a copy in another pack from a current shard is kept aside.
    fn insert(&mut self, key: (BlobId, u8), loc: BlobLocation) {
        let current = self.is_current(&loc);
        let held_current = match self.map.get(&key).copied() {
            None => {
                self.map.insert(key, loc);
                return;
            }
            Some(held) if held.pack_id == loc.pack_id => {
                if current && !self.is_current(&held) {
                    self.push_stale(key, held);
                    self.map.insert(key, loc);
                } else if current && !same_extent(&held, &loc) {
                    self.push_other(key, loc);
                } else if !current && !same_extent(&held, &loc) {
                    self.push_stale(key, loc);
                }
                return;
            }
            Some(held) => self.is_current(&held),
        };
        if !current {
            self.push_stale(key, loc);
            return;
        }
        if held_current {
            self.push_other(key, loc);
        } else {
            let held = self.map.insert(key, loc);
            if let Some(held) = held {
                self.push_stale(key, held);
            }
        }
    }

    fn push_stale(&mut self, key: (BlobId, u8), loc: BlobLocation) {
        let alts = self.stale.entry(key).or_default();
        if !alts.contains(&loc) {
            alts.push(loc);
        }
    }

    fn push_other(&mut self, key: (BlobId, u8), loc: BlobLocation) {
        let alts = self.others.entry(key).or_default();
        if !alts.contains(&loc) {
            alts.push(loc);
        }
    }

    /// Moves every blob whose chosen location is in a `newly` superseded
    /// shard to a current copy, when one is known.
    fn prefer_current(&mut self, newly: &HashSet<BlobId>) {
        let mut emptied = Vec::new();
        let mut demoted = Vec::new();
        for (key, loc) in &mut self.map {
            if !newly.contains(&loc.shard) {
                continue;
            }
            let Some(alts) = self.others.get_mut(key) else {
                continue;
            };
            if let Some(i) = alts
                .iter()
                .position(|a| !self.superseded.contains(&a.shard))
            {
                demoted.push((*key, *loc));
                *loc = alts.swap_remove(i);
                if alts.is_empty() {
                    emptied.push(*key);
                }
            }
        }
        for (key, loc) in demoted {
            self.push_stale(key, loc);
        }
        for key in emptied {
            self.others.remove(&key);
        }
    }

    /// Forgets every location in a pack of `gone`, falling back to another
    /// known copy of the blob.
    fn forget_packs(&mut self, gone: &HashSet<BlobId>) {
        self.others.retain(|_, alts| {
            alts.retain(|a| !gone.contains(&a.pack_id));
            !alts.is_empty()
        });
        self.stale.retain(|_, alts| {
            alts.retain(|a| !gone.contains(&a.pack_id));
            !alts.is_empty()
        });
        let mut lost = Vec::new();
        for (key, loc) in &mut self.map {
            if !gone.contains(&loc.pack_id) {
                continue;
            }
            match self.others.get_mut(key) {
                Some(alts) => {
                    let i = alts
                        .iter()
                        .position(|a| !self.superseded.contains(&a.shard))
                        .unwrap_or(0);
                    *loc = alts.swap_remove(i);
                    if alts.is_empty() {
                        self.others.remove(key);
                    }
                }
                None => lost.push(*key),
            }
        }
        for key in lost {
            self.map.remove(&key);
        }
        self.packs.retain(|id, _| !gone.contains(id));
    }

    /// Forgets every record of a pack that the store does not list with the
    /// size the record claims, and returns how many packs that was. A shard
    /// is authenticated by `k_idx`, which every writer holds, so it can
    /// describe packs that do not exist; trusting it would make a backup skip
    /// uploading data it believes is stored. Needs LIST on `packs/`, which a
    /// write-only credential does not have.
    ///
    /// ponytail: this does not look inside a pack, so a pack that exists but
    /// lacks the claimed blob is still trusted; reading headers is what
    /// `rebuild_index` does and costs three ranged GETs per pack.
    pub async fn retain_listed_packs<S: BlobStore>(&mut self, store: &S) -> Result<usize> {
        Ok(self.retain_packs(&listed_packs(store).await?))
    }

    /// [`Index::retain_listed_packs`] against a listing taken already.
    pub fn retain_packs(&mut self, listed: &HashMap<BlobId, u64>) -> usize {
        let missing: HashSet<BlobId> = self
            .packs
            .iter()
            .filter(|(id, size)| listed.get(*id) != Some(size))
            .map(|(id, _)| *id)
            .collect();
        self.forget_packs(&missing);
        missing.len()
    }

    async fn load_one<S: BlobStore>(
        &mut self,
        store: &S,
        k_idx: &[u8; 32],
        repo_id: &RepoId,
        shard_id: BlobId,
    ) -> Result<bool> {
        if self.has_shard(&shard_id) {
            return Ok(true);
        }
        match store.get(&index_object_key(&shard_id)).await {
            Ok(bytes) => {
                let shard = open_shard(k_idx, repo_id, &shard_id, &bytes)?;
                self.add_shard(shard_id, &shard)?;
                Ok(true)
            }
            Err(BlobError::NotFound) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Loads the shards a snapshot names by their exact keys. A shard that is
    /// gone is skipped; the caller falls back to [`Index::load_all`].
    pub async fn load_refs<S: BlobStore>(
        &mut self,
        store: &S,
        k_idx: &[u8; 32],
        repo_id: &RepoId,
        refs: &[Vec<u8>],
    ) -> Result<()> {
        for r in refs {
            let shard_id: BlobId = r
                .as_slice()
                .try_into()
                .map_err(|_| Error::corrupt("snapshot", "index reference must be 32 bytes"))?;
            self.load_one(store, k_idx, repo_id, shard_id).await?;
        }
        Ok(())
    }

    /// Discovery by LIST. Only used when no snapshot names the shards (a
    /// writer starting up) or when a named shard is missing. A shard that
    /// does not open is recorded in [`Index::skipped`] and left out.
    pub async fn load_all<S: BlobStore>(
        &mut self,
        store: &S,
        k_idx: &[u8; 32],
        repo_id: &RepoId,
    ) -> Result<()> {
        for entry in store.list(INDEX_PREFIX).await? {
            let Some(shard_id) = entry.key.strip_prefix(INDEX_PREFIX).and_then(parse_hex32) else {
                continue;
            };
            match self.load_one(store, k_idx, repo_id, shard_id).await {
                Ok(_) => {}
                Err(e @ Error::Corrupt { .. }) => self.skipped.push((entry.key, e.to_string())),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

fn same_extent(a: &BlobLocation, b: &BlobLocation) -> bool {
    (a.pack_id, a.offset, a.enc_len, a.ulen) == (b.pack_id, b.offset, b.enc_len, b.ulen)
}

/// The packs `packs/` lists, with their sizes.
pub async fn listed_packs<S: BlobStore>(store: &S) -> Result<HashMap<BlobId, u64>> {
    let mut listed = HashMap::new();
    for entry in store.list(PACKS_PREFIX).await? {
        let id = entry.key.rsplit('/').next().and_then(parse_hex32);
        if let Some(id) = id.filter(|id| pack_object_key(id) == entry.key) {
            listed.insert(id, entry.size);
        }
    }
    Ok(listed)
}

#[derive(Debug, Default)]
pub struct RebuildReport {
    pub packs_scanned: u64,
    pub packs_indexed: u64,
    pub shards_written: u64,
    /// `(object key, reason)` of every pack that could not be parsed.
    pub packs_skipped: Vec<(String, String)>,
    /// `(object key, reason)` of every index shard that could not be opened.
    pub shards_skipped: Vec<(String, String)>,
}

/// Re-creates index entries for every pack that no trusted shard mentions,
/// reading only each pack's prefix, trailer and header. Once a server has
/// upgraded the repository only attested shards count, so a shard nobody
/// vouches for cannot hide a pack from the rebuild; before that every shard
/// counts, as nothing is attested yet. The shards written here are not
/// attested: the header of a pack is written by whoever uploaded the pack.
/// Packs are not hashed: a pack whose bytes differ from its name is only noticed when a blob of it is read.
/// Packs a running cleanup marked for deletion are left alone: indexing one
/// of them again would make a shard name a pack that is about to go.
///
/// ponytail: packs are read one after another, three ranged GETs each; run them
/// concurrently if rebuilding a very large bucket gets slow.
pub async fn rebuild_index<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    progress: &(dyn Fn(u64, u64) + Sync),
) -> Result<RebuildReport> {
    let locks = read_gc_locks(store, keys.k_idx(), keys.repo_id()).await?;
    if locks.denied {
        return Err(Error::Store(BlobError::Denied {
            message: "locks/ can't be read, so a running cleanup can't be ruled out".into(),
        }));
    }
    let doomed = locks.candidate_packs();
    let attested = read_attestations(store, keys).await?;
    let upgraded = attested.markers.contains(&MARKER_LEGACY_SHARDS);
    let mut index = Index::default();
    for entry in store.list(INDEX_PREFIX).await? {
        let Some(shard_id) = entry
            .key
            .strip_prefix(INDEX_PREFIX)
            .and_then(parse_hex32)
            .filter(|id| !upgraded || attested.shards.contains(id))
        else {
            continue;
        };
        match index
            .load_one(store, keys.k_idx(), keys.repo_id(), shard_id)
            .await
        {
            Ok(_) => {}
            Err(e @ Error::Corrupt { .. }) => index.skipped.push((entry.key, e.to_string())),
            Err(e) => return Err(e),
        }
    }

    let mut todo = Vec::new();
    for entry in store.list(PACKS_PREFIX).await? {
        let pack_id = entry.key.rsplit('/').next().and_then(parse_hex32);
        if let Some(id) = pack_id.filter(|id| pack_object_key(id) == entry.key)
            && !index.has_pack(&id)
            && !doomed.contains(&id)
        {
            todo.push((entry.key, id, entry.size));
        }
    }

    let mut report = RebuildReport {
        shards_skipped: index.skipped().to_vec(),
        ..Default::default()
    };
    let mut entropy = Entropy::os();
    let mut records = Vec::new();
    let total = todo.len() as u64;
    for (key, pack_id, size) in todo {
        report.packs_scanned += 1;
        progress(report.packs_scanned, total);
        match read_pack_header(store, &key, size, keys).await {
            Ok(mut header) => {
                header.entries.sort_by_key(|e| e.blob_id);
                let mut entries = Vec::with_capacity(header.entries.len() * ENTRY_LEN);
                for e in &header.entries {
                    e.write(&mut entries);
                }
                records.push(pb::PackRecord {
                    pack_id: pack_id.to_vec(),
                    hpke_enc: header.prefix.hpke_enc,
                    sealed_pack_key: header.prefix.sealed_pack_key,
                    pack_size: size,
                    entries,
                });
                report.packs_indexed += 1;
            }
            Err(e @ (Error::Corrupt { .. } | Error::UnsupportedVersion { .. })) => {
                report.packs_skipped.push((key, e.to_string()));
            }
            Err(e) => return Err(e),
        }
        if records.len() >= REBUILD_SHARD_PACKS {
            write_rebuilt_shard(store, keys, &mut records, &mut entropy, &mut report).await?;
        }
    }
    write_rebuilt_shard(store, keys, &mut records, &mut entropy, &mut report).await?;
    Ok(report)
}

async fn write_rebuilt_shard<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    records: &mut Vec<pb::PackRecord>,
    entropy: &mut Entropy,
    report: &mut RebuildReport,
) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let (id, bytes) = seal_shard(
        keys.k_idx(),
        keys.repo_id(),
        std::mem::take(records),
        entropy,
    )?;
    put_named(store, &index_object_key(&id), bytes).await?;
    report.shards_written += 1;
    Ok(())
}

#[cfg(test)]
mod tests {
    use rand_chacha::ChaCha20Rng;
    use rand_chacha::rand_core::SeedableRng;

    use super::*;
    use crate::pack::ENTRY_LEN;

    fn record(blob: u8, offset: u32, size: u64) -> pb::PackRecord {
        let mut entries = vec![blob; 32];
        entries.push(1);
        entries.extend_from_slice(&offset.to_le_bytes());
        entries.extend_from_slice(&100u32.to_le_bytes());
        entries.extend_from_slice(&200u32.to_le_bytes());
        assert_eq!(entries.len(), ENTRY_LEN);
        pb::PackRecord {
            pack_id: vec![9; 32],
            hpke_enc: vec![0; HPKE_ENC_LEN],
            sealed_pack_key: vec![0; SEALED_KEY_LEN],
            pack_size: size,
            entries,
        }
    }

    #[test]
    fn index_shard_roundtrip_and_name_check() {
        let mut ent = Entropy::from_rng(ChaCha20Rng::from_seed([1; 32]));
        let (k, repo) = ([7u8; 32], [8u8; 16]);
        let good = record(4, PACK_PREFIX_LEN as u32, 5000);
        let (id, bytes) = seal_shard(&k, &repo, vec![good], &mut ent).unwrap();

        let shard = open_shard(&k, &repo, &id, &bytes).unwrap();
        let mut index = Index::default();
        index.add_shard(id, &shard).unwrap();
        assert_eq!(index.get(&[4; 32], 1).unwrap().ulen, 200);
        assert!(index.get(&[4; 32], 2).is_none());
        assert!(index.has_shard(&id));

        let other = [0xAA; 32];
        assert!(matches!(
            open_shard(&k, &repo, &other, &bytes),
            Err(Error::Corrupt { .. })
        ));
        assert!(open_shard(&[1; 32], &repo, &id, &bytes).is_err());

        let huge = {
            let mut r = record(6, PACK_PREFIX_LEN as u32, 1 << 40);
            r.entries[37..41].copy_from_slice(&u32::MAX.to_le_bytes());
            r
        };
        let mut index = Index::default();
        let bad = pb::IndexShard {
            packs: vec![huge],
            supersedes: Vec::new(),
        };
        assert!(matches!(
            index.add_shard(id, &bad),
            Err(Error::Corrupt { .. })
        ));

        let outside = record(5, 4950, 5000);
        let mut index = Index::default();
        let bad = pb::IndexShard {
            packs: vec![outside],
            supersedes: Vec::new(),
        };
        assert!(matches!(
            index.add_shard(id, &bad),
            Err(Error::Corrupt { .. })
        ));
        assert!(index.is_empty());
    }

    fn shard_of(pack: u8, blob: u8, supersedes: &[BlobId]) -> pb::IndexShard {
        let mut rec = record(blob, PACK_PREFIX_LEN as u32, 5000);
        rec.pack_id = vec![pack; 32];
        pb::IndexShard {
            packs: vec![rec],
            supersedes: supersedes.iter().map(|s| s.to_vec()).collect(),
        }
    }

    fn two_blob_shard(pack: u8, blobs: [u8; 2]) -> pb::IndexShard {
        let mut shard = shard_of(pack, blobs[0], &[]);
        shard.packs[0]
            .entries
            .extend_from_slice(&record(blobs[1], PACK_PREFIX_LEN as u32 + 200, 5000).entries);
        shard
    }

    #[test]
    fn index_supersedes_order_independent() {
        let (old, new) = ([1u8; 32], [2u8; 32]);
        let old_shard = two_blob_shard(10, [4, 5]);
        let new_shard = shard_of(10, 4, &[old]);
        for order in [
            [(old, &old_shard), (new, &new_shard)],
            [(new, &new_shard), (old, &old_shard)],
        ] {
            let mut index = Index::default();
            for (id, shard) in order {
                index.add_shard(id, shard).unwrap();
            }
            assert_eq!(index.get(&[4; 32], 1).unwrap().shard, new);
            assert_eq!(index.get(&[5; 32], 1).unwrap().shard, old);
            assert!(index.has_shard(&old) && index.has_shard(&new));
        }
    }

    #[test]
    fn index_supersedes_keeps_a_current_copy() {
        let (b, c, a) = ([1u8; 32], [2u8; 32], [3u8; 32]);
        let shard_b = shard_of(10, 4, &[]);
        let shard_c = shard_of(11, 4, &[]);
        let shard_a = shard_of(12, 6, &[b]);
        let orders = [
            [(b, &shard_b), (c, &shard_c), (a, &shard_a)],
            [(a, &shard_a), (b, &shard_b), (c, &shard_c)],
            [(c, &shard_c), (a, &shard_a), (b, &shard_b)],
            [(b, &shard_b), (a, &shard_a), (c, &shard_c)],
        ];
        for order in orders {
            let mut index = Index::default();
            index.exclude_packs(HashSet::from([[12u8; 32]]));
            for (id, shard) in order {
                index.add_shard(id, shard).unwrap();
            }
            assert_eq!(index.get(&[4; 32], 1).unwrap().pack_id, [11; 32]);
        }
    }

    #[test]
    fn index_planted_supersedes_hides_nothing() {
        let (legit, planted) = ([1u8; 32], [2u8; 32]);
        let planted_shard = pb::IndexShard {
            packs: Vec::new(),
            supersedes: vec![legit.to_vec(), planted.to_vec()],
        };
        let legit_shard = two_blob_shard(10, [4, 5]);
        for order in [
            [(legit, &legit_shard), (planted, &planted_shard)],
            [(planted, &planted_shard), (legit, &legit_shard)],
        ] {
            let mut index = Index::default();
            for (id, shard) in order {
                index.add_shard(id, shard).unwrap();
            }
            assert_eq!(index.get(&[4; 32], 1).unwrap().pack_id, [10; 32]);
            assert_eq!(index.get(&[5; 32], 1).unwrap().pack_id, [10; 32]);
            assert!(index.has_pack(&[10; 32]));
        }
    }

    #[test]
    fn index_missing_pack_falls_back() {
        let mut index = Index::default();
        index.add_shard([1; 32], &shard_of(10, 4, &[])).unwrap();
        index.add_shard([2; 32], &shard_of(11, 4, &[])).unwrap();
        index.add_shard([3; 32], &shard_of(12, 5, &[])).unwrap();
        let listed = HashMap::from([([11u8; 32], 5000u64)]);
        assert_eq!(index.retain_packs(&listed), 2);
        assert_eq!(index.get(&[4; 32], 1).unwrap().pack_id, [11; 32]);
        assert!(index.get(&[5; 32], 1).is_none());
        assert!(!index.has_pack(&[10; 32]) && index.has_pack(&[11; 32]));
    }

    #[test]
    fn index_supersedes_prefers_new_location() {
        let (old, new) = ([1u8; 32], [2u8; 32]);
        let old_shard = shard_of(10, 4, &[]);
        let new_shard = shard_of(11, 4, &[old]);
        for order in [
            [(old, &old_shard), (new, &new_shard)],
            [(new, &new_shard), (old, &old_shard)],
        ] {
            let mut index = Index::default();
            for (id, shard) in order {
                index.add_shard(id, shard).unwrap();
            }
            assert_eq!(index.get(&[4; 32], 1).unwrap().pack_id, [11; 32]);
        }
    }

    #[test]
    fn index_excluded_pack_keeps_second_location() {
        let mut index = Index::default();
        index.exclude_packs(HashSet::from([[10u8; 32]]));
        index.add_shard([1; 32], &shard_of(10, 4, &[])).unwrap();
        assert!(index.get(&[4; 32], 1).is_none());
        assert!(!index.has_pack(&[10; 32]));
        index.add_shard([2; 32], &shard_of(11, 4, &[])).unwrap();
        assert_eq!(index.get(&[4; 32], 1).unwrap().pack_id, [11; 32]);
    }

    #[test]
    fn index_supersedes_rejects_bad_id() {
        let mut shard = shard_of(10, 4, &[]);
        shard.supersedes.push(vec![0; 5]);
        assert!(matches!(
            Index::default().add_shard([1; 32], &shard),
            Err(Error::Corrupt { .. })
        ));
    }

    fn shifted_shard(pack: u8, blob: u8, offset: u32) -> pb::IndexShard {
        let mut shard = shard_of(pack, blob, &[]);
        shard.packs[0].entries = record(blob, offset, 5000).entries;
        shard
    }

    #[test]
    fn index_untrusted_supersedes_keeps_legit_copy() {
        let (legit, planted) = ([1u8; 32], [2u8; 32]);
        let legit_shard = shard_of(10, 4, &[]);
        let planted_shard = shard_of(12, 4, &[legit]);
        for order in [
            [(legit, &legit_shard), (planted, &planted_shard)],
            [(planted, &planted_shard), (legit, &legit_shard)],
        ] {
            let mut index = Index::default();
            index.set_supersedes_trust(SupersedesTrust::Only(HashSet::new()));
            for (id, shard) in order {
                index.add_shard(id, shard).unwrap();
            }
            let packs: HashSet<BlobId> = index
                .locations(&[4; 32], 1)
                .iter()
                .map(|l| l.pack_id)
                .collect();
            assert_eq!(packs, HashSet::from([[10u8; 32], [12u8; 32]]));
        }
        let mut index = Index::default();
        index.add_shard(legit, &legit_shard).unwrap();
        index.add_shard(planted, &planted_shard).unwrap();
        assert_eq!(index.get(&[4; 32], 1).unwrap().pack_id, [12; 32]);
    }

    #[test]
    fn index_trusted_supersedes_prefers_new_location() {
        let (old, new) = ([1u8; 32], [2u8; 32]);
        let old_shard = shard_of(10, 4, &[]);
        let new_shard = shard_of(11, 4, &[old]);
        for order in [
            [(old, &old_shard), (new, &new_shard)],
            [(new, &new_shard), (old, &old_shard)],
        ] {
            let mut index = Index::default();
            index.set_supersedes_trust(SupersedesTrust::Only(HashSet::from([new])));
            for (id, shard) in order {
                index.add_shard(id, shard).unwrap();
            }
            assert_eq!(index.get(&[4; 32], 1).unwrap().pack_id, [11; 32]);
        }
    }

    #[test]
    fn index_locations_keeps_superseded_copy_as_last_resort() {
        let (legit, forged) = ([1u8; 32], [2u8; 32]);
        let legit_shard = shard_of(10, 4, &[]);
        let forged_shard = shifted_shard(11, 4, PACK_PREFIX_LEN as u32 + 300);
        let forged_shard = pb::IndexShard {
            supersedes: vec![legit.to_vec()],
            ..forged_shard
        };
        for order in [
            [(legit, &legit_shard), (forged, &forged_shard)],
            [(forged, &forged_shard), (legit, &legit_shard)],
        ] {
            let mut index = Index::default();
            index.set_supersedes_trust(SupersedesTrust::Only(HashSet::from([forged])));
            for (id, shard) in order {
                index.add_shard(id, shard).unwrap();
            }
            assert_eq!(index.get(&[4; 32], 1).unwrap().pack_id, [11; 32]);
            let packs: Vec<BlobId> = index
                .locations(&[4; 32], 1)
                .iter()
                .map(|l| l.pack_id)
                .collect();
            assert_eq!(packs, vec![[11u8; 32], [10u8; 32]]);
        }
    }

    #[test]
    fn index_locations_lists_chosen_first() {
        let mut index = Index::default();
        index.add_shard([1; 32], &shard_of(10, 4, &[])).unwrap();
        index.add_shard([2; 32], &shard_of(11, 4, &[])).unwrap();
        index.add_shard([3; 32], &shard_of(12, 4, &[])).unwrap();
        let chosen = *index.get(&[4; 32], 1).unwrap();
        let all = index.locations(&[4; 32], 1);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0], chosen);
        assert!(index.locations(&[7; 32], 1).is_empty());
        assert!(index.locations(&[4; 32], 2).is_empty());
    }

    #[test]
    fn index_locations_keeps_same_pack_copy_at_other_offset() {
        let (planted, legit) = ([1u8; 32], [2u8; 32]);
        let planted_shard = shifted_shard(10, 4, PACK_PREFIX_LEN as u32 + 300);
        let legit_shard = shifted_shard(10, 4, PACK_PREFIX_LEN as u32);
        let mut index = Index::default();
        index.set_supersedes_trust(SupersedesTrust::Only(HashSet::new()));
        index.add_shard(planted, &planted_shard).unwrap();
        index.add_shard(legit, &legit_shard).unwrap();
        assert_eq!(
            index.get(&[4; 32], 1).unwrap().offset,
            PACK_PREFIX_LEN as u32 + 300
        );
        let offsets: Vec<u32> = index
            .locations(&[4; 32], 1)
            .iter()
            .map(|l| l.offset)
            .collect();
        assert_eq!(
            offsets,
            [PACK_PREFIX_LEN as u32 + 300, PACK_PREFIX_LEN as u32]
        );

        index.add_shard(legit, &legit_shard).unwrap();
        assert_eq!(index.locations(&[4; 32], 1).len(), 2);
    }
}
