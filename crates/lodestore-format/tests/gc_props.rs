mod adv_support;
mod gc_support;

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ops::Range;
use std::sync::Mutex;

use adv_support::{attacker_keys, forge_offsets, open_all_shards, plant_shard, plant_shard_where};
use bytes::Bytes;
use gc_support::{Cycle, Fx};
use lodestore_format::crypto::{
    BLOB_TYPE_DATA, BLOB_TYPE_TREE, BlobId, HPKE_ENC_LEN, SEALED_KEY_LEN,
};
use lodestore_format::gc::compact::compact_plan;
use lodestore_format::gc::mark::{LiveSet, ShardSet};
use lodestore_format::gc::plan::{PlanOptions, plan};
use lodestore_format::pack::{PACK_PREFIX_LEN, pack_object_key, parse_entries};
use lodestore_format::pb;
use lodestore_format::store::{
    BlobDelete, BlobError, BlobPurge, BlobStore, ListEntry, LocalFsStore, PurgeOutcome,
};
use lodestore_format::writer::BackupOutcome;
use proptest::prelude::*;
use proptest::test_runner::{
    Config, FileFailurePersistence, RngAlgorithm, TestCaseError, TestRng, TestRunner,
};
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{Rng, SeedableRng};

const MIB: u64 = 1 << 20;

/// A store that lists in an order chosen by the seed of the case, because the
/// order of a listing changes how shards load, and that notes every read with
/// the number of bytes returned, a failed read as zero.
struct Hostile<S> {
    inner: S,
    seed: u64,
    reads: Mutex<Vec<(String, u64)>>,
}

impl<S> Hostile<S> {
    fn new(inner: S, seed: u64) -> Self {
        Hostile {
            inner,
            seed,
            reads: Mutex::new(Vec::new()),
        }
    }

    fn take_reads(&self) -> Vec<(String, u64)> {
        std::mem::take(&mut *self.reads.lock().unwrap())
    }

    fn note(&self, key: &str, got: Result<&Bytes, &BlobError>) {
        let len = got.map_or(0, |b| b.len() as u64);
        self.reads.lock().unwrap().push((key.to_owned(), len));
    }
}

fn shuffle<T>(items: &mut [T], seed: u64) {
    let mut rng = ChaCha20Rng::seed_from_u64(seed);
    for i in (1..items.len()).rev() {
        let j = usize::try_from(rng.next_u64() % (i as u64 + 1)).unwrap();
        items.swap(i, j);
    }
}

impl<S: BlobStore> BlobStore for Hostile<S> {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<(), BlobError> {
        self.inner.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        let out = self.inner.get(key).await;
        self.note(key, out.as_ref());
        out
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, BlobError> {
        let out = self.inner.get_range(key, range).await;
        self.note(key, out.as_ref());
        out
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        let mut out = self.inner.list(prefix).await?;
        shuffle(&mut out, self.seed);
        Ok(out)
    }

    async fn ensure_prefixes(&self, prefixes: &[&str]) -> Result<(), BlobError> {
        self.inner.ensure_prefixes(prefixes).await
    }
}

impl<S: BlobDelete> BlobDelete for Hostile<S> {
    async fn delete(&self, key: &str) -> Result<(), BlobError> {
        self.inner.delete(key).await
    }
}

impl<S: BlobPurge> BlobPurge for Hostile<S> {
    async fn purge(&self, key: &str) -> PurgeOutcome {
        self.inner.purge(key).await
    }

    async fn deleted_keys(
        &self,
        prefix: &str,
        older_than_ms: i64,
    ) -> Result<Vec<String>, BlobError> {
        self.inner.deleted_keys(prefix, older_than_ms).await
    }
}

fn bytes_under(reads: &[(String, u64)], prefix: &str) -> u64 {
    reads
        .iter()
        .filter(|(k, _)| k.starts_with(prefix))
        .map(|(_, n)| n)
        .sum()
}

type Hfx = Fx<Hostile<LocalFsStore>>;

fn size_of(seed: u8) -> usize {
    (8 + usize::from(seed % 5) * 6) << 10
}

fn unique_seed(backup: usize) -> u8 {
    100 + u8::try_from(backup).unwrap()
}

/// Equal seeds are equal files, so a seed stands for a piece of content.
fn make_files(seeds: &BTreeSet<u8>) -> Vec<(String, u8, usize)> {
    seeds
        .iter()
        .map(|s| (format!("f{s}"), *s, size_of(*s)))
        .collect()
}

async fn backup_of(fx: &Hfx, label: &str, seeds: &BTreeSet<u8>) -> BackupOutcome {
    let owned = make_files(seeds);
    let files: Vec<(&str, u8, usize)> = owned.iter().map(|(n, s, z)| (&**n, *s, *z)).collect();
    fx.backup(label, &files).await
}

/// What a host with writer keys puts into `index/`; the numbers pick records
/// and shards of the real index. A real `supersedes` cycle cannot be written:
/// a shard is named by the hash of its bytes, so it cannot name one that
/// names it back.
#[derive(Debug, Clone)]
enum Plant {
    /// The records of one pack, every blob one byte off its frame.
    Forge(usize),
    /// The forged records of a real shard, superseding it.
    Supersede(usize),
    /// A forged shard superseding a real one, and a second one superseding it.
    Chain(usize, usize),
    /// Forged records superseding a shard that does not exist.
    DanglingSupersedes(usize),
    /// A record that names a pack that is not in the bucket.
    DanglingPack(usize),
}

/// A backup that opens and commits while the cycle runs.
#[derive(Debug, Clone)]
struct Mid {
    placement: usize,
    resurrect: bool,
    pool: Vec<u8>,
}

/// Where a backup opens and commits: the number of GC steps already done. One
/// opened before the manifest is published must commit before the verification.
fn placements() -> Vec<(usize, usize)> {
    let (published, verified) = (2, 2);
    (0..=GC_STEPS)
        .flat_map(|open| (open..=GC_STEPS).map(move |commit| (open, commit)))
        .filter(|(open, commit)| *open >= published || *commit <= verified)
        .collect()
}

const GC_STEPS: usize = 5;

#[derive(Debug, Clone)]
struct Case {
    seed: u64,
    /// Content from the shared pool of each backup, and whether it is forgotten.
    backups: Vec<(Vec<u8>, bool)>,
    plants: Vec<Plant>,
    mid: Option<Mid>,
}

impl Case {
    /// At least one backup is forgotten and at least one kept.
    fn forgotten(&self) -> Vec<bool> {
        let mut out: Vec<bool> = self.backups.iter().map(|b| b.1).collect();
        if out.iter().all(|f| *f) {
            *out.last_mut().unwrap() = false;
        } else if out.iter().all(|f| !*f) {
            out[0] = true;
        }
        out
    }
}

fn plant_strategy() -> impl Strategy<Value = Plant> {
    prop_oneof![
        (0..16usize).prop_map(Plant::Forge),
        (0..16usize).prop_map(Plant::Supersede),
        (0..16usize, 0..16usize).prop_map(|(i, j)| Plant::Chain(i, j)),
        (0..16usize).prop_map(Plant::DanglingSupersedes),
        (0..16usize).prop_map(Plant::DanglingPack),
    ]
}

fn case_strategy() -> impl Strategy<Value = Case> {
    let mid = prop::option::weighted(
        0.5,
        (
            0..placements().len(),
            any::<bool>(),
            prop::collection::vec(1u8..=4, 0..=2),
        )
            .prop_map(|(placement, resurrect, pool)| Mid {
                placement,
                resurrect,
                pool,
            }),
    );
    (
        any::<u64>(),
        prop::collection::vec(
            (prop::collection::vec(1u8..=4, 1..=3), any::<bool>()),
            2..=4,
        ),
        prop::collection::vec(plant_strategy(), 0..=3),
        mid,
    )
        .prop_map(|(seed, backups, plants, mid)| Case {
            seed,
            backups,
            plants,
            mid,
        })
}

fn fake_id(seed: u64, n: u64) -> BlobId {
    ChaCha20Rng::seed_from_u64(seed ^ (n << 56)).random()
}

/// Puts the shards into the bucket with the keys of a writer. Returns how many
/// shards were planted, the pack objects their records name, and those among
/// them that do not exist.
async fn plant(fx: &Hfx, case: &Case) -> (u64, BTreeSet<String>, BTreeSet<String>) {
    let keys = attacker_keys(&fx.repo);
    let mut real = open_all_shards(&fx.store, &keys).await;
    real.sort_by_key(|(id, _)| *id);
    let records: Vec<pb::PackRecord> = real.iter().flat_map(|(_, s)| s.packs.clone()).collect();
    let (mut shards, mut named, mut missing) = (0, BTreeSet::new(), BTreeSet::new());
    let mut name = |record: &pb::PackRecord| {
        named.insert(pack_object_key(
            &record.pack_id.as_slice().try_into().unwrap(),
        ));
    };
    for (n, p) in case.plants.iter().enumerate() {
        let rec = |i: &usize| records[i % records.len()].clone();
        let n = n as u64;
        match p {
            Plant::Forge(i) => {
                name(&rec(i));
                plant_shard(&fx.store, &keys, vec![forge_offsets(&rec(i))], vec![]).await;
                shards += 1;
            }
            Plant::Supersede(i) => {
                let (id, shard) = &real[i % real.len()];
                shard.packs.iter().for_each(&mut name);
                let forged = shard.packs.iter().map(forge_offsets).collect();
                plant_shard(&fx.store, &keys, forged, vec![*id]).await;
                shards += 1;
            }
            Plant::Chain(i, j) => {
                name(&rec(i));
                name(&rec(j));
                let under = real[i % real.len()].0;
                let first =
                    plant_shard(&fx.store, &keys, vec![forge_offsets(&rec(i))], vec![under]).await;
                plant_shard(&fx.store, &keys, vec![forge_offsets(&rec(j))], vec![first]).await;
                shards += 2;
            }
            Plant::DanglingSupersedes(i) => {
                name(&rec(i));
                let ghost = fake_id(case.seed, n);
                plant_shard(&fx.store, &keys, vec![forge_offsets(&rec(i))], vec![ghost]).await;
                shards += 1;
            }
            Plant::DanglingPack(i) => {
                let ghost = fake_id(case.seed, n);
                let mut record = rec(i);
                record.pack_id = ghost.to_vec();
                missing.insert(pack_object_key(&ghost));
                plant_shard(&fx.store, &keys, vec![record], vec![]).await;
                shards += 1;
            }
        }
    }
    (shards, named, missing)
}

#[derive(Default)]
struct Tally {
    deleted: Cell<u32>,
    repacked: Cell<u32>,
    planted: Cell<u32>,
    interleaved: Cell<u32>,
    worst_packs_milli: Cell<u64>,
    worst_index_milli: Cell<u64>,
}

fn keys(listing: &BTreeMap<String, u64>) -> BTreeSet<String> {
    listing.keys().cloned().collect()
}

async fn run_case(case: &Case, tally: &Tally) -> Result<(), TestCaseError> {
    let fx: Hfx = Fx::seeded(case.seed)
        .await
        .wrap(|s| Hostile::new(s, case.seed));
    let forgotten = case.forgotten();

    let mut seeds_of: Vec<BTreeSet<u8>> = Vec::new();
    let mut owned: Vec<BTreeSet<u8>> = Vec::new();
    let mut created: Vec<BTreeSet<String>> = Vec::new();
    let mut ids = Vec::new();
    let mut stored = BTreeSet::new();
    for (i, (pool, _)) in case.backups.iter().enumerate() {
        let mut seeds: BTreeSet<u8> = pool.iter().copied().collect();
        seeds.insert(unique_seed(i));
        let before = keys(&fx.packs().await);
        let out = backup_of(&fx, &format!("b{i}"), &seeds).await;
        created.push(&keys(&fx.packs().await) - &before);
        owned.push(&seeds - &stored);
        stored.extend(seeds.iter().copied());
        seeds_of.push(seeds);
        ids.push(out.snapshot_id);
    }
    for (i, gone) in forgotten.iter().enumerate() {
        if *gone {
            fx.forget(&ids[i]).await;
        }
    }

    let kept: BTreeMap<_, _> = fx.sources.borrow().clone();
    let mut before = BTreeMap::new();
    for (id, src) in &kept {
        fx.store.take_reads();
        fx.restore_one(id, src).await;
        before.insert(*id, bytes_under(&fx.store.take_reads(), "packs/"));
    }

    let (planted, named, missing) = plant(&fx, case).await;
    let start = keys(&fx.packs().await);
    let mid = case.mid.as_ref().map(|m| {
        let mut seeds: BTreeSet<u8> = m.pool.iter().copied().collect();
        seeds.insert(200);
        if m.resurrect {
            let first = forgotten.iter().position(|f| *f).unwrap();
            seeds.insert(unique_seed(first));
        }
        let owned = make_files(&seeds);
        let files: Vec<(&str, u8, usize)> = owned.iter().map(|(n, s, z)| (&**n, *s, *z)).collect();
        (placements()[m.placement], fx.make_src("mid", &files), seeds)
    });
    let mut writer = None;
    let mut mid_id = None;
    let mut cycle = Cycle::new(&fx);
    let mut repacks = false;
    for gap in 0..=GC_STEPS {
        if let Some(((open, commit), src, _)) = &mid {
            if *open == gap {
                writer = Some(fx.open_writer().await);
            }
            if *commit == gap {
                mid_id = Some(
                    fx.commit(writer.as_mut().unwrap(), src.clone())
                        .await
                        .snapshot_id,
                );
            }
        }
        match gap {
            0 => cycle.survey(100).await,
            1 => {
                repacks = !cycle.survey.as_ref().unwrap().plan.repack.is_empty();
                cycle.repack_publish().await;
            }
            2 => {
                cycle.verify_compact().await;
            }
            3 => prop_assert!(cycle.sweep_shards().await.failed.is_empty()),
            4 => {
                prop_assert!(cycle.sweep_packs(usize::MAX).await.failed.is_empty());
                prop_assert!(cycle.finish().await.failed.is_empty());
            }
            _ => {}
        }
    }

    let after = fx.packs().await;
    let after_keys = keys(&after);
    let deleted = !(&start - &after_keys).is_empty();
    tally.deleted.set(tally.deleted.get() + u32::from(deleted));
    tally
        .repacked
        .set(tally.repacked.get() + u32::from(repacks));

    let index_total: u64 = fx.listing("index/").await.values().sum();
    tally
        .planted
        .set(tally.planted.get() + u32::from(planted > 0));
    tally
        .interleaved
        .set(tally.interleaved.get() + u32::from(mid.is_some()));
    let present = fx.sources.borrow().clone();
    let mut mid_reads = BTreeSet::new();
    for (id, src) in &present {
        fx.store.take_reads();
        fx.restore_one(id, src).await;
        let reads = fx.store.take_reads();
        if mid_id == Some(*id) {
            mid_reads.extend(reads.iter().map(|(k, _)| k.clone()));
        }
        for (key, _) in reads.iter().filter(|(k, _)| k.starts_with("packs/")) {
            prop_assert!(
                after_keys.contains(key) || missing.contains(key),
                "restore read {key}, not in the bucket"
            );
        }
        let packs = bytes_under(&reads, "packs/");
        if let Some(earlier) = before.get(id) {
            let bound = (planted + 1) * earlier + MIB;
            prop_assert!(packs <= bound, "packs/ read {packs} > {bound}");
            tally.worst_packs_milli.set(
                tally
                    .worst_packs_milli
                    .get()
                    .max(packs * 1000 / (*earlier).max(1)),
            );
        }
        let index = bytes_under(&reads, "index/");
        prop_assert!(
            index <= 2 * index_total,
            "index/ read {index} > 2 x {index_total}"
        );
        tally.worst_index_milli.set(
            tally
                .worst_index_milli
                .get()
                .max(index * 1000 / index_total.max(1)),
        );
    }

    for pack in &cycle.resurrected {
        let key = pack_object_key(pack);
        prop_assert!(
            named.contains(&key) || mid_reads.contains(&key),
            "{key} was brought back, but no planted record names it and the backup made during the cycle does not read it"
        );
    }

    let kept_seeds: BTreeSet<u8> = (0..case.backups.len())
        .filter(|i| !forgotten[*i])
        .flat_map(|i| seeds_of[i].iter().copied())
        .chain(mid.iter().flat_map(|(_, _, seeds)| seeds.iter().copied()))
        .collect();
    for i in (0..case.backups.len()).filter(|i| forgotten[*i]) {
        if owned[i].is_disjoint(&kept_seeds) {
            for pack in &created[i] {
                prop_assert!(
                    named.contains(pack) || !after_keys.contains(pack),
                    "dead pack {pack} is still there"
                );
            }
        }
    }
    Ok(())
}

#[test]
/// The differential check that what cleanup keeps is what restore needs, with
/// bounded reads and no empty cases.
fn gc_hostile_repository_restores_after_cycle() {
    let tally = Tally::default();
    let mut runner = TestRunner::new_with_rng(
        Config {
            failure_persistence: Some(Box::new(FileFailurePersistence::Direct(
                "tests/proptest-regressions/gc_props.txt",
            ))),
            ..Config::with_cases(16)
        },
        TestRng::deterministic_rng(RngAlgorithm::ChaCha),
    );
    runner
        .run(&case_strategy(), |case| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(run_case(&case, &tally))
        })
        .unwrap_or_else(|e| panic!("{e}"));
    println!(
        "gc_props counters: deleted={} repack={} planted={} interleaved={}",
        tally.deleted.get(),
        tally.repacked.get(),
        tally.planted.get(),
        tally.interleaved.get()
    );
    println!(
        "gc_props ratios: packs_milli={} index_milli={}",
        tally.worst_packs_milli.get(),
        tally.worst_index_milli.get()
    );
    assert!(tally.deleted.get() >= 8, "too few cases deleted a pack");
    assert!(tally.repacked.get() >= 4, "too few cases repacked");
}

/// A shard that lists, ahead of the real ones, the same blobs in packs the
/// bucket does not have: the survey must still place them in the real packs.
#[tokio::test]
async fn gc_survey_places_blobs_in_packs_that_exist() {
    let fx = Fx::new().await;
    fx.backup("a", &[("a", 1, 100 << 10), ("b", 2, 50 << 10)])
        .await;
    let keys = attacker_keys(&fx.repo);
    let real = open_all_shards(&fx.store, &keys).await;
    let lowest = real.iter().map(|(id, _)| *id).min().unwrap();
    let ghosts: Vec<pb::PackRecord> = real
        .iter()
        .flat_map(|(_, s)| s.packs.iter())
        .zip(1u8..)
        .map(|(record, n)| pb::PackRecord {
            pack_id: vec![n; 32],
            ..record.clone()
        })
        .collect();
    plant_shard_where(&fx.store, &keys, ghosts, Vec::new(), |id| *id < lowest).await;

    let mut cycle = Cycle::new(&fx);
    cycle.survey(100).await;
    cycle.repack_publish().await;
    cycle.verify_compact().await;
    cycle.sweep_shards().await;
    cycle.sweep_packs(usize::MAX).await;
    cycle.finish().await;
    fx.assert_all_restore().await;
}

/// The same planted shard in front of a sparse pack: the repack must copy the
/// live blobs out of the real pack, so verification has nothing to bring back
/// and the dead bytes are freed in one cycle.
#[tokio::test]
async fn gc_repack_moves_blobs_a_planted_shard_misplaces() {
    let fx = Fx::new().await;
    let first = fx
        .backup("ab", &[("a", 1, 100 << 10), ("b", 2, 900 << 10)])
        .await;
    fx.backup("a", &[("a", 1, 100 << 10)]).await;
    fx.forget(&first.snapshot_id).await;
    let keys = attacker_keys(&fx.repo);
    let real = open_all_shards(&fx.store, &keys).await;
    let lowest = real.iter().map(|(id, _)| *id).min().unwrap();
    let ghosts: Vec<pb::PackRecord> = real
        .iter()
        .flat_map(|(_, s)| s.packs.iter())
        .zip(1u8..)
        .map(|(record, n)| pb::PackRecord {
            pack_id: vec![n; 32],
            ..record.clone()
        })
        .collect();
    plant_shard_where(&fx.store, &keys, ghosts, Vec::new(), |id| *id < lowest).await;
    let before = fx.pack_bytes().await;

    let mut cycle = Cycle::new(&fx);
    cycle.survey(50).await;
    assert!(!cycle.survey.as_ref().unwrap().plan.repack.is_empty());
    cycle.repack_publish().await;
    assert!(!cycle.repack.replaced.is_empty());
    assert!(!cycle.repack.new_packs.is_empty());
    assert!(cycle.repack.bytes_written > 0);
    cycle.verify_compact().await;
    assert!(cycle.resurrected.is_empty(), "{:?}", cycle.resurrected);
    cycle.sweep_shards().await;
    cycle.sweep_packs(usize::MAX).await;
    cycle.finish().await;

    let packs = fx.packs().await;
    for pack in &cycle.repack.replaced {
        assert!(!packs.contains_key(&pack_object_key(pack)));
    }
    let after = fx.pack_bytes().await;
    println!("pack bytes: before={before} after={after}");
    assert!(after + (700 << 10) < before, "{before} -> {after}");
    let again = fx.survey(50).await;
    assert!(again.plan.repack.is_empty() && again.plan.delete.is_empty());
    fx.assert_all_restore().await;
}

const PACKS: u8 = 4;
const BLOBS: u8 = 5;
const PACK_SIZE: u64 = 8192;
const OFFSETS: [u32; 3] = [PACK_PREFIX_LEN as u32, 1300, 1500];
const ENC_LENS: [u32; 2] = [60, 100];
const NOW_MS: i64 = 1_000_000;

#[derive(Debug, Clone)]
struct RecSpec {
    pack: u8,
    /// Blob, tree or data, offset, length.
    entries: Vec<(u8, bool, u8, u8)>,
}

#[derive(Debug, Clone)]
struct ShardSpec {
    records: Vec<RecSpec>,
    supersedes: Vec<u8>,
    attested: bool,
}

fn shards_strategy() -> impl Strategy<Value = Vec<ShardSpec>> {
    let entry = (0..BLOBS, any::<bool>(), 0..3u8, 0..2u8);
    let record = (0..PACKS, prop::collection::vec(entry, 0..4))
        .prop_map(|(pack, entries)| RecSpec { pack, entries });
    let shard = (
        prop::collection::vec(record, 0..3),
        prop::collection::vec(any::<u8>(), 0..3),
        any::<bool>(),
    )
        .prop_map(|(records, supersedes, attested)| ShardSpec {
            records,
            supersedes,
            attested,
        });
    prop::collection::vec(shard, 1..5)
}

fn shard_id(i: usize) -> BlobId {
    [u8::try_from(i).unwrap() + 1; 32]
}

fn pack_id_of(pack: u8) -> BlobId {
    [pack + 50; 32]
}

fn blob_id_of(blob: u8) -> BlobId {
    [blob + 100; 32]
}

fn shard_set(specs: &[ShardSpec]) -> ShardSet {
    let n = specs.len();
    let shards = specs
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            let packs = spec
                .records
                .iter()
                .map(|r| {
                    let mut entries = Vec::new();
                    for (blob, tree, offset, len) in &r.entries {
                        let enc_len = ENC_LENS[usize::from(*len)];
                        entries.extend_from_slice(&blob_id_of(*blob));
                        entries.push(if *tree {
                            BLOB_TYPE_TREE
                        } else {
                            BLOB_TYPE_DATA
                        });
                        entries.extend_from_slice(&OFFSETS[usize::from(*offset)].to_le_bytes());
                        entries.extend_from_slice(&enc_len.to_le_bytes());
                        entries.extend_from_slice(&(enc_len - 41).to_le_bytes());
                    }
                    pb::PackRecord {
                        pack_id: pack_id_of(r.pack).to_vec(),
                        hpke_enc: vec![0; HPKE_ENC_LEN],
                        sealed_pack_key: vec![0; SEALED_KEY_LEN],
                        pack_size: PACK_SIZE,
                        entries,
                    }
                })
                .collect();
            let supersedes = spec
                .supersedes
                .iter()
                .map(|v| match usize::from(*v) % (n + 1) {
                    k if k < n => shard_id(k).to_vec(),
                    _ => vec![200; 32],
                })
                .collect();
            (shard_id(i), pb::IndexShard { packs, supersedes })
        })
        .collect();
    ShardSet {
        shards,
        attested: specs
            .iter()
            .enumerate()
            .filter(|(_, s)| s.attested)
            .map(|(i, _)| shard_id(i))
            .collect(),
        ..ShardSet::default()
    }
}

#[derive(Debug, Clone)]
struct PlanCase {
    shards: Vec<ShardSpec>,
    live: Vec<bool>,
    /// Age of each listed pack: old, young, or not reported.
    ages: Vec<u8>,
    unindexed: u8,
    pct: u8,
    max_candidates: usize,
    budget: Option<u64>,
    order: u64,
}

fn plan_case() -> impl Strategy<Value = PlanCase> {
    (
        shards_strategy(),
        prop::collection::vec(any::<bool>(), 1..32),
        prop::collection::vec(0..3u8, 1..8),
        0..3u8,
        0..=100u8,
        1..8usize,
        prop::option::of(0..40_000u64),
        any::<u64>(),
    )
        .prop_map(
            |(shards, live, ages, unindexed, pct, max_candidates, budget, order)| PlanCase {
                shards,
                live,
                ages,
                unindexed,
                pct,
                max_candidates,
                budget,
                order,
            },
        )
}

struct Planned {
    delete: Vec<BlobId>,
    repack: Vec<BlobId>,
    /// The pack the index places each live blob in.
    live_in: Vec<BlobId>,
}

fn run_plan(case: &PlanCase, order: Option<u64>) -> Planned {
    let shards = shard_set(&case.shards);
    let index = shards.index(&HashSet::new()).unwrap();
    let mut live = LiveSet::default();
    let blobs: BTreeSet<(BlobId, u8)> = shards
        .shards
        .iter()
        .flat_map(|(_, s)| s.packs.iter())
        .flat_map(|r| parse_entries(&r.entries).unwrap())
        .map(|e| (e.blob_id, e.blob_type))
        .collect();
    for (i, (id, blob_type)) in blobs.iter().enumerate() {
        if case.live[i % case.live.len()] {
            live.insert(*id, *blob_type);
        }
    }
    let indexed: BTreeSet<BlobId> = shards
        .shards
        .iter()
        .flat_map(|(_, s)| s.packs.iter())
        .map(|r| r.pack_id.as_slice().try_into().unwrap())
        .collect();
    let extra = (0..case.unindexed).map(|n| pack_id_of(PACKS + n));
    let mut listing: Vec<ListEntry> = indexed
        .iter()
        .copied()
        .chain(extra)
        .enumerate()
        .map(|(i, id)| ListEntry {
            key: pack_object_key(&id),
            size: PACK_SIZE + 2000 * (i as u64 % 2),
            modified_ms: match case.ages[i % case.ages.len()] {
                0 => Some(0),
                1 => Some(NOW_MS - 10),
                _ => None,
            },
        })
        .collect();
    if let Some(seed) = order {
        shuffle(&mut listing, seed);
    }
    let opts = PlanOptions {
        now_ms: NOW_MS,
        min_age_ms: 500,
        repack_below_pct: case.pct,
        max_repack_bytes: case.budget.unwrap_or(u64::MAX),
        max_candidates: case.max_candidates,
    };
    let out = plan(&shards, &index, &live, &listing, &opts).unwrap();
    let ids = |v: &[lodestore_format::gc::plan::PackStat]| v.iter().map(|p| p.pack_id).collect();
    Planned {
        delete: ids(&out.delete),
        repack: ids(&out.repack),
        live_in: live
            .iter()
            .map(|(id, t)| index.get(id, *t).unwrap().pack_id)
            .collect(),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn gc_plan_never_deletes_live(case in plan_case()) {
        let planned = run_plan(&case, None);
        for pack in &planned.delete {
            prop_assert!(!planned.live_in.contains(pack), "a pack with a live blob is to be deleted");
            prop_assert!(!planned.repack.contains(pack), "a pack is to be deleted and repacked");
        }
    }

    #[test]
    fn gc_plan_ignores_listing_order(case in plan_case()) {
        let (a, b) = (run_plan(&case, None), run_plan(&case, Some(case.order)));
        let set = |v: &[BlobId]| v.iter().copied().collect::<BTreeSet<_>>();
        prop_assert_eq!(set(&a.delete), set(&b.delete));
        prop_assert_eq!(set(&a.repack), set(&b.repack));
    }

    #[test]
    fn gc_compact_keeps_every_location(
        specs in shards_strategy(),
        chosen in prop::collection::vec(any::<bool>(), PACKS as usize),
    ) {
        let shards = shard_set(&specs);
        let candidates: HashSet<BlobId> = (0..PACKS)
            .filter(|p| chosen[usize::from(*p)])
            .map(pack_id_of)
            .collect();
        let out = compact_plan(&shards, &candidates);
        let kept: BTreeSet<_> = [&out.verified_records, &out.attested_records, &out.other_records]
            .into_iter()
            .flatten()
            .flat_map(|r| {
                let pack = r.pack_id.clone();
                parse_entries(&r.entries).unwrap().into_iter().map(move |e| (pack.clone(), e.blob_id, e.blob_type, e.offset, e.enc_len, e.ulen))
            })
            .collect();
        for (id, shard) in &shards.shards {
            if !out.rewrite.contains(id) {
                continue;
            }
            for record in &shard.packs {
                let pack: BlobId = record.pack_id.as_slice().try_into().unwrap();
                for e in parse_entries(&record.entries).unwrap() {
                    let place = (record.pack_id.clone(), e.blob_id, e.blob_type, e.offset, e.enc_len, e.ulen);
                    prop_assert_eq!(kept.contains(&place), !candidates.contains(&pack));
                }
            }
        }
    }
}
