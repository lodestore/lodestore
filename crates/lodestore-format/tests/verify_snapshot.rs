//! The server proves a new snapshot by opening frames: a shard is believed
//! only after `verify_shard`, and a snapshot resolves only through such shards.

mod adv_support;
mod gc_support;

use std::collections::HashSet;
use std::fs;
use std::ops::Range;

use adv_support::{
    attacker_keys, forge_offsets, open_all_shards, plant_blobs, plant_shard, plant_shard_where,
};
use bytes::Bytes;
use gc_support::Fx;
use lodestore_format::Error;
use lodestore_format::check::is_data_error;
use lodestore_format::chunker::blob_id;
use lodestore_format::crypto::{BLOB_TYPE_DATA, BlobId};
use lodestore_format::pack::{pack_object_key, parse_entries};
use lodestore_format::pb;
use lodestore_format::snapshot::snapshot_object_key;
use lodestore_format::store::{BlobError, BlobStore, ListEntry, LocalFsStore, put_named};
use lodestore_format::tree::encode_tree;
use lodestore_format::verify_snapshot::{
    check_snapshot_resolves, snapshot_index_refs, verify_shard,
};

const FILE: &[(&str, u8, usize)] = &[("f", 1, 1000)];

struct Refusing<'a> {
    inner: &'a LocalFsStore,
    key: String,
}

impl BlobStore for Refusing<'_> {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<(), BlobError> {
        self.inner.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        if key == self.key {
            return Err(BlobError::Denied {
                message: "no".into(),
            });
        }
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, BlobError> {
        if key == self.key {
            return Err(BlobError::Denied {
                message: "no".into(),
            });
        }
        self.inner.get_range(key, range).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        self.inner.list(prefix).await
    }
}

async fn refs(fx: &Fx, snapshot: &BlobId) -> Vec<BlobId> {
    snapshot_index_refs(&fx.store, fx.repo.read_keys(), snapshot)
        .await
        .unwrap()
}

async fn honest_shard(fx: &Fx) -> (BlobId, pb::IndexShard) {
    fx.backup("a", FILE).await;
    let keys = attacker_keys(&fx.repo);
    open_all_shards(&fx.store, &keys)
        .await
        .into_iter()
        .find(|(_, s)| !s.packs.is_empty())
        .unwrap()
}

async fn verify_err(fx: &Fx, shard: &BlobId) -> Error {
    verify_shard(&fx.store, fx.repo.read_keys(), shard)
        .await
        .unwrap_err()
}

#[tokio::test]
async fn verify_shard_accepts_honest_shard() {
    let fx = Fx::new().await;
    let (id, shard) = honest_shard(&fx).await;
    let want: usize = shard
        .packs
        .iter()
        .map(|p| parse_entries(&p.entries).unwrap().len())
        .sum();
    let got = verify_shard(&fx.store, fx.repo.read_keys(), &id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.blobs as usize, want);
    assert_eq!(got.packs as usize, shard.packs.len());
    assert!(got.bytes_read > 0);
}

#[tokio::test]
async fn verify_shard_rejects_false_offset() {
    let fx = Fx::new().await;
    let (_, shard) = honest_shard(&fx).await;
    let keys = attacker_keys(&fx.repo);
    let forged = plant_shard(
        &fx.store,
        &keys,
        shard.packs.iter().map(forge_offsets).collect(),
        Vec::new(),
    )
    .await;
    assert!(is_data_error(&verify_err(&fx, &forged).await));
}

#[tokio::test]
async fn verify_shard_rejects_missing_pack() {
    let fx = Fx::new().await;
    let (_, shard) = honest_shard(&fx).await;
    let mut record = shard.packs[0].clone();
    record.pack_id = vec![7; 32];
    let keys = attacker_keys(&fx.repo);
    let forged = plant_shard(&fx.store, &keys, vec![record], Vec::new()).await;
    assert!(is_data_error(&verify_err(&fx, &forged).await));
}

#[tokio::test]
async fn verify_shard_rejects_size_mismatch() {
    let fx = Fx::new().await;
    let (_, shard) = honest_shard(&fx).await;
    let mut record = shard.packs[0].clone();
    record.pack_size += 1;
    let keys = attacker_keys(&fx.repo);
    let forged = plant_shard(&fx.store, &keys, vec![record], Vec::new()).await;
    assert!(is_data_error(&verify_err(&fx, &forged).await));
}

#[tokio::test]
async fn verify_shard_rejects_foreign_key_material() {
    let fx = Fx::new().await;
    let (_, shard) = honest_shard(&fx).await;
    let keys = attacker_keys(&fx.repo);
    let mut record = shard.packs[0].clone();
    record.hpke_enc[0] ^= 1;
    let forged = plant_shard(&fx.store, &keys, vec![record], Vec::new()).await;
    assert!(is_data_error(&verify_err(&fx, &forged).await));
    let mut record = shard.packs[0].clone();
    record.sealed_pack_key[0] ^= 1;
    let forged = plant_shard(&fx.store, &keys, vec![record], Vec::new()).await;
    assert!(is_data_error(&verify_err(&fx, &forged).await));
}

#[tokio::test]
async fn verify_shard_missing_shard_is_none() {
    let fx = Fx::new().await;
    fx.backup("a", FILE).await;
    let got = verify_shard(&fx.store, fx.repo.read_keys(), &[3; 32])
        .await
        .unwrap();
    assert!(got.is_none());
}

#[tokio::test]
async fn verify_shard_store_error_is_not_data_error() {
    let fx = Fx::new().await;
    let (id, shard) = honest_shard(&fx).await;
    let pack: BlobId = shard.packs[0].pack_id.as_slice().try_into().unwrap();
    let store = Refusing {
        inner: &fx.store,
        key: pack_object_key(&pack),
    };
    let err = verify_shard(&store, fx.repo.read_keys(), &id)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Store(BlobError::Denied { .. })));
    assert!(!is_data_error(&err));
}

fn set(ids: impl IntoIterator<Item = BlobId>) -> HashSet<BlobId> {
    ids.into_iter().collect()
}

async fn resolves(
    fx: &Fx,
    snapshot: &BlobId,
    verified: &HashSet<BlobId>,
    attested: &HashSet<BlobId>,
) -> Result<lodestore_format::verify_snapshot::SnapshotCheck, Error> {
    check_snapshot_resolves(&fx.store, fx.repo.read_keys(), snapshot, verified, attested).await
}

/// A shard that claims the data blob of `FILE` in a junk pack, planted before
/// the snapshot of that file is written, so the writer deduplicates against it.
async fn snapshot_over_forged_shard(fx: &Fx) -> (BlobId, BlobId) {
    let src = fx.make_src("f", FILE);
    let content = fs::read(src.join("f")).unwrap();
    let id = blob_id(fx.repo.read_keys().k_id(), &content);
    let keys = attacker_keys(&fx.repo);
    let forged = plant_blobs(&fx.store, &keys, &[(id, BLOB_TYPE_DATA)]).await;
    let mut w = fx.open_writer().await;
    let out = fx.commit(&mut w, src).await;
    let named = refs(fx, &out.snapshot_id).await;
    assert!(named.contains(&forged), "the writer did not deduplicate");
    (out.snapshot_id, forged)
}

#[tokio::test]
async fn verify_snapshot_honest_resolves() {
    let fx = Fx::new().await;
    let out = fx.backup("a", FILE).await;
    let named = refs(&fx, &out.snapshot_id).await;
    let got = resolves(&fx, &out.snapshot_id, &set(named), &HashSet::new())
        .await
        .unwrap();
    assert!(got.trees >= 1);
    assert!(got.data_blobs >= 1);
    assert!(got.newly_verified.is_empty());
}

#[tokio::test]
async fn verify_snapshot_checks_attested_ref_before_trusting_it() {
    let fx = Fx::new().await;
    let a = fx.backup("a", FILE).await;
    let shard_a = refs(&fx, &a.snapshot_id).await;
    let b = fx.backup("b", FILE).await;
    let named = refs(&fx, &b.snapshot_id).await;
    assert!(shard_a.iter().all(|s| named.contains(s)));
    let own = set(named.into_iter().filter(|s| !shard_a.contains(s)));
    let got = resolves(&fx, &b.snapshot_id, &own, &set(shard_a.clone()))
        .await
        .unwrap();
    assert_eq!(set(got.newly_verified), set(shard_a));
}

#[tokio::test]
async fn verify_snapshot_rejects_forged_shard_attested_at_upgrade() {
    let fx = Fx::new().await;
    let (snapshot, forged) = snapshot_over_forged_shard(&fx).await;
    let own = set(refs(&fx, &snapshot)
        .await
        .into_iter()
        .filter(|s| *s != forged));
    let err = resolves(&fx, &snapshot, &own, &set([forged]))
        .await
        .unwrap_err();
    assert!(is_data_error(&err), "{err}");
    let trusting = set(refs(&fx, &snapshot).await);
    resolves(&fx, &snapshot, &trusting, &HashSet::new())
        .await
        .expect("only opening the frames of the shard rejects it");
}

/// A snapshot whose data blob is only in the shards of an earlier backup:
/// `(snapshot, its own shards, the earlier shards)`.
async fn snapshot_over_earlier_shards(fx: &Fx) -> (BlobId, Vec<BlobId>, Vec<BlobId>) {
    let a = fx.backup("a", FILE).await;
    let shard_a = refs(fx, &a.snapshot_id).await;
    let content = fs::read(fx.tmp.path().join("src-a/f")).unwrap();
    let data = blob_id(fx.repo.read_keys().k_id(), &content);
    let mut w = fx.open_writer().await;
    let tree = w
        .put_tree_raw(&encode_tree(vec![pb::Node {
            name: b"f".to_vec(),
            r#type: pb::NodeType::File as i32,
            mode: 0o644,
            content: vec![data.to_vec()],
            ..Default::default()
        }]))
        .unwrap();
    let raw = w.commit_snapshot_raw(tree, Vec::new()).await.unwrap();
    let named = refs(fx, &raw).await;
    assert!(named.iter().all(|s| !shard_a.contains(s)));
    (raw, named, shard_a)
}

#[tokio::test]
async fn verify_snapshot_verifies_attested_fallback() {
    let fx = Fx::new().await;
    let (raw, named, shard_a) = snapshot_over_earlier_shards(&fx).await;
    let got = resolves(&fx, &raw, &set(named), &set(shard_a.clone()))
        .await
        .unwrap();
    assert_eq!(set(got.newly_verified), set(shard_a));
}

#[tokio::test]
async fn verify_snapshot_skips_attested_holder_that_fails() {
    let fx = Fx::new().await;
    let (raw, named, shard_a) = snapshot_over_earlier_shards(&fx).await;
    let keys = attacker_keys(&fx.repo);
    let records: Vec<pb::PackRecord> = open_all_shards(&fx.store, &keys)
        .await
        .into_iter()
        .filter(|(id, _)| shard_a.contains(id))
        .flat_map(|(_, shard)| shard.packs)
        .collect();
    let missing = records
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, mut r)| {
            r.pack_id = vec![200 + i as u8; 32];
            r
        })
        .collect();
    let lowest = *shard_a.iter().min().unwrap();
    let bad = plant_shard_where(&fx.store, &keys, missing, Vec::new(), |id| *id < lowest).await;
    let mut truncated = records;
    for r in &mut truncated {
        r.entries.pop();
    }
    let malformed = plant_shard(&fx.store, &keys, truncated, Vec::new()).await;

    let mut attested = set(shard_a.clone());
    attested.extend([bad, malformed]);
    let got = resolves(&fx, &raw, &set(named), &attested).await.unwrap();
    assert_eq!(set(got.newly_verified), set(shard_a));
}

#[tokio::test]
async fn verify_snapshot_rejects_blob_only_in_unverified_foreign_shard() {
    let fx = Fx::new().await;
    let (snapshot, forged) = snapshot_over_forged_shard(&fx).await;
    let own = set(refs(&fx, &snapshot)
        .await
        .into_iter()
        .filter(|s| *s != forged));
    let err = resolves(&fx, &snapshot, &own, &HashSet::new())
        .await
        .unwrap_err();
    assert!(is_data_error(&err), "{err}");
}

#[tokio::test]
async fn verify_snapshot_rejects_tree_with_false_location() {
    let fx = Fx::new().await;
    let (_, shard) = honest_shard(&fx).await;
    let snapshot = fx.sources.borrow().keys().next().copied().unwrap();
    let keys = attacker_keys(&fx.repo);
    let forged = plant_shard(
        &fx.store,
        &keys,
        shard.packs.iter().map(forge_offsets).collect(),
        Vec::new(),
    )
    .await;
    let err = resolves(&fx, &snapshot, &set([forged]), &HashSet::new())
        .await
        .unwrap_err();
    assert!(is_data_error(&err), "{err}");
}

#[tokio::test]
async fn verify_snapshot_rejects_renamed_object() {
    let fx = Fx::new().await;
    let out = fx.backup("a", FILE).await;
    let bytes = fs::read(
        fx.tmp
            .path()
            .join("repo")
            .join(snapshot_object_key(&out.snapshot_id)),
    )
    .unwrap();
    let fake: BlobId = [9; 32];
    put_named(&fx.store, &snapshot_object_key(&fake), bytes)
        .await
        .unwrap();
    let err = resolves(&fx, &fake, &HashSet::new(), &HashSet::new())
        .await
        .unwrap_err();
    assert!(is_data_error(&err), "{err}");
}
