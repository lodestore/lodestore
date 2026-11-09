//! A host with writer credentials plants index shards; the collector's view
//! of what is live must not change.

mod adv_support;
mod gc_support;
#[path = "adv_support/planted.rs"]
mod planted;

use adv_support::{attacker_keys, forge_offsets, open_all_shards, plant_shard};
use gc_support::Fx;
use lodestore_format::gc::survey;
use lodestore_format::writer::BackupOptions;
use planted::{PlantedListing, planted_shard_entry};

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

#[tokio::test]
async fn adv_gc_skips_planted_oversized_shard() {
    let fx = Fx::new().await;
    fx.backup("host-a", &[("a", 1, 200 << 10)]).await;
    fx.backup("host-b", &[("b", 2, 200 << 10)]).await;
    let before = fx.survey(0).await;

    let entry = planted_shard_entry(5 << 30);
    let key = entry.key.clone();
    let planted = PlantedListing::new(&fx.store, vec![entry]);
    let after = survey(
        &planted,
        fx.repo.read_keys(),
        fx.repo.config(),
        &gc_support::plan_opts(0),
        &|_| {},
    )
    .await
    .unwrap();

    assert!(after.shards.skipped.iter().any(|(k, _)| *k == key));
    assert_eq!(planted.reads_of(&key), 0);
    assert_eq!(after.snapshots, before.snapshots);
    assert_eq!(after.live.len(), before.live.len());
}

#[tokio::test]
async fn adv_writer_skips_planted_oversized_shard() {
    let fx = Fx::new().await;
    fx.backup("host-a", &[("a", 1, 200 << 10)]).await;

    let entry = planted_shard_entry(5 << 30);
    let key = entry.key.clone();
    let planted = PlantedListing::new(&fx.store, vec![entry]);
    let keys = fx.repo.read_keys().write_keys(fx.repo.config_hash());
    let mut writer = lodestore_format::writer::Writer::new(&planted, keys, fx.repo.config())
        .await
        .unwrap();
    let src = fx.make_src("host-b", &[("b", 2, 200 << 10)]);
    writer
        .backup(
            std::slice::from_ref(&src),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();

    assert_eq!(planted.reads_of(&key), 0);
}
