//! Repack, verify, compact and sweep over a real repository in a folder.

mod gc_support;

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::ops::Range;
use std::sync::atomic::{AtomicIsize, Ordering};

use bytes::Bytes;

use gc_support::*;
use lodestore_format::Error;
use lodestore_format::crypto::{BLOB_TYPE_DATA, BlobId, Entropy, HPKE_ENC_LEN, SEALED_KEY_LEN};
use lodestore_format::gc::candidates;
use lodestore_format::gc::compact::{check_reachable, compact_plan, write_compacted};
use lodestore_format::gc::mark::load_shards;
use lodestore_format::gc::sweep::confirm_unreferenced;
use lodestore_format::index::{index_object_key, seal_shard};
use lodestore_format::pack::{PACK_PREFIX_LEN, pack_object_key, parse_entries};
use lodestore_format::pb;
use lodestore_format::store::{BlobError, BlobStore, ListEntry, LocalFsStore, put_named};

const SHARED: (&str, u8, usize) = ("shared", 1, 100 << 10);

#[tokio::test]
async fn gc_repack_full_cycle_restores() {
    let fx = Fx::new().await;
    let a = fx
        .backup("a", &[("shared", 1, 100 << 10), ("only-a", 2, 200 << 10)])
        .await;
    fx.backup("b", &[("shared", 1, 100 << 10), ("only-b", 3, 200 << 10)])
        .await;
    fx.forget(&a.snapshot_id).await;
    let before = fx.pack_bytes().await;
    let packs_before = fx.packs().await.len();

    let mut cycle = Cycle::new(&fx);
    cycle.survey(100).await;
    assert!(!cycle.survey.as_ref().unwrap().plan.repack.is_empty());
    cycle.repack_publish().await;
    assert!(cycle.repack.shard.is_some());
    let plan = cycle.verify_compact().await;
    assert!(!plan.is_empty());
    assert!(cycle.resurrected.is_empty());
    let shards = cycle.sweep_shards().await;
    assert!(shards.failed.is_empty());
    assert_eq!(shards.deleted as usize, plan.rewrite.len());
    let packs = cycle.sweep_packs(usize::MAX).await;
    assert!(packs.failed.is_empty());
    assert_eq!(packs.deleted as usize, cycle.candidates.len());
    assert_eq!(cycle.finish().await.deleted, 1);

    fx.assert_all_restore().await;
    assert_eq!(fx.sources.borrow().len(), 1);
    assert!(fx.pack_bytes().await < before);
    assert!(fx.packs().await.len() <= packs_before);
    assert!(fx.listing("locks/").await.is_empty());
    assert_eq!(fx.record_shards().await, 1);
}

#[tokio::test]
async fn gc_repack_partial_pack_rewritten() {
    let fx = Fx::new().await;
    let a = fx.backup("a", &[SHARED, ("only-a", 2, 200 << 10)]).await;
    fx.backup("b", &[SHARED, ("only-b", 3, 200 << 10)]).await;
    fx.forget(&a.snapshot_id).await;

    let mut cycle = Cycle::new(&fx);
    cycle.survey(50).await;
    let survey = cycle.survey.as_ref().unwrap();
    let sparse = survey.plan.repack.clone();
    let data = sparse
        .iter()
        .find(|p| p.blob_type == BLOB_TYPE_DATA)
        .expect("the data pack of the forgotten snapshot is sparse");
    assert!(data.live_bytes * 100 < 50 * data.size);
    let size: u64 = sparse.iter().map(|p| p.size).sum();
    let live: u64 = sparse.iter().map(|p| p.live_bytes).sum();
    let ids: Vec<_> = sparse.iter().map(|p| p.pack_id).collect();

    cycle.repack_publish().await;
    let r = &cycle.repack;
    assert_eq!(r.replaced, ids);
    assert_eq!(r.bytes_read, size);
    assert!(!r.new_packs.is_empty());
    assert!(r.bytes_written >= live && r.bytes_written < size);

    let replaced: HashSet<BlobId> = ids.into_iter().collect();
    check_reachable(
        &fx.store,
        fx.repo.read_keys(),
        &cycle.survey.as_ref().unwrap().live,
        &replaced,
        &HashSet::new(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn gc_repack_types_kept_apart() {
    let fx = Fx::new().await;
    let src = fx.make_src("x", &[("old", 2, 200 << 10), ("sub/c", 1, 100 << 10)]);
    let first = {
        let mut w = fx.open_writer().await;
        fx.commit(&mut w, src.clone()).await
    };
    fs::remove_file(src.join("old")).unwrap();
    write_files(&src, &[("new", 3, 200 << 10)]);
    {
        let mut w = fx.open_writer().await;
        fx.commit(&mut w, src.clone()).await;
    }
    fx.forget(&first.snapshot_id).await;

    let mut cycle = Cycle::new(&fx);
    cycle.survey(50).await;
    let kinds: BTreeSet<u8> = cycle
        .survey
        .as_ref()
        .unwrap()
        .plan
        .repack
        .iter()
        .map(|p| p.blob_type)
        .collect();
    assert_eq!(kinds.len(), 2, "a data pack and a tree pack are sparse");

    cycle.repack_publish().await;
    assert_eq!(cycle.repack.new_packs.len(), 2);
    let shards = load_shards(&fx.store, fx.repo.read_keys()).await.unwrap();
    let mut seen = BTreeSet::new();
    for (_, shard) in &shards.shards {
        for record in &shard.packs {
            let id: BlobId = record.pack_id.as_slice().try_into().unwrap();
            if cycle.repack.new_packs.contains(&id) {
                let types: BTreeSet<u8> = parse_entries(&record.entries)
                    .unwrap()
                    .iter()
                    .map(|e| e.blob_type)
                    .collect();
                assert_eq!(types.len(), 1);
                seen.extend(types);
            }
        }
    }
    assert_eq!(seen.len(), 2);

    cycle.verify_compact().await;
    cycle.sweep_shards().await;
    cycle.sweep_packs(usize::MAX).await;
    cycle.finish().await;
    fx.assert_all_restore().await;
}

#[tokio::test]
async fn gc_repack_verify_resurrects() {
    let fx = Fx::new().await;
    let a = fx.backup("a", &[SHARED, ("only-a", 2, 200 << 10)]).await;
    fx.backup("b", &[SHARED, ("only-b", 3, 200 << 10)]).await;
    fx.forget(&a.snapshot_id).await;

    let mut cycle = Cycle::new(&fx);
    cycle.survey(50).await;
    let sparse = cycle
        .survey
        .as_ref()
        .unwrap()
        .plan
        .repack
        .iter()
        .find(|p| p.blob_type == BLOB_TYPE_DATA)
        .unwrap()
        .pack_id;
    let c_src = fx.make_src("c", &[("only-a", 2, 200 << 10)]);
    let mut writer = fx.open_writer().await;
    cycle.repack_publish().await;
    fx.commit(&mut writer, c_src).await;

    cycle.verify_compact().await;
    assert!(cycle.resurrected.contains(&sparse));
    assert!(!cycle.candidates.contains(&sparse));
    cycle.sweep_shards().await;
    cycle.sweep_packs(usize::MAX).await;
    cycle.finish().await;

    assert!(fx.packs().await.contains_key(&pack_object_key(&sparse)));
    assert_eq!(fx.sources.borrow().len(), 2);
    fx.assert_all_restore().await;
}

#[tokio::test]
async fn gc_repack_d15_blocks_sweep() {
    let fx = Fx::new().await;
    let a = fx.backup("a", &[SHARED, ("only-a", 2, 200 << 10)]).await;
    fx.backup("b", &[SHARED, ("only-b", 3, 200 << 10)]).await;
    fx.forget(&a.snapshot_id).await;

    let mut cycle = Cycle::new(&fx);
    cycle.survey(50).await;
    cycle.repack_publish().await;
    let survey = cycle.survey.as_ref().unwrap();
    let planned: HashSet<BlobId> = candidates(&survey.plan, &cycle.repack)
        .into_iter()
        .collect();
    let victim = survey
        .live
        .iter()
        .filter_map(|(id, t)| survey.index.get(id, *t))
        .map(|l| l.pack_id)
        .find(|p| !planned.contains(p))
        .expect("a live blob outside the planned packs");
    let mut wrong = planned.clone();
    wrong.insert(victim);

    let shards = load_shards(&fx.store, fx.repo.read_keys()).await.unwrap();
    let plan = compact_plan(&shards, &wrong);
    let err = write_compacted(
        &fx.store,
        fx.repo.read_keys(),
        &plan,
        &survey.live,
        &wrong,
        &mut Entropy::os(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Corrupt { .. }), "{err:?}");
    assert!(
        unsuperseded(&fx).await.len() == fx.listing("index/").await.len(),
        "a compaction that failed its check must not supersede anything"
    );
    let packs = fx.packs().await;
    assert!(
        confirm_unreferenced(&fx.store, fx.repo.read_keys(), &wrong)
            .await
            .is_err()
    );
    assert_eq!(fx.packs().await, packs);
    assert!(fx.packs().await.contains_key(&pack_object_key(&victim)));
}

fn synthetic_record(n: u32) -> pb::PackRecord {
    let mut entries = Vec::new();
    entries.extend_from_slice(blake3::hash(&n.to_le_bytes()).as_bytes());
    entries.push(BLOB_TYPE_DATA);
    entries.extend_from_slice(&(PACK_PREFIX_LEN as u32).to_le_bytes());
    entries.extend_from_slice(&100u32.to_le_bytes());
    entries.extend_from_slice(&50u32.to_le_bytes());
    pb::PackRecord {
        pack_id: blake3::hash(&[&n.to_le_bytes()[..], b"pack"].concat())
            .as_bytes()
            .to_vec(),
        hpke_enc: vec![0; HPKE_ENC_LEN],
        sealed_pack_key: vec![0; SEALED_KEY_LEN],
        pack_size: 5000,
        entries,
    }
}

async fn unsuperseded(fx: &Fx) -> BTreeSet<BlobId> {
    let shards = load_shards(&fx.store, fx.repo.read_keys()).await.unwrap();
    let replaced: HashSet<BlobId> = shards
        .shards
        .iter()
        .flat_map(|(_, s)| s.supersedes.iter())
        .map(|id| id.as_slice().try_into().unwrap())
        .collect();
    shards
        .shards
        .iter()
        .map(|(id, _)| *id)
        .filter(|id| !replaced.contains(id))
        .collect()
}

#[tokio::test]
async fn gc_repack_compaction_merges_small_shards() {
    let fx = Fx::new().await;
    for i in 0..5u8 {
        fx.backup(&format!("s{i}"), &[("f", i + 10, 10 << 10)])
            .await;
    }
    let keys = fx.repo.read_keys();
    let records: Vec<_> = (0..600).map(synthetic_record).collect();
    let (big, bytes) =
        seal_shard(keys.k_idx(), keys.repo_id(), records, &mut Entropy::os()).unwrap();
    put_named(&fx.store, &index_object_key(&big), bytes)
        .await
        .unwrap();
    let before = unsuperseded(&fx).await;
    assert_eq!(before.len(), 6);

    let shards = load_shards(&fx.store, keys).await.unwrap();
    let plan = compact_plan(&shards, &HashSet::new());
    assert_eq!(plan.rewrite.len(), 5);
    assert!(!plan.rewrite.contains(&big));
    let written = write_compacted(
        &fx.store,
        keys,
        &plan,
        &Default::default(),
        &HashSet::new(),
        &mut Entropy::os(),
    )
    .await
    .unwrap();
    assert_eq!(written.len(), 2, "one shard of records and the commit");

    let after = unsuperseded(&fx).await;
    assert_eq!(after, BTreeSet::from([big, written[0], written[1]]));
    fx.assert_all_restore().await;
}

#[tokio::test]
async fn gc_repack_compaction_noop() {
    let fx = Fx::new().await;
    fx.backup("a", &[SHARED]).await;
    let shards = load_shards(&fx.store, fx.repo.read_keys()).await.unwrap();
    assert_eq!(shards.shards.len(), 1);
    assert!(compact_plan(&shards, &HashSet::new()).is_empty());
}

/// Fails every PUT under `index/` after the first `left`.
struct FailIndexPuts<'a> {
    inner: &'a LocalFsStore,
    left: AtomicIsize,
}

impl BlobStore for FailIndexPuts<'_> {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<(), BlobError> {
        let allowed = !key.starts_with("index/") || self.left.fetch_sub(1, Ordering::SeqCst) > 0;
        if !allowed {
            return Err(BlobError::backend("injected failure"));
        }
        self.inner.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.inner.get_range(key, range).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        self.inner.list(prefix).await
    }
}

#[tokio::test]
async fn gc_compaction_cut_short_hides_nothing() {
    let fx = Fx::new().await;
    fx.backup("a", &[SHARED]).await;
    let keys = fx.repo.read_keys();
    let records: Vec<_> = (0..1100).map(synthetic_record).collect();
    let (big, bytes) = seal_shard(
        keys.k_idx(),
        keys.repo_id(),
        records.clone(),
        &mut Entropy::os(),
    )
    .unwrap();
    put_named(&fx.store, &index_object_key(&big), bytes)
        .await
        .unwrap();
    let doomed: HashSet<BlobId> =
        HashSet::from([records[0].pack_id.as_slice().try_into().unwrap()]);
    let shards = load_shards(&fx.store, keys).await.unwrap();
    let plan = compact_plan(&shards, &doomed);
    assert!(plan.records.len() > 1024, "the compaction needs two shards");

    let failing = FailIndexPuts {
        inner: &fx.store,
        left: AtomicIsize::new(1),
    };
    write_compacted(
        &failing,
        keys,
        &plan,
        &Default::default(),
        &doomed,
        &mut Entropy::os(),
    )
    .await
    .unwrap_err();

    let index = load_shards(&fx.store, keys)
        .await
        .unwrap()
        .index(&HashSet::new())
        .unwrap();
    for record in &records {
        let blob: BlobId = record.entries[..32].try_into().unwrap();
        assert!(index.get(&blob, BLOB_TYPE_DATA).is_some());
    }
    fx.assert_all_restore().await;
}
