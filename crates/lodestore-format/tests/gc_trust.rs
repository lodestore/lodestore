//! Compaction and index rebuild keep the line between what the server vouches
//! for and what a host with writer credentials could have written.

mod adv_support;
mod gc_support;

use std::collections::{BTreeSet, HashSet};

use adv_support::{attacker_keys, forge_offsets, open_all_shards, plant_shard_where};
use gc_support::Fx;
use lodestore_format::attest::{
    MARKER_LEGACY_SHARDS, peek_attestations, put_attestation, read_attestations,
};
use lodestore_format::crypto::{BlobId, Entropy};
use lodestore_format::gc::compact::{CompactPlan, Compacted, compact_plan, write_compacted};
use lodestore_format::gc::mark::load_shards;
use lodestore_format::gc::sweep::sweep_shards;
use lodestore_format::index::{index_object_key, rebuild_index};
use lodestore_format::pb;
use lodestore_format::store::BlobDelete;
use prost::Message;

async fn shard_records(fx: &Fx, ids: &[BlobId]) -> Vec<pb::PackRecord> {
    let shards = load_shards(&fx.store, fx.repo.read_keys()).await.unwrap();
    ids.iter()
        .flat_map(|id| {
            let (_, shard) = shards
                .shards
                .iter()
                .find(|(s, _)| s == id)
                .expect("the shard is in the bucket");
            shard.packs.clone()
        })
        .collect()
}

async fn pack_ids(fx: &Fx, ids: &[BlobId]) -> BTreeSet<Vec<u8>> {
    shard_records(fx, ids)
        .await
        .into_iter()
        .map(|r| r.pack_id)
        .collect()
}

async fn compact(fx: &Fx, verified: &[BlobId], attested: &[BlobId]) -> (CompactPlan, Compacted) {
    let keys = fx.repo.read_keys();
    let mut shards = load_shards(&fx.store, keys).await.unwrap();
    shards.verified = verified.iter().copied().collect();
    shards.attested = attested.iter().copied().collect();
    let plan = compact_plan(&shards, &HashSet::new());
    let done = write_compacted(
        &fx.store,
        keys,
        &plan,
        &Default::default(),
        &HashSet::new(),
        &mut Entropy::os(),
    )
    .await
    .unwrap();
    (plan, done)
}

async fn attest(fx: &Fx, shards: &[BlobId]) {
    put_attestation(
        &fx.store,
        fx.repo.read_keys(),
        shards,
        &[MARKER_LEGACY_SHARDS],
        Vec::new(),
        &mut Entropy::os(),
    )
    .await
    .unwrap();
}

async fn real_shards(fx: &Fx) -> Vec<BlobId> {
    let keys = attacker_keys(&fx.repo);
    let mut ids: Vec<_> = open_all_shards(&fx.store, &keys)
        .await
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    ids.sort();
    ids
}

/// Two hosts, their shards attested, and an unattested shard of the same packs
/// with every offset moved one byte, sorted before every real shard.
async fn forged() -> (Fx, Vec<BlobId>, BlobId, Vec<pb::PackRecord>) {
    let fx = Fx::new().await;
    fx.backup("a", &[("a", 1, 300 << 10)]).await;
    fx.backup("b", &[("b", 2, 200 << 10)]).await;
    let real = real_shards(&fx).await;
    attest(&fx, &real).await;
    let keys = attacker_keys(&fx.repo);
    let forged: Vec<_> = open_all_shards(&fx.store, &keys)
        .await
        .iter()
        .flat_map(|(_, s)| s.packs.iter().map(forge_offsets))
        .collect();
    let lowest = real[0];
    let planted = plant_shard_where(&fx.store, &keys, forged.clone(), Vec::new(), |id| {
        *id < lowest
    })
    .await;
    (fx, real, planted, forged)
}

#[tokio::test]
async fn gc_trust_compaction_keeps_legit_record_next_to_forged() {
    let (fx, real, planted, _) = forged().await;
    let (plan, _) = compact(&fx, &[], &real).await;
    assert!(plan.rewrite.contains(&planted));
    sweep_shards(&fx.store, &plan.rewrite).await;
    fx.assert_all_restore().await;
}

#[tokio::test]
async fn gc_trust_compaction_does_not_attest_forged_records() {
    let (fx, real, _, forged) = forged().await;
    let (_, done) = compact(&fx, &[], &real).await;
    assert!(done.verified.is_empty());
    assert!(!done.other.is_empty());
    let forged: HashSet<_> = forged.iter().map(|r| r.encode_to_vec()).collect();
    let attested = read_attestations(&fx.store, fx.repo.read_keys())
        .await
        .unwrap()
        .shards;
    for id in done.verified.iter().chain(&done.attested) {
        assert!(attested.contains(id));
        for record in shard_records(&fx, &[*id]).await {
            assert!(!forged.contains(&record.encode_to_vec()));
        }
    }
    for id in &done.other {
        assert!(!attested.contains(id));
    }
}

#[tokio::test]
async fn gc_trust_compaction_attests_trusted_shards() {
    let fx = Fx::new().await;
    fx.backup("a", &[("a", 1, 300 << 10)]).await;
    fx.backup("b", &[("b", 2, 200 << 10)]).await;
    fx.backup("c", &[("c", 3, 100 << 10)]).await;
    let real = real_shards(&fx).await;
    let (plan, done) = compact(&fx, &real[..1], &real[..2]).await;
    assert_eq!(plan.record_count(), shard_records(&fx, &real).await.len());
    let attested = read_attestations(&fx.store, fx.repo.read_keys())
        .await
        .unwrap()
        .shards;
    let seen = peek_attestations(&fx.store).await.unwrap().shards;
    let superseding = done.superseding.expect("a commit shard");
    assert!(!done.verified.is_empty() && !done.attested.is_empty());
    for id in done
        .verified
        .iter()
        .chain(&done.attested)
        .chain([&superseding])
    {
        assert!(attested.contains(id));
        assert!(seen.contains(id));
    }
    for id in &done.other {
        assert!(!attested.contains(id));
    }
}

#[tokio::test]
async fn gc_trust_compaction_keeps_unverified_records_apart() {
    let fx = Fx::new().await;
    fx.backup("a", &[("a", 1, 300 << 10)]).await;
    fx.backup("l", &[("l", 2, 200 << 10)]).await;
    fx.backup("p", &[("p", 3, 100 << 10)]).await;
    let real = real_shards(&fx).await;
    assert_eq!(real.len(), 3);
    let (a, l, p) = (real[0], real[1], real[2]);
    let (_, done) = compact(&fx, &[a], &[a, l]).await;
    assert_eq!(
        pack_ids(&fx, &done.verified).await,
        pack_ids(&fx, &[a]).await
    );
    assert_eq!(
        pack_ids(&fx, &done.attested).await,
        pack_ids(&fx, &[l]).await
    );
    assert_eq!(pack_ids(&fx, &done.other).await, pack_ids(&fx, &[p]).await);
}

async fn lose_index(fx: &Fx, shards: &[BlobId]) {
    for id in shards {
        fx.store.delete(&index_object_key(id)).await.unwrap();
    }
}

#[tokio::test]
async fn gc_trust_rebuild_reindexes_pack_named_only_by_planted_shard() {
    let fx = Fx::new().await;
    fx.backup("a", &[("a", 1, 300 << 10)]).await;
    fx.backup("b", &[("b", 2, 200 << 10)]).await;
    let real = real_shards(&fx).await;
    attest(&fx, &real).await;
    let keys = attacker_keys(&fx.repo);
    let forged: Vec<_> = open_all_shards(&fx.store, &keys)
        .await
        .iter()
        .flat_map(|(_, s)| s.packs.iter().map(forge_offsets))
        .collect();
    plant_shard_where(&fx.store, &keys, forged, Vec::new(), |_| true).await;
    lose_index(&fx, &real).await;

    let report = rebuild_index(&fx.store, fx.repo.read_keys(), &|_, _| {})
        .await
        .unwrap();
    assert!(report.packs_indexed > 0);
    assert_eq!(report.packs_indexed, report.packs_scanned);
    fx.assert_all_restore().await;
}

#[tokio::test]
async fn gc_trust_rebuild_does_not_attest() {
    let fx = Fx::new().await;
    fx.backup("a", &[("a", 1, 300 << 10)]).await;
    let real = real_shards(&fx).await;
    attest(&fx, &real).await;
    lose_index(&fx, &real).await;

    let report = rebuild_index(&fx.store, fx.repo.read_keys(), &|_, _| {})
        .await
        .unwrap();
    assert!(report.shards_written > 0);
    let attested = read_attestations(&fx.store, fx.repo.read_keys())
        .await
        .unwrap()
        .shards;
    let rebuilt = real_shards(&fx).await;
    assert!(!rebuilt.is_empty());
    for id in rebuilt {
        assert!(!attested.contains(&id));
    }
}
