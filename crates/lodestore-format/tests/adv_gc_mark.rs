//! A host with writer credentials plants index shards; the collector's view
//! of what is live must not change.

mod adv_support;
mod gc_support;

use adv_support::{attacker_keys, forge_offsets, open_all_shards, plant_shard};
use gc_support::Fx;

#[tokio::test]
async fn adv_planted_supersedes_does_not_stop_gc_survey() {
    let fx = Fx::new().await;
    fx.backup("host-a", &[("a", 1, 200 << 10), ("shared", 3, 100 << 10)])
        .await;
    fx.backup("host-b", &[("b", 2, 200 << 10), ("shared", 3, 100 << 10)])
        .await;
    let before = fx.survey(0).await;

    let keys = attacker_keys(&fx.repo);
    let shards = open_all_shards(&fx.store, &keys).await;
    let records = shards
        .iter()
        .flat_map(|(_, shard)| shard.packs.iter().map(forge_offsets))
        .collect();
    let supersedes = shards.iter().map(|(id, _)| *id).collect();
    plant_shard(&fx.store, &keys, records, supersedes).await;

    let after = fx.survey(0).await;
    assert_eq!(after.live.len(), before.live.len());
    for (id, blob_type) in before.live.iter() {
        assert!(after.live.contains(id, *blob_type));
    }
    assert_eq!(after.snapshots, before.snapshots);
}
