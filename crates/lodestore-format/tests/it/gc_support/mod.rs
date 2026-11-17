#![allow(dead_code)]

//! A small repository in a temporary folder and the garbage collection steps
//! as separate calls, shared by the `gc_repack` and `gc_interleave` tests.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use lodestore_format::crypto::{BlobId, Entropy};
use lodestore_format::gc::compact::{CompactPlan, compact_plan, write_compacted};
use lodestore_format::gc::mark::load_shards;
use lodestore_format::gc::plan::PlanOptions;
use lodestore_format::gc::repack::{RepackOutcome, repack};
use lodestore_format::gc::sweep::{
    SweepOutcome, confirm_unreferenced, sweep_object, sweep_packs, sweep_shards,
};
use lodestore_format::gc::verify::verify;
use lodestore_format::gc::{Survey, candidates, survey};
use lodestore_format::index::{hex32, parse_hex32};
use lodestore_format::keys::KdfCost;
use lodestore_format::locks::read_gc_locks;
use lodestore_format::locks::{LockId, gc_manifest_key, new_lock_id, publish_gc_manifest};
use lodestore_format::pack::pack_object_key;
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::snapshot::snapshot_object_key;
use lodestore_format::store::{BlobDelete, BlobPurge, BlobStore, LocalFsStore};
use lodestore_format::writer::{BackupOptions, BackupOutcome, Writer};
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{Rng, SeedableRng};

const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

pub fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

pub fn plan_opts(repack_below_pct: u8) -> PlanOptions {
    PlanOptions {
        now_ms: now_ms() + 1000,
        min_age_ms: 0,
        repack_below_pct,
        max_repack_bytes: u64::MAX,
        max_candidates: 1000,
    }
}

/// `(file name, seed, size)`; equal seed and size give equal content.
pub type Files<'a> = &'a [(&'a str, u8, usize)];

pub struct Fx<S = LocalFsStore> {
    pub tmp: tempfile::TempDir,
    pub store: S,
    pub repo: Repository,
    /// Every backup made through [`Fx::make_src`], by snapshot id.
    pub sources: RefCell<BTreeMap<BlobId, PathBuf>>,
    seeds: Option<RefCell<ChaCha20Rng>>,
}

impl Fx {
    pub async fn new() -> Fx {
        Fx::init(Entropy::os(), None).await
    }

    /// Keys, writers and collection cycles all draw from a generator seeded
    /// with `seed`, so a run repeats.
    pub async fn seeded(seed: u64) -> Fx {
        let mut seeds = ChaCha20Rng::seed_from_u64(seed);
        let entropy = Entropy::from_rng(ChaCha20Rng::seed_from_u64(seeds.next_u64()));
        Fx::init(entropy, Some(RefCell::new(seeds))).await
    }

    async fn init(mut entropy: Entropy, seeds: Option<RefCell<ChaCha20Rng>>) -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalFsStore::new(tmp.path().join("repo"));
        let options = InitOptions {
            kdf: LOW_KDF,
            ..Default::default()
        };
        PendingInit::generate(&mut entropy, options)
            .unwrap()
            .commit(&store, b"pw", &mut entropy)
            .await
            .unwrap();
        let repo = Repository::open(&store, b"pw").await.unwrap();
        Fx {
            tmp,
            store,
            repo,
            sources: RefCell::new(BTreeMap::new()),
            seeds,
        }
    }

    /// A copy of the repository in a new temporary folder. Snapshots keep
    /// pointing at the source folders of the original, which must outlive it.
    pub async fn fork(&self) -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        copy_dir(self.tmp.path(), tmp.path());
        let store = LocalFsStore::new(tmp.path().join("repo"));
        let repo = Repository::open(&store, b"pw").await.unwrap();
        let sources = self.sources.borrow().clone();
        Fx {
            tmp,
            store,
            repo,
            sources: RefCell::new(sources),
            seeds: self.seeds.clone(),
        }
    }

    /// The same repository behind another view of its store.
    pub fn wrap<T: BlobStore>(self, f: impl FnOnce(LocalFsStore) -> T) -> Fx<T> {
        Fx {
            tmp: self.tmp,
            store: f(self.store),
            repo: self.repo,
            sources: self.sources,
            seeds: self.seeds,
        }
    }
}

impl<S: BlobStore> Fx<S> {
    /// The source of randomness for the next writer or cycle.
    pub fn entropy(&self) -> Entropy {
        match &self.seeds {
            None => Entropy::os(),
            Some(seeds) => {
                let next = seeds.borrow_mut().next_u64();
                Entropy::from_rng(ChaCha20Rng::seed_from_u64(next))
            }
        }
    }

    pub fn make_src(&self, label: &str, files: Files<'_>) -> PathBuf {
        let src = self.tmp.path().join(format!("src-{label}"));
        fs::create_dir(&src).unwrap();
        write_files(&src, files);
        src
    }

    pub async fn open_writer(&self) -> Writer<&S> {
        let keys = self.repo.read_keys().write_keys(self.repo.config_hash());
        Writer::new(&self.store, keys, self.repo.config())
            .await
            .unwrap()
            .with_entropy(self.entropy())
    }

    pub async fn commit(&self, w: &mut Writer<&S>, src: PathBuf) -> BackupOutcome {
        let out = w
            .backup(
                std::slice::from_ref(&src),
                BackupOptions::default(),
                &|_| {},
            )
            .await
            .unwrap();
        self.sources.borrow_mut().insert(out.snapshot_id, src);
        out
    }

    pub async fn backup(&self, label: &str, files: Files<'_>) -> BackupOutcome {
        let src = self.make_src(label, files);
        let mut w = self.open_writer().await;
        self.commit(&mut w, src).await
    }

    /// Removes the snapshot object, as retention does.
    pub async fn forget(&self, id: &BlobId)
    where
        S: BlobDelete,
    {
        self.store.delete(&snapshot_object_key(id)).await.unwrap();
        self.sources.borrow_mut().remove(id);
    }

    pub async fn packs(&self) -> BTreeMap<String, u64> {
        self.listing("packs/").await
    }

    /// Index shards that describe packs; an empty one only commits a
    /// compaction and goes with the next one.
    pub async fn record_shards(&self) -> usize {
        load_shards(&self.store, self.repo.read_keys())
            .await
            .unwrap()
            .shards
            .iter()
            .filter(|(_, shard)| !shard.packs.is_empty())
            .count()
    }

    pub async fn listing(&self, prefix: &str) -> BTreeMap<String, u64> {
        self.store
            .list(prefix)
            .await
            .unwrap()
            .into_iter()
            .map(|e| (e.key, e.size))
            .collect()
    }

    pub async fn pack_bytes(&self) -> u64 {
        self.packs().await.values().sum()
    }

    pub async fn survey(&self, pct: u8) -> Survey {
        survey(
            &self.store,
            self.repo.read_keys(),
            self.repo.config(),
            &plan_opts(pct),
            &|_| {},
        )
        .await
        .unwrap()
    }

    /// Restores every snapshot still in `sources` with a reader that has seen
    /// nothing before, and compares the files byte for byte.
    pub async fn assert_all_restore(&self) {
        let sources = self.sources.borrow().clone();
        for (id, src) in &sources {
            self.restore_one(id, src).await;
        }
    }

    /// Restores one snapshot with a reader that has seen nothing before and
    /// compares the files byte for byte with `src`.
    pub async fn restore_one(&self, id: &BlobId, src: &Path) {
        let mut reader = Reader::open(
            &self.store,
            self.repo.read_keys().duplicate(),
            self.repo.config(),
        )
        .unwrap();
        let target = tempfile::tempdir_in(self.tmp.path()).unwrap();
        let report = reader
            .restore(id, target.path(), &RestoreOptions::default(), &|_| {})
            .await
            .unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let canonical = src.canonicalize().unwrap();
        let restored = target.path().join(canonical.strip_prefix("/").unwrap());
        assert_same_files(src, &restored);
    }

    pub fn bucket(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            let Ok(read) = fs::read_dir(dir) else { return };
            for e in read {
                let path = e.unwrap().path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else {
                    let rel = path.strip_prefix(root).unwrap().to_path_buf();
                    out.insert(rel, fs::read(&path).unwrap());
                }
            }
        }
        let root = self.tmp.path().join("repo");
        let mut out = BTreeMap::new();
        walk(&root, &root, &mut out);
        out
    }
}

fn copy_dir(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            fs::create_dir(&target).unwrap();
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Writes the files, creating the folders in their names.
pub fn write_files(dir: &Path, files: Files<'_>) {
    for (name, seed, size) in files {
        let mut data = vec![0u8; *size];
        ChaCha20Rng::from_seed([*seed; 32]).fill_bytes(&mut data);
        let path = dir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, &data).unwrap();
    }
}

pub fn assert_same_files(a: &Path, b: &Path) {
    let names = |dir: &Path| -> BTreeSet<_> {
        fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            .map(|e| e.unwrap().file_name())
            .collect()
    };
    assert_eq!(names(a), names(b), "files in {}", a.display());
    for name in names(a) {
        let (pa, pb) = (a.join(&name), b.join(&name));
        if pa.is_dir() {
            assert_same_files(&pa, &pb);
        } else {
            assert_eq!(fs::read(&pa).unwrap(), fs::read(&pb).unwrap(), "{pa:?}");
        }
    }
}

/// One collection, step by step.
pub struct Cycle<'a, S = LocalFsStore> {
    pub fx: &'a Fx<S>,
    pub gc_id: LockId,
    pub survey: Option<Survey>,
    pub repack: RepackOutcome,
    /// What the manifest names.
    pub published: HashSet<BlobId>,
    /// What is left of it after `verify`.
    pub candidates: HashSet<BlobId>,
    pub rewrite: Vec<BlobId>,
    pub resurrected: Vec<BlobId>,
    pub entropy: Entropy,
}

impl<'a, S: BlobStore + BlobDelete + BlobPurge> Cycle<'a, S> {
    pub fn new(fx: &'a Fx<S>) -> Self {
        let mut entropy = fx.entropy();
        Cycle {
            fx,
            gc_id: new_lock_id(&mut entropy),
            survey: None,
            repack: RepackOutcome::default(),
            published: HashSet::new(),
            candidates: HashSet::new(),
            rewrite: Vec::new(),
            resurrected: Vec::new(),
            entropy,
        }
    }

    /// Picks the work up again from the manifest in the bucket, as a restarted
    /// server would.
    pub async fn resume(fx: &'a Fx<S>) -> Self {
        let mut c = Cycle::new(fx);
        let keys = fx.repo.read_keys();
        let locks = read_gc_locks(&fx.store, keys.k_idx(), keys.repo_id())
            .await
            .unwrap();
        assert_eq!(locks.manifests.len(), 1);
        c.gc_id = locks.manifests[0].gc_id;
        c.published = locks.candidate_packs();
        c.candidates = c.published.clone();
        c
    }

    pub async fn survey(&mut self, repack_below_pct: u8) {
        self.survey = Some(self.fx.survey(repack_below_pct).await);
    }

    pub async fn repack_publish(&mut self) {
        let fx = self.fx;
        let survey = self.survey.as_ref().expect("survey first");
        self.repack = repack(
            &fx.store,
            fx.repo.read_keys(),
            survey,
            &mut self.entropy,
            &|_, _| {},
        )
        .await
        .unwrap();
        let list = candidates(&survey.plan, &self.repack);
        let keys = fx.repo.read_keys();
        publish_gc_manifest(
            &fx.store,
            keys.k_idx(),
            keys.repo_id(),
            &self.gc_id,
            &list,
            &mut self.entropy,
        )
        .await
        .unwrap();
        self.published = list.into_iter().collect();
        self.candidates = self.published.clone();
    }

    pub async fn verify_compact(&mut self) -> CompactPlan {
        let fx = self.fx;
        let verified = verify(
            &fx.store,
            fx.repo.read_keys(),
            fx.repo.config(),
            &self.published,
            &|_, _| {},
        )
        .await
        .unwrap();
        self.candidates = verified.candidates.clone();
        self.resurrected = verified.resurrected.clone();
        let plan = compact_plan(&verified.shards, &self.candidates);
        write_compacted(
            &fx.store,
            fx.repo.read_keys(),
            &plan,
            &verified.live,
            &self.candidates,
            &mut self.entropy,
        )
        .await
        .unwrap();
        self.rewrite = plan.rewrite.clone();
        plan
    }

    pub async fn sweep_shards(&self) -> SweepOutcome {
        sweep_shards(&self.fx.store, &self.rewrite).await
    }

    /// Deletes the first `limit` candidate packs still in the bucket.
    pub async fn sweep_packs(&self, limit: usize) -> SweepOutcome {
        let fx = self.fx;
        confirm_unreferenced(&fx.store, fx.repo.read_keys(), &self.candidates)
            .await
            .unwrap();
        let sizes: HashMap<String, u64> = fx.packs().await.into_iter().collect();
        let mut packs: Vec<(BlobId, u64)> = self
            .candidates
            .iter()
            .filter_map(|id| sizes.get(&pack_object_key(id)).map(|s| (*id, *s)))
            .collect();
        packs.sort();
        packs.truncate(limit);
        sweep_packs(&fx.store, &packs).await
    }

    pub async fn finish(&self) -> SweepOutcome {
        sweep_object(&self.fx.store, &gc_manifest_key(&self.gc_id), 0).await
    }
}

pub fn pack_ids(listing: &BTreeMap<String, u64>) -> BTreeSet<BlobId> {
    listing
        .keys()
        .map(|k| parse_hex32(k.rsplit('/').next().unwrap()).unwrap())
        .collect()
}

pub fn hex(id: &BlobId) -> String {
    hex32(id)
}
