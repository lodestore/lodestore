//! Index shards: immutable, AEAD-sealed lists of the blobs of one or more
//! packs. The in-memory index maps a blob id to its location.

use std::collections::{HashMap, HashSet};

use prost::Message;

use crate::crypto::{
    BlobId, Entropy, HPKE_ENC_LEN, RepoId, SEALED_KEY_LEN, open_index_shard, seal_index_shard,
};
use crate::error::{Error, Result};
use crate::keys::ReadKeys;
use crate::pack::{
    ENTRY_LEN, PACK_PREFIX_LEN, frame_len_in_range, max_ulen, pack_object_key, parse_entries,
    read_pack_header,
};
use crate::pb;
use crate::store::{BlobError, BlobStore, put_named};

const INDEX_PREFIX: &str = "index/";
const PACKS_PREFIX: &str = "packs/";
/// Pack records per shard written by a rebuild.
const REBUILD_SHARD_PACKS: usize = 1024;

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
    let shard = pb::IndexShard {
        packs,
        supersedes: Vec::new(),
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
#[derive(Default)]
pub struct Index {
    map: HashMap<(BlobId, u8), BlobLocation>,
    shards: HashSet<BlobId>,
    packs: HashMap<BlobId, u64>,
    skipped: Vec<(String, String)>,
}

impl Index {
    /// A data chunk and a tree blob with equal plaintext share an id, so the
    /// type is part of the key.
    pub fn get(&self, id: &BlobId, blob_type: u8) -> Option<&BlobLocation> {
        self.map.get(&(*id, blob_type))
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

    pub fn add_shard(&mut self, shard_id: BlobId, shard: &pb::IndexShard) -> Result<()> {
        let bad = |reason: &str| Error::corrupt(index_object_key(&shard_id), reason.to_owned());
        let mut staged = Vec::new();
        let mut pack_sizes = Vec::new();
        for pack in &shard.packs {
            let pack_id: BlobId = pack
                .pack_id
                .as_slice()
                .try_into()
                .map_err(|_| bad("pack id must be 32 bytes"))?;
            pack_sizes.push((pack_id, pack.pack_size));
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
        for (id, loc) in staged {
            self.map.entry((id, loc.blob_type)).or_insert(loc);
        }
        self.packs.extend(pack_sizes);
        self.shards.insert(shard_id);
        Ok(())
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
        let mut listed = HashMap::new();
        for entry in store.list(PACKS_PREFIX).await? {
            let id = entry.key.rsplit('/').next().and_then(parse_hex32);
            if let Some(id) = id.filter(|id| pack_object_key(id) == entry.key) {
                listed.insert(id, entry.size);
            }
        }
        let missing: HashSet<BlobId> = self
            .packs
            .iter()
            .filter(|(id, size)| listed.get(*id) != Some(size))
            .map(|(id, _)| *id)
            .collect();
        self.map.retain(|_, loc| !missing.contains(&loc.pack_id));
        self.packs.retain(|id, _| !missing.contains(id));
        Ok(missing.len())
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

/// Re-creates index entries for every pack that no shard mentions, reading
/// only each pack's prefix, trailer and header. Packs are not hashed: a pack
/// whose bytes differ from its name is only noticed when a blob of it is read.
///
/// ponytail: packs are read one after another, three ranged GETs each; run them
/// concurrently if rebuilding a very large bucket gets slow.
pub async fn rebuild_index<S: BlobStore>(
    store: &S,
    keys: &ReadKeys,
    progress: &dyn Fn(u64, u64),
) -> Result<RebuildReport> {
    let mut index = Index::default();
    index.load_all(store, keys.k_idx(), keys.repo_id()).await?;

    let mut todo = Vec::new();
    for entry in store.list(PACKS_PREFIX).await? {
        let pack_id = entry.key.rsplit('/').next().and_then(parse_hex32);
        if let Some(id) = pack_id.filter(|id| pack_object_key(id) == entry.key)
            && !index.has_pack(&id)
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
}
