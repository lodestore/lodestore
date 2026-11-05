//! A host with writer credentials plants a shard that names the real packs
//! with wrong offsets and lists first; every snapshot still restores.

mod adv_support;
mod gc_support;

use std::collections::HashSet;

use adv_support::{attacker_keys, forge_offsets, open_all_shards, plant_shard_where};
use gc_support::{Cycle, Fx};
use lodestore_format::crypto::{BlobId, Entropy};
use lodestore_format::index::parse_hex32;
use lodestore_format::pack::pack_object_key;
use lodestore_format::pb;
use lodestore_format::reader::Reader;
use lodestore_format::store::{BlobStore, put_named};

/// Two hosts after a full cleanup, so that no snapshot names a shard still
/// in `index/` and a reader has to list them. Returns the real pack records
/// and the lowest shard id.
async fn compacted_fleet() -> (Fx, Vec<pb::PackRecord>, BlobId) {
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

    let shards = open_all_shards(&fx.store, &attacker_keys(&fx.repo)).await;
    let lowest = shards.iter().map(|(id, _)| *id).min().unwrap();
    let records = shards
        .into_iter()
        .flat_map(|(_, shard)| shard.packs)
        .collect();
    (fx, records, lowest)
}

async fn plant_first(fx: &Fx, records: Vec<pb::PackRecord>, lowest: BlobId) {
    let keys = attacker_keys(&fx.repo);
    plant_shard_where(&fx.store, &keys, records, Vec::new(), |id| *id < lowest).await;
}

#[tokio::test]
async fn adv_forged_copy_listed_first_does_not_break_restore() {
    let (fx, records, lowest) = compacted_fleet().await;
    plant_first(&fx, records.iter().map(forge_offsets).collect(), lowest).await;
    fx.assert_all_restore().await;
}

#[tokio::test]
async fn adv_copy_in_pack_of_unknown_version_does_not_break_restore() {
    let (fx, records, lowest) = compacted_fleet().await;
    let mut entropy = Entropy::os();
    let mut planted = Vec::new();
    for record in &records {
        let real: BlobId = record.pack_id.as_slice().try_into().unwrap();
        let mut bytes = fx
            .store
            .get(&pack_object_key(&real))
            .await
            .unwrap()
            .to_vec();
        bytes[6] = 99;
        let twin: BlobId = entropy.bytes();
        put_named(&fx.store, &pack_object_key(&twin), bytes)
            .await
            .unwrap();
        planted.push(pb::PackRecord {
            pack_id: twin.to_vec(),
            ..record.clone()
        });
    }
    plant_first(&fx, planted, lowest).await;
    fx.assert_all_restore().await;
}
