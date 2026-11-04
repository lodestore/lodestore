//! Server attestations: written and verified only by a holder of the master
//! key, never forgeable with writer keys, invisible to the 0.1 lock readers.

use bytes::Bytes;
use lodestore_format::attest::{
    ATTEST_FRAME_V1, ATTEST_PREFIX, MARKER_LEGACY_SHARDS, attest_key, put_attestation,
    read_attestations,
};
use lodestore_format::crypto::{Entropy, attest_mac};
use lodestore_format::keys::{MasterKey, ReadKeys};
use lodestore_format::locks::{new_lock_id, read_gc_locks, read_writer_locks};
use lodestore_format::pb;
use lodestore_format::store::{BlobStore, LocalFsStore};
use prost::Message;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::SeedableRng;

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
