//! A host with writer credentials plants a shard that names the real packs
//! with wrong offsets and lists first; every snapshot still restores.

mod adv_support;
mod gc_support;

use std::collections::HashSet;

use adv_support::{attacker_keys, forge_offsets, open_all_shards, plant_shard_where};
use gc_support::{Cycle, Fx};
use lodestore_format::index::parse_hex32;
use lodestore_format::reader::Reader;

#[tokio::test]
async fn adv_forged_copy_listed_first_does_not_break_restore() {
    let fx = Fx::new().await;
    let a = fx
        .backup(
            "host-a",
            &[("shared", 1, 100 << 10), ("only-a", 2, 200 << 10)],
        )
        .await;
    fx.backup(
        "host-b",
        &[("shared", 1, 100 << 10), ("only-b", 3, 200 << 10)],
    )
    .await;
    fx.forget(&a.snapshot_id).await;

    let mut cycle = Cycle::new(&fx);
    cycle.survey(100).await;
    cycle.repack_publish().await;
    cycle.verify_compact().await;
    cycle.sweep_shards().await;
    cycle.sweep_packs(usize::MAX).await;
    cycle.finish().await;

    let listed: HashSet<_> = fx
        .listing("index/")
        .await
        .keys()
        .map(|k| parse_hex32(k.strip_prefix("index/").unwrap()).unwrap())
        .collect();
    assert!(!listed.is_empty());
    let reader =
        Reader::open(&fx.store, fx.repo.read_keys().duplicate(), fx.repo.config()).unwrap();
    let snapshots = reader.list_snapshots().await.unwrap().entries;
    assert!(!snapshots.is_empty());
    for entry in snapshots {
        let snapshot = reader.snapshot(&entry.id).await.unwrap();
        for raw in &snapshot.index_refs {
            let id: [u8; 32] = raw.as_slice().try_into().unwrap();
            assert!(
                !listed.contains(&id),
                "a snapshot still names a shard in index/; the scenario would not reach the listing"
            );
        }
    }

    let keys = attacker_keys(&fx.repo);
    let shards = open_all_shards(&fx.store, &keys).await;
    let records = shards
        .iter()
        .flat_map(|(_, shard)| shard.packs.iter().map(forge_offsets))
        .collect();
    let supersedes = shards.iter().map(|(id, _)| *id).collect();
    let lowest = shards.iter().map(|(id, _)| *id).min().unwrap();
    plant_shard_where(&fx.store, &keys, records, supersedes, |id| *id < lowest).await;

    fx.assert_all_restore().await;
}
