//! Repack, verify, compact and sweep over a real repository in a folder.

mod adv_support;
mod gc_support;

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::ops::Range;
use std::sync::Mutex;
use std::sync::atomic::{AtomicIsize, Ordering};

use bytes::Bytes;

use adv_support::{attacker_keys, forge_offsets, open_all_shards, plant_shard, put_junk_pack};
use gc_support::*;
use lodestore_format::Error;
use lodestore_format::attest::read_attestations;
use lodestore_format::crypto::{BLOB_TYPE_DATA, BlobId, Entropy, HPKE_ENC_LEN, SEALED_KEY_LEN};
use lodestore_format::gc::compact::{check_reachable, compact_plan, write_compacted};
use lodestore_format::gc::mark::load_shards;
use lodestore_format::gc::repack::{RepackOutcome, repack};
use lodestore_format::gc::sweep::confirm_unreferenced;
use lodestore_format::gc::verify::verify;
use lodestore_format::gc::{Survey, candidates};
use lodestore_format::index::{index_object_key, seal_shard};
use lodestore_format::pack::{PACK_PREFIX_LEN, pack_object_key, parse_entries};
use lodestore_format::pb;
use lodestore_format::store::{BlobError, BlobStore, ListEntry, LocalFsStore, put_named};

const SHARED: (&str, u8, usize) = ("shared", 1, 100 << 10);

async fn locks_left(fx: &Fx) -> Vec<String> {
    let mut left = fx.listing("locks/").await;
    left.retain(|key, _| !key.starts_with("locks/attest-"));
    left.into_keys().collect()
}

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
    assert!(locks_left(&fx).await.is_empty());
    assert_eq!(
        fx.record_shards().await,
        2,
        "records of the attested repack shard and the unattested rest stay apart"
    );
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
    let records: Vec<_> = [&written.verified, &written.attested, &written.other]
        .into_iter()
        .flatten()
        .copied()
        .collect();
    let commit = written.superseding.expect("the commit");
    assert_eq!(records.len(), 1, "one shard of records and the commit");

    let after = unsuperseded(&fx).await;
    assert_eq!(after, BTreeSet::from([big, records[0], commit]));
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
    assert!(plan.record_count() > 1024, "the compaction needs two shards");

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

/// Records every ranged read.
struct RangeLog<'a> {
    inner: &'a LocalFsStore,
    ranges: Mutex<Vec<(String, Range<u64>)>>,
}

impl BlobStore for RangeLog<'_> {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<(), BlobError> {
        self.inner.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.ranges
            .lock()
            .unwrap()
            .push((key.to_owned(), range.clone()));
        self.inner.get_range(key, range).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        self.inner.list(prefix).await
    }
}

/// A junk pack of the size of `pack` and a shard that gives it the entries
/// of `pack` with every frame one byte off.
async fn plant_forged_twin(fx: &Fx, pack: &BlobId) -> BlobId {
    let keys = attacker_keys(&fx.repo);
    let size = fx.packs().await[&pack_object_key(pack)];
    let twin = put_junk_pack(&fx.store, size as usize).await;
    let mut records = Vec::new();
    for (_, shard) in open_all_shards(&fx.store, &keys).await {
        for record in shard.packs {
            if record.pack_id.as_slice() == pack {
                let mut forged = forge_offsets(&record);
                forged.pack_id = twin.to_vec();
                records.push(forged);
            }
        }
    }
    assert!(!records.is_empty());
    plant_shard(&fx.store, &keys, records, Vec::new()).await;
    twin
}

#[tokio::test]
async fn verify_resurrects_when_survivor_does_not_open() {
    let fx = Fx::new().await;
    fx.backup("a", &[("a", 1, 200 << 10)]).await;
    fx.backup("b", &[("b", 2, 200 << 10)]).await;
    let survey = fx.survey(0).await;
    let (id, blob_type) = survey
        .live
        .iter()
        .find(|(_, t)| *t == BLOB_TYPE_DATA)
        .unwrap();
    let pack = survey.index.get(id, *blob_type).unwrap().pack_id;
    plant_forged_twin(&fx, &pack).await;

    let candidates = HashSet::from([pack]);
    let verified = verify(
        &fx.store,
        fx.repo.read_keys(),
        fx.repo.config(),
        &candidates,
        &|_, _| {},
    )
    .await
    .unwrap();
    assert_eq!(verified.resurrected, vec![pack]);
    assert!(verified.candidates.is_empty());
}

#[tokio::test]
async fn verify_deletes_when_survivor_opens() {
    let fx = Fx::new().await;
    let a = fx.backup("a", &[SHARED, ("only-a", 2, 200 << 10)]).await;
    fx.backup("b", &[SHARED, ("only-b", 3, 200 << 10)]).await;
    fx.forget(&a.snapshot_id).await;
    let mut cycle = Cycle::new(&fx);
    cycle.survey(100).await;
    cycle.repack_publish().await;
    assert!(!cycle.repack.replaced.is_empty());

    let verified = verify(
        &fx.store,
        fx.repo.read_keys(),
        fx.repo.config(),
        &cycle.published,
        &|_, _| {},
    )
    .await
    .unwrap();
    assert!(verified.resurrected.is_empty());
    assert_eq!(verified.candidates, cycle.published);
}

#[tokio::test]
async fn verify_checks_each_blob_once() {
    let fx = Fx::new().await;
    let a = fx.backup("a", &[SHARED, ("only-a", 2, 200 << 10)]).await;
    fx.backup("b", &[SHARED, ("only-b", 3, 200 << 10)]).await;
    fx.forget(&a.snapshot_id).await;
    let mut cycle = Cycle::new(&fx);
    cycle.survey(100).await;
    cycle.repack_publish().await;
    let data_pack = cycle
        .survey
        .as_ref()
        .unwrap()
        .plan
        .repack
        .iter()
        .find(|p| p.blob_type == BLOB_TYPE_DATA)
        .map(|p| p.pack_id)
        .unwrap();
    let twin = plant_forged_twin(&fx, &data_pack).await;
    let mut candidates = cycle.published.clone();
    candidates.insert(twin);

    let log = RangeLog {
        inner: &fx.store,
        ranges: Mutex::new(Vec::new()),
    };
    let verified = verify(
        &log,
        fx.repo.read_keys(),
        fx.repo.config(),
        &candidates,
        &|_, _| {},
    )
    .await
    .unwrap();
    assert!(verified.resurrected.is_empty());

    let index = load_shards(&fx.store, fx.repo.read_keys())
        .await
        .unwrap()
        .index(&HashSet::new())
        .unwrap();
    let ranges = log.ranges.into_inner().unwrap();
    let mut checked = 0;
    for (id, blob_type) in verified.live.iter().filter(|(_, t)| *t == BLOB_TYPE_DATA) {
        for loc in index.locations(id, *blob_type) {
            if !cycle.repack.new_packs.contains(&loc.pack_id) {
                continue;
            }
            let start = u64::from(loc.offset);
            let want = (
                pack_object_key(&loc.pack_id),
                start..start + u64::from(loc.enc_len),
            );
            assert_eq!(ranges.iter().filter(|r| **r == want).count(), 1);
            checked += 1;
        }
    }
    assert!(checked > 0);
}

/// Two sparse data packs: each holds a blob that a surviving snapshot shares.
async fn two_sparse_packs() -> Fx {
    let fx = Fx::new().await;
    let a = fx.backup("a", &[SHARED, ("only-a", 2, 200 << 10)]).await;
    fx.backup("b", &[SHARED, ("only-b", 3, 200 << 10)]).await;
    let c = fx
        .backup("c", &[("shared-2", 4, 100 << 10), ("only-c", 5, 200 << 10)])
        .await;
    fx.backup("d", &[("shared-2", 4, 100 << 10), ("only-d", 6, 200 << 10)])
        .await;
    fx.forget(&a.snapshot_id).await;
    fx.forget(&c.snapshot_id).await;
    fx
}

fn forged_shard(shard: &pb::IndexShard, targets: &HashSet<BlobId>) -> pb::IndexShard {
    pb::IndexShard {
        packs: shard
            .packs
            .iter()
            .filter(|r| targets.contains(r.pack_id.as_slice()))
            .map(forge_offsets)
            .collect(),
        supersedes: Vec::new(),
    }
}

/// Makes the index of the survey believe the frames of `targets` lie one byte
/// off. With `keep_real` the forged records come first and the real ones stay
/// as other copies; without it the real records are gone.
fn forge_survey(survey: &mut Survey, targets: &HashSet<BlobId>, keep_real: bool) {
    let shards = &mut survey.shards.shards;
    if keep_real {
        let forged: Vec<_> = shards
            .iter()
            .map(|(id, shard)| (*id, forged_shard(shard, targets)))
            .filter(|(_, shard)| !shard.packs.is_empty())
            .map(|(id, shard)| (blake3::hash(&id).into(), shard))
            .collect();
        shards.splice(0..0, forged);
    } else {
        for (_, shard) in shards.iter_mut() {
            for record in &mut shard.packs {
                if targets.contains(record.pack_id.as_slice()) {
                    *record = forge_offsets(record);
                }
            }
        }
    }
    survey.index = survey.shards.index(&HashSet::new()).unwrap();
}

async fn run_repack(fx: &Fx, survey: &Survey) -> RepackOutcome {
    repack(
        &fx.store,
        fx.repo.read_keys(),
        survey,
        &mut Entropy::os(),
        &|_, _| {},
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn repack_keeps_pack_whose_frame_does_not_open() {
    let fx = two_sparse_packs().await;
    let mut survey = fx.survey(100).await;
    let ids: Vec<BlobId> = survey.plan.repack.iter().map(|p| p.pack_id).collect();
    assert_eq!(ids.len(), 2);
    forge_survey(&mut survey, &HashSet::from([ids[0]]), false);

    let out = run_repack(&fx, &survey).await;
    assert_eq!(out.replaced, vec![ids[1]]);
    assert_eq!(out.skipped.len(), 1);
    assert_eq!(out.skipped[0].0, ids[0]);
    assert!(!out.skipped[0].1.is_empty());
    assert!(out.shard.is_some());
}

#[tokio::test]
async fn repack_uses_other_copy_in_same_pack() {
    let fx = two_sparse_packs().await;
    let mut survey = fx.survey(100).await;
    let ids: HashSet<BlobId> = survey.plan.repack.iter().map(|p| p.pack_id).collect();
    forge_survey(&mut survey, &ids, true);
    let (id, blob_type) = survey
        .live
        .iter()
        .find(|(id, t)| {
            survey
                .index
                .get(id, *t)
                .is_some_and(|l| ids.contains(&l.pack_id))
        })
        .unwrap();
    assert_eq!(survey.index.locations(id, *blob_type).len(), 2);

    let out = run_repack(&fx, &survey).await;
    assert!(out.skipped.is_empty(), "{:?}", out.skipped);
    assert_eq!(out.replaced.iter().copied().collect::<HashSet<_>>(), ids);
}

#[tokio::test]
async fn repack_skips_unopenable_prefix_and_size_mismatch() {
    let fx = two_sparse_packs().await;
    let survey = fx.survey(100).await;
    let ids: Vec<BlobId> = survey.plan.repack.iter().map(|p| p.pack_id).collect();
    let path = |id: &BlobId| fx.tmp.path().join("repo").join(pack_object_key(id));
    let mut garbled = fs::read(path(&ids[0])).unwrap();
    garbled[..PACK_PREFIX_LEN].fill(0xAB);
    fs::write(path(&ids[0]), garbled).unwrap();
    let mut longer = fs::read(path(&ids[1])).unwrap();
    longer.push(0);
    fs::write(path(&ids[1]), longer).unwrap();

    let out = run_repack(&fx, &survey).await;
    assert!(out.replaced.is_empty());
    assert!(out.shard.is_none());
    let skipped: HashSet<BlobId> = out.skipped.iter().map(|(id, _)| *id).collect();
    assert_eq!(skipped, ids.into_iter().collect());
}

#[tokio::test]
async fn repack_attests_its_shard() {
    let fx = two_sparse_packs().await;
    let survey = fx.survey(100).await;
    let out = run_repack(&fx, &survey).await;
    let shard = out.shard.expect("blobs were copied");
    let attested = read_attestations(&fx.store, fx.repo.read_keys())
        .await
        .unwrap();
    assert!(attested.shards.contains(&shard));
}
