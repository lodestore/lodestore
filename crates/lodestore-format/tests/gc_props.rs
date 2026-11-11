mod adv_support;
mod gc_support;

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::Mutex;

use bytes::Bytes;
use gc_support::{Cycle, Fx};
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

#[derive(Debug, Clone)]
struct Case {
    seed: u64,
    /// Content from the shared pool of each backup, and whether it is forgotten.
    backups: Vec<(Vec<u8>, bool)>,
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

fn case_strategy() -> impl Strategy<Value = Case> {
    (
        any::<u64>(),
        prop::collection::vec(
            (prop::collection::vec(1u8..=4, 1..=3), any::<bool>()),
            2..=4,
        ),
    )
        .prop_map(|(seed, backups)| Case { seed, backups })
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

    let start = keys(&fx.packs().await);
    let mut cycle = Cycle::new(&fx);
    cycle.survey(100).await;
    let repacks = !cycle.survey.as_ref().unwrap().plan.repack.is_empty();
    cycle.repack_publish().await;
    cycle.verify_compact().await;
    prop_assert!(cycle.sweep_shards().await.failed.is_empty());
    prop_assert!(cycle.sweep_packs(usize::MAX).await.failed.is_empty());
    prop_assert!(cycle.finish().await.failed.is_empty());

    let after = fx.packs().await;
    let after_keys = keys(&after);
    let deleted = !(&start - &after_keys).is_empty();
    tally.deleted.set(tally.deleted.get() + u32::from(deleted));
    tally
        .repacked
        .set(tally.repacked.get() + u32::from(repacks));

    let index_total: u64 = fx.listing("index/").await.values().sum();
    let planted = 0u64;
    for (id, src) in &kept {
        fx.store.take_reads();
        fx.restore_one(id, src).await;
        let reads = fx.store.take_reads();
        for (key, _) in reads.iter().filter(|(k, _)| k.starts_with("packs/")) {
            prop_assert!(
                after_keys.contains(key),
                "restore read {key}, not in the bucket"
            );
        }
        let packs = bytes_under(&reads, "packs/");
        let bound = (planted + 1) * before[id] + MIB;
        prop_assert!(packs <= bound, "packs/ read {packs} > {bound}");
        let index = bytes_under(&reads, "index/");
        prop_assert!(
            index <= 2 * index_total,
            "index/ read {index} > 2 x {index_total}"
        );
        let milli = |n: u64, d: u64| n * 1000 / d.max(1);
        tally
            .worst_packs_milli
            .set(tally.worst_packs_milli.get().max(milli(packs, before[id])));
        tally
            .worst_index_milli
            .set(tally.worst_index_milli.get().max(milli(index, index_total)));
    }

    let kept_seeds: BTreeSet<u8> = (0..case.backups.len())
        .filter(|i| !forgotten[*i])
        .flat_map(|i| seeds_of[i].iter().copied())
        .collect();
    for i in (0..case.backups.len()).filter(|i| forgotten[*i]) {
        if owned[i].is_disjoint(&kept_seeds) {
            for pack in &created[i] {
                prop_assert!(
                    !after_keys.contains(pack),
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
