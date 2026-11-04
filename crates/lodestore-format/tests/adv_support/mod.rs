#![allow(dead_code)]

//! What a compromised host can do to a bucket with writer credentials: read
//! the index shards (it holds `k_idx`) and plant objects of its own. Only the
//! public API of the first release is used, so the scenarios can run against
//! that release unchanged.

use lodestore_format::crypto::{BlobId, Entropy, HPKE_ENC_LEN, SEALED_KEY_LEN};
use lodestore_format::index::{index_object_key, open_shard, parse_hex32, seal_shard_superseding};
use lodestore_format::keys::WriteKeys;
use lodestore_format::pack::{ENTRY_LEN, PACK_PREFIX_LEN, pack_object_key, parse_entries};
use lodestore_format::pb;
use lodestore_format::repo::Repository;
use lodestore_format::store::{BlobStore, put_named};

pub fn attacker_keys(repo: &Repository) -> WriteKeys {
    repo.read_keys().write_keys(repo.config_hash())
}

/// Every shard in `index/` that opens, as the attacker reads them.
pub async fn open_all_shards<S: BlobStore>(
    store: &S,
    keys: &WriteKeys,
) -> Vec<(BlobId, pb::IndexShard)> {
    let mut out = Vec::new();
    for entry in store.list("index/").await.unwrap() {
        let id = entry
            .key
            .strip_prefix("index/")
            .and_then(parse_hex32)
            .expect("shard key");
        let bytes = store.get(&entry.key).await.unwrap();
        out.push((
            id,
            open_shard(&keys.k_idx, &keys.repo_id, &id, &bytes).unwrap(),
        ));
    }
    out
}

pub async fn plant_shard<S: BlobStore>(
    store: &S,
    keys: &WriteKeys,
    records: Vec<pb::PackRecord>,
    supersedes: Vec<BlobId>,
) -> BlobId {
    let (id, bytes) = seal_shard_superseding(
        &keys.k_idx,
        &keys.repo_id,
        records,
        supersedes,
        &mut Entropy::os(),
    )
    .unwrap();
    put_named(store, &index_object_key(&id), bytes)
        .await
        .unwrap();
    id
}

/// Seals again with fresh entropy until the shard id satisfies `accept`, so a
/// scenario controls where the shard falls in the sorted listing of `index/`.
pub async fn plant_shard_where<S: BlobStore>(
    store: &S,
    keys: &WriteKeys,
    records: Vec<pb::PackRecord>,
    supersedes: Vec<BlobId>,
    accept: impl Fn(&BlobId) -> bool,
) -> BlobId {
    let mut entropy = Entropy::os();
    for _ in 0..10_000 {
        let (id, bytes) = seal_shard_superseding(
            &keys.k_idx,
            &keys.repo_id,
            records.clone(),
            supersedes.clone(),
            &mut entropy,
        )
        .unwrap();
        if accept(&id) {
            put_named(store, &index_object_key(&id), bytes)
                .await
                .unwrap();
            return id;
        }
    }
    panic!("no shard id satisfied the predicate in 10000 attempts");
}

/// The same record with every blob placed one byte off its real frame: the
/// entry passes the checks made when a shard is loaded, but the frame it
/// points at does not open.
pub fn forge_offsets(record: &pb::PackRecord) -> pb::PackRecord {
    let mut forged = record.clone();
    let mut entries = Vec::with_capacity(record.entries.len());
    for e in parse_entries(&record.entries).unwrap() {
        let offset = if u64::from(e.offset) + u64::from(e.enc_len) < record.pack_size {
            e.offset + 1
        } else {
            e.offset - 1
        };
        assert!(offset as usize >= PACK_PREFIX_LEN);
        entries.extend_from_slice(&e.blob_id);
        entries.push(e.blob_type);
        entries.extend_from_slice(&offset.to_le_bytes());
        entries.extend_from_slice(&e.enc_len.to_le_bytes());
        entries.extend_from_slice(&e.ulen.to_le_bytes());
    }
    forged.entries = entries;
    forged
}

/// Random bytes under a valid pack name; no frame in it opens.
pub async fn put_junk_pack<S: BlobStore>(store: &S, size: usize) -> BlobId {
    let mut entropy = Entropy::os();
    let id: BlobId = entropy.bytes();
    let mut bytes = vec![0u8; size];
    entropy.fill(&mut bytes);
    put_named(store, &pack_object_key(&id), bytes)
        .await
        .unwrap();
    id
}

/// A junk pack and a shard that names `blobs` in it.
pub async fn plant_blobs<S: BlobStore>(
    store: &S,
    keys: &WriteKeys,
    blobs: &[(BlobId, u8)],
) -> BlobId {
    const FRAME: u32 = 64;
    let size = PACK_PREFIX_LEN + blobs.len() * FRAME as usize + 64;
    let pack_id = put_junk_pack(store, size).await;
    let mut sorted = blobs.to_vec();
    sorted.sort();
    let mut entries = Vec::with_capacity(sorted.len() * ENTRY_LEN);
    for (i, (blob_id, blob_type)) in sorted.iter().enumerate() {
        let offset = (PACK_PREFIX_LEN + i * FRAME as usize) as u32;
        entries.extend_from_slice(blob_id);
        entries.push(*blob_type);
        entries.extend_from_slice(&offset.to_le_bytes());
        entries.extend_from_slice(&FRAME.to_le_bytes());
        entries.extend_from_slice(&16u32.to_le_bytes());
    }
    let mut entropy = Entropy::os();
    let record = pb::PackRecord {
        pack_id: pack_id.to_vec(),
        hpke_enc: entropy_bytes(&mut entropy, HPKE_ENC_LEN),
        sealed_pack_key: entropy_bytes(&mut entropy, SEALED_KEY_LEN),
        pack_size: size as u64,
        entries,
    };
    plant_shard(store, keys, vec![record], Vec::new()).await
}

fn entropy_bytes(entropy: &mut Entropy, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    entropy.fill(&mut out);
    out
}
