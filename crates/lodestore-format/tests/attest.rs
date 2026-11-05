//! Server attestations: written and verified only by a holder of the master
//! key, never forgeable with writer keys, invisible to the 0.1 lock readers.

use std::ops::Range;

use bytes::Bytes;
use lodestore_format::attest::{
    ATTEST_FRAME_V1, ATTEST_PREFIX, MARKER_LEGACY_SHARDS, MAX_ATTEST_SIZE, attest_key,
    open_attestation, open_provenance, peek_attestation, peek_attestations, put_attestation,
    read_attestations, seal_attestation, seal_provenance,
};
use lodestore_format::crypto::{Entropy, attest_mac};
use lodestore_format::keys::{MasterKey, ReadKeys};
use lodestore_format::locks::{new_lock_id, read_gc_locks, read_writer_locks};
use lodestore_format::pb;
use lodestore_format::store::{BlobError, BlobStore, ListEntry, LocalFsStore};
use prost::Message;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{Rng, SeedableRng};

fn keys() -> ReadKeys {
    ReadKeys::derive(MasterKey::from_bytes([1u8; 32]), [2u8; 16])
}

fn entropy(seed: u8) -> Entropy {
    Entropy::from_rng(ChaCha20Rng::from_seed([seed; 32]))
}

fn store() -> (tempfile::TempDir, LocalFsStore) {
    let tmp = tempfile::tempdir().unwrap();
    let store = LocalFsStore::new(tmp.path().join("repo"));
    (tmp, store)
}

/// An attestation framed correctly but authenticated under `mac_key`.
fn framed_under(mac_key: &[u8; 32], keys: &ReadKeys, shard: u8, seed: u8) -> (String, Vec<u8>) {
    let id = new_lock_id(&mut entropy(seed));
    let attestation = pb::Attestation {
        attest_id: id.to_vec(),
        shard_ids: vec![vec![shard; 32]],
        ..Default::default()
    };
    let body = attestation.encode_to_vec();
    let mut bytes = vec![ATTEST_FRAME_V1];
    bytes.extend_from_slice(&attest_mac(mac_key, keys.repo_id(), ATTEST_FRAME_V1, &body));
    bytes.extend_from_slice(&body);
    (attest_key(&id), bytes)
}

#[tokio::test]
async fn attest_round_trip_on_local_store() {
    let (_tmp, store) = store();
    let keys = keys();
    let key = put_attestation(
        &store,
        &keys,
        &[[9u8; 32], [3u8; 32], [9u8; 32]],
        &[MARKER_LEGACY_SHARDS],
        vec![b"sealed".to_vec()],
        &mut entropy(5),
    )
    .await
    .unwrap();
    assert!(key.starts_with("locks/attest-"));

    let set = read_attestations(&store, &keys).await.unwrap();
    assert_eq!(set.shards, [[3u8; 32], [9u8; 32]].into_iter().collect());
    assert_eq!(set.markers, [MARKER_LEGACY_SHARDS].into_iter().collect());
    assert_eq!(set.sealed_provenance, vec![b"sealed".to_vec()]);
    assert_eq!(set.objects, vec![key]);
    assert!(set.rejected.is_empty());
    assert!(!set.incomplete);
}

#[tokio::test]
async fn attest_writer_keys_cannot_forge() {
    let (_tmp, store) = store();
    let keys = keys();
    let writer = keys.write_keys([0u8; 32]);
    for (seed, mac_key) in [(1u8, &*writer.k_idx), (2, &*writer.k_id)] {
        let (key, bytes) = framed_under(mac_key, &keys, 7, seed);
        store.put_if_absent(&key, Bytes::from(bytes)).await.unwrap();
    }
    let set = read_attestations(&store, &keys).await.unwrap();
    assert!(set.shards.is_empty());
    assert!(set.objects.is_empty());
    assert_eq!(set.rejected.len(), 2);
    assert!(
        set.rejected
            .iter()
            .all(|(k, _)| k.starts_with(ATTEST_PREFIX))
    );

    let (key, bytes) = framed_under(keys.k_srv(), &keys, 7, 3);
    store.put_if_absent(&key, Bytes::from(bytes)).await.unwrap();
    let set = read_attestations(&store, &keys).await.unwrap();
    assert_eq!(set.shards, [[7u8; 32]].into_iter().collect());
}

#[test]
fn attest_write_keys_lack_server_key() {
    let keys = keys();
    let writer = keys.write_keys([0u8; 32]);
    let k_srv = *keys.k_srv();
    assert_ne!(k_srv, *writer.k_id);
    assert_ne!(k_srv, *writer.k_idx);
    assert_ne!(k_srv, *keys.k_id());
    assert_ne!(k_srv, *keys.k_idx());

    let text = writer.to_file_text();
    let encoded = data_encoding::BASE64
        .decode(text.lines().nth(1).unwrap().as_bytes())
        .unwrap();
    assert!(!encoded.windows(32).any(|w| w == k_srv));
    assert!(!text.contains(&data_encoding::BASE64.encode(&k_srv)));
    assert!(!text.contains(&data_encoding::HEXLOWER.encode(&k_srv)));
}

#[tokio::test]
async fn attest_other_lock_readers_ignore_attestations() {
    let (_tmp, store) = store();
    let keys = keys();
    let before_gc = read_gc_locks(&store, keys.k_idx(), keys.repo_id())
        .await
        .unwrap();
    let before_writers = read_writer_locks(&store, keys.k_idx(), keys.repo_id())
        .await
        .unwrap();

    put_attestation(
        &store,
        &keys,
        &[[1u8; 32]],
        &[],
        Vec::new(),
        &mut entropy(8),
    )
    .await
    .unwrap();

    let gc = read_gc_locks(&store, keys.k_idx(), keys.repo_id())
        .await
        .unwrap();
    let writers = read_writer_locks(&store, keys.k_idx(), keys.repo_id())
        .await
        .unwrap();
    assert_eq!(gc.manifests, before_gc.manifests);
    assert_eq!(gc.denied, before_gc.denied);
    assert_eq!(writers, before_writers);
}

fn random_bytes(rng: &mut ChaCha20Rng, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    rng.fill_bytes(&mut out);
    out
}

fn below(rng: &mut ChaCha20Rng, n: usize) -> usize {
    rng.next_u64() as usize % n
}

fn random_text(rng: &mut ChaCha20Rng) -> String {
    let len = below(rng, 65);
    (0..len)
        .filter_map(|_| char::from_u32(rng.next_u32() % 0x11_0000))
        .collect()
}

fn random_attestation(rng: &mut ChaCha20Rng) -> pb::Attestation {
    let shards = below(rng, 65);
    let markers = below(rng, 5);
    let provenance = below(rng, 4);
    pb::Attestation {
        attest_id: random_bytes(rng, 16),
        created: Some(pb::Timestamp {
            seconds: rng.next_u64() as i64,
            nanos: rng.next_u32(),
        }),
        shard_ids: (0..shards).map(|_| random_bytes(rng, 32)).collect(),
        markers: (0..markers).map(|_| rng.next_u32()).collect(),
        sealed_provenance: (0..provenance)
            .map(|_| {
                let len = below(rng, 257);
                random_bytes(rng, len)
            })
            .collect(),
    }
}

fn random_provenance(rng: &mut ChaCha20Rng) -> pb::ProvenanceRecord {
    pb::ProvenanceRecord {
        snapshot_id: random_bytes(rng, 32),
        host_id: random_text(rng),
        hostname: random_text(rng),
        job_id: random_text(rng),
        verified_at: Some(pb::Timestamp {
            seconds: rng.next_u64() as i64,
            nanos: rng.next_u32(),
        }),
    }
}

fn mutate(rng: &mut ChaCha20Rng, valid: &[u8]) -> Vec<u8> {
    match below(rng, 4) {
        0 => {
            let mut out = valid.to_vec();
            let bit = below(rng, valid.len() * 8);
            out[bit / 8] ^= 1 << (bit % 8);
            out
        }
        1 => valid[..below(rng, valid.len())].to_vec(),
        2 => {
            let mut out = valid.to_vec();
            let extra = 1 + below(rng, 64);
            out.extend(random_bytes(rng, extra));
            out
        }
        _ => {
            let len = below(rng, 300);
            random_bytes(rng, len)
        }
    }
}

#[test]
fn attest_round_trip_generated_inputs() {
    let keys = keys();
    let mut rng = ChaCha20Rng::from_seed([21; 32]);
    let mut entropy = entropy(22);
    for _ in 0..500 {
        let attestation = random_attestation(&mut rng);
        let id: [u8; 16] = attestation.attest_id.as_slice().try_into().unwrap();
        let key = attest_key(&id);
        let sealed = seal_attestation(keys.k_srv(), keys.repo_id(), &attestation);
        assert_eq!(
            open_attestation(keys.k_srv(), keys.repo_id(), &key, &sealed).unwrap(),
            attestation
        );
        assert_eq!(peek_attestation(&key, &sealed).unwrap(), attestation);

        let record = random_provenance(&mut rng);
        let sealed = seal_provenance(&keys, &record, &mut entropy).unwrap();
        assert_eq!(open_provenance(&keys, &sealed).unwrap(), record);
    }
}

#[test]
fn attest_hostile_inputs_never_panic() {
    let keys = keys();
    let mut rng = ChaCha20Rng::from_seed([23; 32]);
    let mut entropy = entropy(24);
    for _ in 0..2000 {
        let attestation = random_attestation(&mut rng);
        let id: [u8; 16] = attestation.attest_id.as_slice().try_into().unwrap();
        let key = attest_key(&id);
        let sealed = seal_attestation(keys.k_srv(), keys.repo_id(), &attestation);
        let hostile = mutate(&mut rng, &sealed);
        assert!(open_attestation(keys.k_srv(), keys.repo_id(), &key, &hostile).is_err());
        let _ = peek_attestation(&key, &hostile);

        let sealed = seal_provenance(&keys, &random_provenance(&mut rng), &mut entropy).unwrap();
        let hostile = mutate(&mut rng, &sealed);
        assert!(open_provenance(&keys, &hostile).is_err());
    }
}

#[tokio::test]
async fn attest_peek_reads_shards_without_key() {
    let (_tmp, store) = store();
    let keys = keys();
    put_attestation(
        &store,
        &keys,
        &[[1u8; 32]],
        &[],
        Vec::new(),
        &mut entropy(1),
    )
    .await
    .unwrap();
    let writer = keys.write_keys([0u8; 32]);
    let (key, bytes) = framed_under(&writer.k_idx, &keys, 2, 2);
    store.put_if_absent(&key, Bytes::from(bytes)).await.unwrap();

    let peeked = peek_attestations(&store).await.unwrap();
    assert_eq!(peeked.shards, [[1u8; 32], [2u8; 32]].into_iter().collect());
    let verified = read_attestations(&store, &keys).await.unwrap();
    assert_eq!(verified.shards, [[1u8; 32]].into_iter().collect());
    assert_eq!(verified.rejected.len(), 1);
}

struct Wrapped<S> {
    inner: S,
    deny_list: bool,
    inflate: u64,
}

impl<S: BlobStore> BlobStore for Wrapped<S> {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<(), BlobError> {
        self.inner.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        assert_eq!(self.inflate, 0, "an oversized object must not be fetched");
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.inner.get_range(key, range).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        if self.deny_list {
            return Err(BlobError::Denied {
                message: "AccessDenied".into(),
            });
        }
        let mut listed = self.inner.list(prefix).await?;
        for entry in &mut listed {
            entry.size += self.inflate;
        }
        Ok(listed)
    }
}

#[tokio::test]
async fn attest_peek_denied_is_empty() {
    let (_tmp, inner) = store();
    put_attestation(
        &inner,
        &keys(),
        &[[1u8; 32]],
        &[],
        Vec::new(),
        &mut entropy(1),
    )
    .await
    .unwrap();
    let store = Wrapped {
        inner,
        deny_list: true,
        inflate: 0,
    };
    let peeked = peek_attestations(&store).await.unwrap();
    assert!(peeked.shards.is_empty());
    assert!(peeked.markers.is_empty());
}

#[tokio::test]
async fn attest_oversized_listing_is_rejected_without_get() {
    let (_tmp, inner) = store();
    let keys = keys();
    put_attestation(
        &inner,
        &keys,
        &[[1u8; 32]],
        &[],
        Vec::new(),
        &mut entropy(1),
    )
    .await
    .unwrap();
    let store = Wrapped {
        inner,
        deny_list: false,
        inflate: MAX_ATTEST_SIZE,
    };
    let set = read_attestations(&store, &keys).await.unwrap();
    assert!(set.shards.is_empty());
    assert_eq!(set.rejected.len(), 1);
}

#[test]
fn attest_provenance_round_trip() {
    let keys = keys();
    let record = pb::ProvenanceRecord {
        snapshot_id: vec![5; 32],
        host_id: "host-1".into(),
        hostname: "db01".into(),
        job_id: "job-9".into(),
        verified_at: Some(pb::Timestamp {
            seconds: 1_700_000_000,
            nanos: 7,
        }),
    };
    let sealed = seal_provenance(&keys, &record, &mut entropy(3)).unwrap();
    assert_eq!(open_provenance(&keys, &sealed).unwrap(), record);

    let mut tampered = sealed.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 1;
    assert!(open_provenance(&keys, &tampered).is_err());

    let other = ReadKeys::derive(MasterKey::from_bytes([1u8; 32]), [9u8; 16]);
    assert!(open_provenance(&other, &sealed).is_err());

    let short = pb::ProvenanceRecord {
        snapshot_id: vec![5; 31],
        ..record
    };
    let sealed = seal_provenance(&keys, &short, &mut entropy(3)).unwrap();
    assert!(open_provenance(&keys, &sealed).is_err());
}
