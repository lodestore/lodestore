//! Every writer of index shards cuts them by size, and the collector rewrites
//! a shard written before the limit into shards under it.

mod gc_support;

use std::collections::HashSet;

use bytes::Bytes;
use gc_support::{Files, Fx};
use lodestore_format::attest::{MARKER_LEGACY_SHARDS, put_attestation};
use lodestore_format::crypto::{BlobId, Entropy, HPKE_ENC_LEN, SEALED_KEY_LEN};
use lodestore_format::gc::compact::{compact_plan, write_compacted};
use lodestore_format::gc::mark::{LiveSet, load_shards};
use lodestore_format::index::{MAX_SHARD_SIZE, index_object_key, seal_shard};
use lodestore_format::pack::{ENTRY_LEN, PACK_MAX_SIZE, PACK_PREFIX_LEN};
use lodestore_format::pb;
use lodestore_format::store::BlobStore;
use lodestore_format::writer::BackupOptions;

const TARGET: usize = 16 << 10;
const SLACK: u64 = 4 << 10;

fn many_small_files() -> Vec<(String, u8, usize)> {
    (0..=u8::MAX)
        .take(200)
        .map(|i| (format!("f{i:03}"), i, 2 << 10))
        .collect()
}

async fn shard_sizes(fx: &Fx) -> Vec<u64> {
    fx.listing("index/").await.into_values().collect()
}

#[tokio::test]
async fn shard_split_writer_keeps_shards_under_the_limit() {
    let fx = Fx::new().await;
    let names = many_small_files();
    let files: Vec<(&str, u8, usize)> =
        names.iter().map(|(n, s, l)| (n.as_str(), *s, *l)).collect();
    let src = fx.make_src("many", &files);
    let mut w = fx.open_writer().await;
    w.set_shard_target(TARGET);
    let out = w
        .backup(
            std::slice::from_ref(&src),
            BackupOptions {
                pack_target_size: 4 << 10,
                index_flush_packs: 100_000,
                ..Default::default()
            },
            &|_| {},
        )
        .await
        .unwrap();
    fx.sources.borrow_mut().insert(out.snapshot_id, src);

    let sizes = shard_sizes(&fx).await;
    assert!(
        sizes.len() > 1,
        "one shard for {} packs",
        fx.packs().await.len()
    );
    for size in sizes {
        assert!(size <= TARGET as u64 + SLACK, "shard of {size} bytes");
    }
    fx.assert_all_restore().await;
}

#[tokio::test]
async fn shard_split_writer_default_target() {
    let fx = Fx::new().await;
    let files: Files<'_> = &[("a", 1, 300 << 10), ("b", 2, 300 << 10)];
    fx.backup("plain", files).await;
    let sizes = shard_sizes(&fx).await;
    assert!(!sizes.is_empty());
    assert!(sizes.iter().all(|s| *s <= MAX_SHARD_SIZE));
    fx.assert_all_restore().await;
}

/// A shard written before the limit: honest records, `records` of about
/// 1 MiB of entries each.
fn legacy_shard(fx: &Fx, records: usize) -> (BlobId, Vec<u8>, Vec<pb::PackRecord>) {
    let mut entropy = Entropy::os();
    let packs: Vec<pb::PackRecord> = (0..records)
        .map(|_| {
            let mut entries = vec![0u8; ENTRY_LEN * 23_000];
            entropy.fill(&mut entries);
            for e in entries.as_chunks_mut::<ENTRY_LEN>().0 {
                e[32] = 1;
                e[33..37].copy_from_slice(&(PACK_PREFIX_LEN as u32).to_le_bytes());
                e[37..41].copy_from_slice(&64u32.to_le_bytes());
                e[41..45].copy_from_slice(&16u32.to_le_bytes());
            }
            pb::PackRecord {
                pack_id: entropy.bytes::<32>().to_vec(),
                hpke_enc: vec![0; HPKE_ENC_LEN],
                sealed_pack_key: vec![0; SEALED_KEY_LEN],
                pack_size: PACK_MAX_SIZE as u64,
                entries,
            }
        })
        .collect();
    let keys = fx.repo.read_keys();
    let (id, bytes) =
        seal_shard(keys.k_idx(), keys.repo_id(), packs.clone(), &mut entropy).unwrap();
    assert!(bytes.len() as u64 > MAX_SHARD_SIZE);
    (id, bytes, packs)
}

#[tokio::test]
async fn shard_split_compaction_rewrites_an_oversized_shard() {
    let fx = Fx::new().await;
    let (id, bytes, packs) = legacy_shard(&fx, 66);
    fx.store
        .put_if_absent(&index_object_key(&id), Bytes::from(bytes))
        .await
        .unwrap();
    put_attestation(
        &fx.store,
        fx.repo.read_keys(),
        &[id],
        &[MARKER_LEGACY_SHARDS],
        Vec::new(),
        &mut Entropy::os(),
    )
    .await
    .unwrap();

    let set = load_shards(&fx.store, fx.repo.read_keys()).await.unwrap();
    assert!(set.oversized.contains(&id));
    let plan = compact_plan(&set, &HashSet::new());
    assert_eq!(plan.rewrite, [id]);
    assert_eq!(plan.attested_records.len(), packs.len());
    assert!(plan.verified_records.is_empty() && plan.other_records.is_empty());

    let done = write_compacted(
        &fx.store,
        fx.repo.read_keys(),
        &plan,
        &LiveSet::default(),
        &HashSet::new(),
        &mut Entropy::os(),
    )
    .await
    .unwrap();
    assert!(done.attested.len() > 1);
    assert!(done.verified.is_empty() && done.other.is_empty());

    let after = load_shards(&fx.store, fx.repo.read_keys()).await.unwrap();
    let mut written: Vec<Vec<u8>> = Vec::new();
    for new in &done.attested {
        let size = fx.listing("index/").await[&index_object_key(new)];
        assert!(size <= MAX_SHARD_SIZE);
        assert!(after.attested.contains(new));
        let shard = &after.shards.iter().find(|(s, _)| s == new).unwrap().1;
        written.extend(shard.packs.iter().map(|p| p.pack_id.clone()));
    }
    written.sort();
    let mut old: Vec<Vec<u8>> = packs.iter().map(|p| p.pack_id.clone()).collect();
    old.sort();
    assert_eq!(written, old);
}
