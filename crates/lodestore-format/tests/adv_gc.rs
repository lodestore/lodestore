//! A host with writer credentials forges index shards; a full collection must
//! neither delete data a snapshot needs nor stop.

mod adv_support;
mod gc_support;

use std::collections::BTreeSet;

use adv_support::{
    attacker_keys, forge_offsets, open_all_shards, plant_shard_where, put_junk_pack,
};
use gc_support::{Cycle, Fx, pack_ids};
use lodestore_format::crypto::{BLOB_TYPE_DATA, BlobId};
use lodestore_format::pack::{ENTRY_LEN, PACK_PREFIX_LEN, pack_object_key};
use lodestore_format::pb;

async fn full_cycle(fx: &Fx, repack_below_pct: u8) {
    let mut cycle = Cycle::new(fx);
    cycle.survey(repack_below_pct).await;
    cycle.repack_publish().await;
    cycle.verify_compact().await;
    cycle.sweep_shards().await;
    cycle.sweep_packs(usize::MAX).await;
    cycle.finish().await;
}

/// The entries of `record` claimed to lie in the pack of `to`, one byte off.
fn retarget(record: &pb::PackRecord, to: &pb::PackRecord) -> Option<pb::PackRecord> {
    if record.pack_size > to.pack_size {
        return None;
    }
    let mut forged = forge_offsets(record);
    forged.pack_id = to.pack_id.clone();
    forged.pack_size = to.pack_size;
    forged.hpke_enc = to.hpke_enc.clone();
    forged.sealed_pack_key = to.sealed_pack_key.clone();
    Some(forged)
}

fn lowest_shard_id(shards: &[(BlobId, pb::IndexShard)]) -> BlobId {
    shards.iter().map(|(id, _)| *id).min().expect("a shard")
}

/// Three hosts with shared files, the first one forgotten, and a shard that
/// supersedes every real one and moves every record one byte off its frame,
/// or into another real pack. Its id sorts before all others.
async fn forged_fleet() -> Fx {
    let fx = Fx::new().await;
    let a = fx
        .backup(
            "host-a",
            &[
                ("a", 1, 300 << 10),
                ("shared", 3, 100 << 10),
                ("a2", 7, 100 << 10),
            ],
        )
        .await;
    fx.backup("host-b", &[("b", 2, 200 << 10), ("shared", 3, 100 << 10)])
        .await;
    fx.backup("host-c", &[("c", 5, 150 << 10), ("a2", 7, 100 << 10)])
        .await;
    fx.forget(&a.snapshot_id).await;

    let keys = attacker_keys(&fx.repo);
    let shards = open_all_shards(&fx.store, &keys).await;
    let records: Vec<_> = shards
        .iter()
        .flat_map(|(_, shard)| shard.packs.iter().cloned())
        .collect();
    let mut forged: Vec<_> = records.iter().map(forge_offsets).collect();
    for from in &records {
        if let Some(moved) = records
            .iter()
            .filter(|to| to.pack_id != from.pack_id)
            .find_map(|to| retarget(from, to))
        {
            forged.push(moved);
        }
    }
    let supersedes = shards.iter().map(|(id, _)| *id).collect();
    let lowest = lowest_shard_id(&shards);
    plant_shard_where(&fx.store, &keys, forged, supersedes, |id| *id < lowest).await;
    fx
}

#[tokio::test]
async fn adv_forged_offsets_and_supersedes_gc_keeps_every_snapshot() {
    let fx = forged_fleet().await;
    let mut cycle = Cycle::new(&fx);
    cycle.survey(100).await;
    cycle.repack_publish().await;
    cycle.verify_compact().await;
    fx.assert_all_restore().await;
    for pack in &cycle.candidates {
        let listing = fx.packs().await;
        assert!(listing.contains_key(&pack_object_key(pack)));
    }
    assert_eq!(fx.sources.borrow().len(), 2);
}

#[tokio::test]
async fn adv_forged_offsets_survive_compaction_and_sweep() {
    let fx = forged_fleet().await;
    full_cycle(&fx, 100).await;
    fx.assert_all_restore().await;
}

#[tokio::test]
async fn adv_survivor_that_does_not_open_keeps_its_pack() {
    let fx = Fx::new().await;
    fx.backup("host-a", &[("a", 1, 400 << 10)]).await;
    let a_packs = pack_ids(&fx.packs().await);
    let a = *fx.sources.borrow().keys().next().unwrap();
    fx.backup("host-b", &[("b", 2, 100 << 10)]).await;
    let b_packs: BTreeSet<BlobId> = pack_ids(&fx.packs().await)
        .difference(&a_packs)
        .copied()
        .collect();
    fx.forget(&a).await;

    let keys = attacker_keys(&fx.repo);
    let shards = open_all_shards(&fx.store, &keys).await;
    let mut forged = Vec::new();
    for (_, shard) in &shards {
        for record in &shard.packs {
            let id: BlobId = record.pack_id.as_slice().try_into().unwrap();
            if !b_packs.contains(&id) {
                continue;
            }
            let twin = put_junk_pack(&fx.store, record.pack_size as usize).await;
            let mut moved = forge_offsets(record);
            moved.pack_id = twin.to_vec();
            forged.push(moved);
        }
    }
    assert_eq!(forged.len(), b_packs.len());
    let lowest = lowest_shard_id(&shards);
    plant_shard_where(&fx.store, &keys, forged, Vec::new(), |id| *id < lowest).await;

    let survey = fx.survey(0).await;
    for pack in &b_packs {
        assert!(
            survey.plan.delete.iter().any(|p| p.pack_id == *pack),
            "the forged records must make the real pack look dead"
        );
    }
    drop(survey);

    full_cycle(&fx, 0).await;
    let left = fx.packs().await;
    for pack in &b_packs {
        assert!(left.contains_key(&pack_object_key(pack)));
    }
    fx.assert_all_restore().await;
}

#[tokio::test]
async fn adv_planted_unopenable_pack_does_not_stop_gc() {
    let fx = Fx::new().await;
    let a = fx
        .backup("host-a", &[("a", 1, 200 << 10), ("shared", 3, 100 << 10)])
        .await;
    fx.backup("host-b", &[("b", 2, 200 << 10), ("shared", 3, 100 << 10)])
        .await;
    fx.forget(&a.snapshot_id).await;

    let survey = fx.survey(0).await;
    let (live_id, _) = survey
        .live
        .iter()
        .find(|(_, t)| *t == BLOB_TYPE_DATA)
        .unwrap();
    let live_id = *live_id;
    let ulen = survey.index.get(&live_id, BLOB_TYPE_DATA).unwrap().ulen;
    let template = survey.shards.shards[0].1.packs[0].clone();
    drop(survey);

    let keys = attacker_keys(&fx.repo);
    let size = PACK_PREFIX_LEN + 64 + 4000;
    let junk = put_junk_pack(&fx.store, size).await;
    let mut entries = Vec::with_capacity(ENTRY_LEN);
    entries.extend_from_slice(&live_id);
    entries.push(BLOB_TYPE_DATA);
    entries.extend_from_slice(&(PACK_PREFIX_LEN as u32).to_le_bytes());
    entries.extend_from_slice(&64u32.to_le_bytes());
    entries.extend_from_slice(&ulen.to_le_bytes());
    let record = pb::PackRecord {
        pack_id: junk.to_vec(),
        hpke_enc: template.hpke_enc,
        sealed_pack_key: template.sealed_pack_key,
        pack_size: size as u64,
        entries,
    };
    let shards = open_all_shards(&fx.store, &keys).await;
    let lowest = lowest_shard_id(&shards);
    plant_shard_where(&fx.store, &keys, vec![record], Vec::new(), |id| {
        *id < lowest
    })
    .await;

    let survey = fx.survey(50).await;
    assert!(
        survey.plan.repack.iter().any(|p| p.pack_id == junk),
        "the planted pack must be a repack target"
    );
    drop(survey);

    full_cycle(&fx, 50).await;
    fx.assert_all_restore().await;
}

/// One record of a real pack, broken in each way loading a shard checks.
fn malformed(record: &pb::PackRecord) -> Vec<pb::PackRecord> {
    let one = pb::PackRecord {
        entries: record.entries[..ENTRY_LEN].to_vec(),
        ..record.clone()
    };
    let with_entry = |edit: &dyn Fn(&mut Vec<u8>)| {
        let mut r = one.clone();
        edit(&mut r.entries);
        r
    };
    let past_end = u32::try_from(record.pack_size).unwrap();
    vec![
        with_entry(&|e| e.truncate(ENTRY_LEN - 1)),
        with_entry(&|e| e[32] = 9),
        with_entry(&|e| e[33..37].copy_from_slice(&0u32.to_le_bytes())),
        with_entry(&|e| e[33..37].copy_from_slice(&past_end.to_le_bytes())),
        pb::PackRecord {
            hpke_enc: one.hpke_enc[1..].to_vec(),
            ..one.clone()
        },
    ]
}

#[tokio::test]
async fn adv_malformed_shard_does_not_stop_gc() {
    let fx = Fx::new().await;
    let a = fx
        .backup("host-a", &[("a", 1, 200 << 10), ("shared", 3, 100 << 10)])
        .await;
    fx.backup("host-b", &[("b", 2, 200 << 10), ("shared", 3, 100 << 10)])
        .await;
    fx.forget(&a.snapshot_id).await;

    let keys = attacker_keys(&fx.repo);
    let shards = open_all_shards(&fx.store, &keys).await;
    let template = shards
        .iter()
        .find_map(|(_, s)| s.packs.first())
        .unwrap()
        .clone();
    let variants = malformed(&template);
    let planted = variants.len();
    for record in variants {
        plant_shard_where(&fx.store, &keys, vec![record], Vec::new(), |_| true).await;
    }

    let survey = fx.survey(100).await;
    assert_eq!(survey.shards.skipped.len(), planted);
    drop(survey);
    full_cycle(&fx, 100).await;
    fx.assert_all_restore().await;
}
