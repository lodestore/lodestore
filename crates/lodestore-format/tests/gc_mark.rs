//! `gc::survey` reads the bucket and plans; it never writes, and it stops on
//! anything it cannot follow.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use lodestore_format::Error;
use lodestore_format::attest::put_attestation;
use lodestore_format::crypto::{BLOB_TYPE_DATA, BLOB_TYPE_TREE, BlobId, Entropy};
use lodestore_format::gc::mark::{LiveSet, MarkPolicy, ShardSet, load_shards};
use lodestore_format::gc::plan::{PackStat, PlanOptions, plan};
use lodestore_format::gc::{Survey, survey, survey_with};
use lodestore_format::index::hex32;
use lodestore_format::keys::KdfCost;
use lodestore_format::pack::{PACK_PREFIX_LEN, pack_object_key, parse_entries};
use lodestore_format::pb;
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::snapshot::snapshot_object_key;
use lodestore_format::store::{BlobDelete, BlobStore, ListEntry, LocalFsStore};
use lodestore_format::writer::{BackupOptions, BackupOutcome, Writer};
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{Rng, SeedableRng};

const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

fn opts() -> PlanOptions {
    PlanOptions {
        now_ms: now_ms() + 1000,
        min_age_ms: 0,
        repack_below_pct: 0,
        max_repack_bytes: u64::MAX,
        max_candidates: 1000,
    }
}

struct Fx {
    tmp: tempfile::TempDir,
    store: LocalFsStore,
    repo: Repository,
}

impl Fx {
    async fn new() -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalFsStore::new(tmp.path().join("repo"));
        let mut entropy = Entropy::os();
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
        Fx { tmp, store, repo }
    }

    async fn backup(&self, seed: u8) -> BackupOutcome {
        let src = self.tmp.path().join(format!("src{seed}"));
        fs::create_dir(&src).unwrap();
        let mut data = vec![0u8; 1 << 20];
        ChaCha20Rng::from_seed([seed; 32]).fill_bytes(&mut data);
        fs::write(src.join("random.bin"), &data).unwrap();
        let keys = self.repo.read_keys().write_keys(self.repo.config_hash());
        let mut w = Writer::new(&self.store, keys, self.repo.config())
            .await
            .unwrap();
        w.backup(&[src], BackupOptions::default(), &|_| {})
            .await
            .unwrap()
    }

    async fn packs(&self) -> BTreeMap<String, u64> {
        self.store
            .list("packs/")
            .await
            .unwrap()
            .into_iter()
            .map(|e| (e.key, e.size))
            .collect()
    }

    async fn survey(&self, opts: &PlanOptions) -> Result<Survey, Error> {
        survey(
            &self.store,
            self.repo.read_keys(),
            self.repo.config(),
            opts,
            &|_| {},
        )
        .await
    }

    async fn survey_with(&self, policy: &MarkPolicy) -> Result<Survey, Error> {
        survey_with(
            &self.store,
            self.repo.read_keys(),
            self.repo.config(),
            &opts(),
            policy,
            &|_| {},
        )
        .await
    }

    async fn packs_of_type(&self, blob_type: u8) -> Vec<String> {
        let shards = load_shards(&self.store, self.repo.read_keys())
            .await
            .unwrap();
        let mut out = Vec::new();
        for (_, shard) in &shards.shards {
            for p in &shard.packs {
                let has = parse_entries(&p.entries)
                    .unwrap()
                    .iter()
                    .any(|e| e.blob_type == blob_type);
                if has {
                    out.push(pack_object_key(&p.pack_id.as_slice().try_into().unwrap()));
                }
            }
        }
        out
    }

    fn bucket(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for e in fs::read_dir(dir).unwrap() {
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

    async fn put(&self, key: &str, bytes: &[u8]) {
        self.store
            .put_if_absent(key, bytes.to_vec().into())
            .await
            .unwrap();
    }
}

fn keys_of(stats: &[PackStat]) -> BTreeSet<String> {
    stats.iter().map(|p| pack_object_key(&p.pack_id)).collect()
}

fn corrupt_object(e: Error) -> String {
    match e {
        Error::Corrupt { object, .. } => object,
        e => panic!("expected Corrupt, got {e:?}"),
    }
}

#[tokio::test]
async fn gc_mark_forgotten_snapshot_frees_packs() {
    let fx = Fx::new().await;
    let a = fx.backup(1).await;
    let packs_a = fx.packs().await;
    fx.backup(2).await;
    fx.store
        .delete(&snapshot_object_key(&a.snapshot_id))
        .await
        .unwrap();

    let s = fx.survey(&opts()).await.unwrap();
    assert_eq!(s.snapshots.len(), 1);
    assert_eq!(
        keys_of(&s.plan.delete),
        packs_a.keys().cloned().collect::<BTreeSet<_>>()
    );
    assert!(s.plan.repack.is_empty());
    assert_eq!(s.plan.bytes_freed(), packs_a.values().sum::<u64>());
    assert_eq!(s.plan.packs_total, fx.packs().await.len() as u64);
}

#[tokio::test]
async fn gc_mark_bucket_unchanged() {
    let fx = Fx::new().await;
    let a = fx.backup(1).await;
    fx.backup(2).await;
    fx.store
        .delete(&snapshot_object_key(&a.snapshot_id))
        .await
        .unwrap();
    let before = fx.bucket();
    fx.survey(&opts()).await.unwrap();
    assert_eq!(fx.bucket(), before);
}

#[tokio::test]
async fn gc_mark_snapshot_in_bucket_is_live() {
    let fx = Fx::new().await;
    fx.backup(1).await;
    fx.backup(2).await;
    let s = fx.survey(&opts()).await.unwrap();
    assert_eq!(s.snapshots.len(), 2);
    assert!(s.plan.delete.is_empty() && s.plan.repack.is_empty());
    assert!(s.plan.unindexed.is_empty());
    assert!(!s.live.is_empty());
}

#[tokio::test]
async fn gc_mark_fail_closed_unopenable_snapshot() {
    let fx = Fx::new().await;
    fx.backup(1).await;
    let junk = b"not a snapshot envelope".to_vec();
    let id = *blake3::hash(&junk).as_bytes();
    let key = snapshot_object_key(&id);
    fx.put(&key, &junk).await;
    let before = fx.bucket();

    let err = fx.survey(&opts()).await.err().unwrap();
    assert_eq!(corrupt_object(err), key);
    assert_eq!(fx.bucket(), before);
}

fn quarantining() -> MarkPolicy {
    MarkPolicy {
        quarantine: true,
        ..MarkPolicy::default()
    }
}

/// Junk under a valid snapshot name: it hashes to its name but does not open.
async fn plant_junk(fx: &Fx) -> (BlobId, String) {
    let junk = b"not a snapshot envelope".to_vec();
    let id = *blake3::hash(&junk).as_bytes();
    let key = snapshot_object_key(&id);
    fx.put(&key, &junk).await;
    (id, key)
}

#[tokio::test]
async fn gc_mark_quarantine_unopenable_snapshot() {
    let fx = Fx::new().await;
    let good = fx.backup(1).await;
    fx.backup(2).await;
    fx.store
        .delete(&snapshot_object_key(&good.snapshot_id))
        .await
        .unwrap();
    let kept = fx.backup(3).await;
    let (_, key) = plant_junk(&fx).await;

    assert!(fx.survey(&opts()).await.is_err());
    let s = fx.survey_with(&quarantining()).await.unwrap();
    assert_eq!(s.quarantined.len(), 1);
    assert_eq!(s.quarantined[0].key, key);
    assert!(!s.quarantined[0].bound);
    assert!(s.plan.delete.is_empty() && s.plan.repack.is_empty());
    assert!(s.snapshots.contains(&kept.snapshot_id));
    assert_eq!(s.snapshots.len(), 2);
}

#[tokio::test]
async fn gc_mark_quarantine_excluded_snapshot_is_skipped() {
    let fx = Fx::new().await;
    let gone = fx.backup(1).await;
    fx.backup(2).await;
    fx.store
        .delete(&snapshot_object_key(&gone.snapshot_id))
        .await
        .unwrap();
    let (id, key) = plant_junk(&fx).await;
    let policy = MarkPolicy {
        excluded: [id].into(),
        ..quarantining()
    };

    let s = fx.survey_with(&policy).await.unwrap();
    assert!(s.quarantined.is_empty());
    assert_eq!(s.ignored.len(), 1);
    assert_eq!(s.ignored[0].0, key);
    assert!(s.ignored[0].1.contains("excluded by an administrator"));
    assert!(!s.plan.delete.is_empty());
}

#[tokio::test]
async fn gc_mark_quarantine_bound_snapshot() {
    let fx = Fx::new().await;
    fx.backup(1).await;
    let (id, key) = plant_junk(&fx).await;
    let policy = MarkPolicy {
        bound: [id].into(),
        excluded: [id].into(),
        ..quarantining()
    };

    let s = fx.survey_with(&policy).await.unwrap();
    assert_eq!(s.quarantined.len(), 1);
    assert_eq!(s.quarantined[0].key, key);
    assert!(s.quarantined[0].bound);
    assert!(s.ignored.is_empty());
}

#[tokio::test]
async fn gc_mark_planted_object_ignored() {
    let fx = Fx::new().await;
    fx.backup(1).await;
    let key = format!("snapshots/{}", hex32(&[7; 32]));
    fx.put(&key, b"planted").await;
    fx.put("snapshots/not-hex", b"junk").await;

    let s = fx.survey(&opts()).await.unwrap();
    assert_eq!(s.snapshots.len(), 1);
    assert_eq!(s.ignored.len(), 1);
    assert_eq!(s.ignored[0].0, key);
}

#[tokio::test]
async fn gc_mark_fail_closed_missing_tree() {
    let fx = Fx::new().await;
    fx.backup(1).await;
    for key in fx.packs_of_type(BLOB_TYPE_TREE).await {
        fx.store.delete(&key).await.unwrap();
    }
    let before = fx.bucket();
    let err = fx.survey(&opts()).await.err().unwrap();
    assert!(matches!(err, Error::Corrupt { .. }), "{err:?}");
    assert_eq!(fx.bucket(), before);
}

#[tokio::test]
async fn gc_mark_fail_closed_unindexed_blob() {
    let fx = Fx::new().await;
    fx.backup(1).await;
    for e in fx.store.list("index/").await.unwrap() {
        fx.store.delete(&e.key).await.unwrap();
    }
    let before = fx.bucket();
    let err = fx.survey(&opts()).await.err().unwrap();
    assert!(
        err.to_string().contains("no index shard describes"),
        "{err}"
    );
    assert_eq!(fx.bucket(), before);
}

#[tokio::test]
async fn gc_mark_fail_closed_missing_pack() {
    let fx = Fx::new().await;
    fx.backup(1).await;
    let data = fx.packs_of_type(BLOB_TYPE_DATA).await;
    let tree = fx.packs_of_type(BLOB_TYPE_TREE).await;
    let victim = data.iter().find(|k| !tree.contains(k)).unwrap().clone();
    fx.store.delete(&victim).await.unwrap();
    let before = fx.bucket();

    let err = fx.survey(&opts()).await.err().unwrap();
    assert_eq!(corrupt_object(err), victim);
    assert_eq!(fx.bucket(), before);
}

const BLOB_LEN: u32 = 1000;
const BLOBS_PER_PACK: usize = 10;
const SYNTH_PACK_SIZE: u64 = (PACK_PREFIX_LEN + BLOBS_PER_PACK * BLOB_LEN as usize + 200) as u64;

fn blob(pack: u8, n: usize) -> BlobId {
    let mut id = [0u8; 32];
    id[0] = pack;
    id[1] = n as u8;
    id
}

fn record(pack: u8, blobs: &[BlobId]) -> pb::PackRecord {
    let mut entries = Vec::new();
    for (i, id) in blobs.iter().enumerate() {
        entries.extend_from_slice(id);
        entries.push(BLOB_TYPE_DATA);
        entries.extend_from_slice(&((PACK_PREFIX_LEN as u32) + i as u32 * BLOB_LEN).to_le_bytes());
        entries.extend_from_slice(&BLOB_LEN.to_le_bytes());
        entries.extend_from_slice(&100u32.to_le_bytes());
    }
    pb::PackRecord {
        pack_id: vec![pack; 32],
        hpke_enc: vec![0; lodestore_format::crypto::HPKE_ENC_LEN],
        sealed_pack_key: vec![0; lodestore_format::crypto::SEALED_KEY_LEN],
        pack_size: SYNTH_PACK_SIZE,
        entries,
    }
}

fn listing(pack: u8, modified_ms: Option<i64>) -> ListEntry {
    ListEntry {
        key: pack_object_key(&[pack; 32]),
        size: SYNTH_PACK_SIZE,
        modified_ms,
    }
}

struct Synth {
    shards: ShardSet,
    live: LiveSet,
    listing: Vec<ListEntry>,
}

impl Synth {
    /// Pack `n` holds ten blobs, `live[n]` of them are live.
    fn new(live: &[usize], modified_ms: Option<i64>) -> Synth {
        let mut s = Synth {
            shards: ShardSet::default(),
            live: LiveSet::default(),
            listing: Vec::new(),
        };
        for (n, live) in live.iter().enumerate() {
            let pack = n as u8 + 1;
            let ids: Vec<_> = (0..BLOBS_PER_PACK).map(|i| blob(pack, i)).collect();
            s.shards.shards.push((
                [pack; 32],
                pb::IndexShard {
                    packs: vec![record(pack, &ids)],
                    supersedes: Vec::new(),
                },
            ));
            for id in &ids[..*live] {
                s.live.insert(*id, BLOB_TYPE_DATA);
            }
            s.listing.push(listing(pack, modified_ms));
        }
        s
    }

    fn plan(&self, o: &PlanOptions) -> lodestore_format::gc::plan::GcPlan {
        let index = self.shards.index(&Default::default()).unwrap();
        plan(&self.shards, &index, &self.live, &self.listing, o).unwrap()
    }
}

fn packs(ns: &[u8]) -> BTreeSet<String> {
    ns.iter().map(|n| pack_object_key(&[*n; 32])).collect()
}

#[test]
fn gc_mark_young_packs_skipped() {
    let s = Synth::new(&[0, 0], Some(10_000));
    let mut o = PlanOptions {
        min_age_ms: 500,
        now_ms: 10_499,
        ..opts()
    };
    let p = s.plan(&o);
    assert!(p.delete.is_empty() && p.repack.is_empty());
    assert_eq!(p.young, 2);

    o.now_ms = 10_500;
    let p = s.plan(&o);
    assert_eq!(keys_of(&p.delete), packs(&[1, 2]));
    assert_eq!(p.young, 0);

    let unknown_age = Synth::new(&[0], None);
    o.now_ms = i64::MAX;
    let p = unknown_age.plan(&o);
    assert!(p.delete.is_empty());
    assert_eq!(p.young, 1);
}

#[test]
fn gc_mark_repack_threshold() {
    let s = Synth::new(&[3, 7, 0], Some(0));
    let o = PlanOptions {
        repack_below_pct: 50,
        ..opts()
    };
    let p = s.plan(&o);
    assert_eq!(keys_of(&p.repack), packs(&[1]));
    assert_eq!(keys_of(&p.delete), packs(&[3]));
    assert_eq!(p.repack[0].live_bytes, 3 * u64::from(BLOB_LEN));
    let freed = SYNTH_PACK_SIZE + SYNTH_PACK_SIZE - 3 * u64::from(BLOB_LEN);
    assert_eq!(p.bytes_freed(), freed);
    assert_eq!(
        p.repack_transfer_bytes(),
        SYNTH_PACK_SIZE + 3 * u64::from(BLOB_LEN)
    );
}

#[test]
fn gc_mark_fixed_pack_overhead_is_not_sparse() {
    let id = blob(1, 0);
    let mut s = Synth::new(&[], Some(0));
    s.shards.shards.push((
        [1; 32],
        pb::IndexShard {
            packs: vec![record(1, &[id])],
            supersedes: Vec::new(),
        },
    ));
    s.live.insert(id, BLOB_TYPE_DATA);
    s.listing.push(ListEntry {
        size: (PACK_PREFIX_LEN + BLOB_LEN as usize + 200) as u64,
        ..listing(1, Some(0))
    });
    let o = PlanOptions {
        repack_below_pct: 50,
        ..opts()
    };
    let p = s.plan(&o);
    assert!(p.repack.is_empty() && p.delete.is_empty());
}

#[test]
fn gc_mark_repack_limit_lowest_first() {
    let s = Synth::new(&[3, 1, 2], Some(0));
    let o = PlanOptions {
        repack_below_pct: 50,
        max_repack_bytes: 2 * SYNTH_PACK_SIZE,
        ..opts()
    };
    let p = s.plan(&o);
    let order: Vec<_> = p.repack.iter().map(|p| p.pack_id[0]).collect();
    assert_eq!(order, vec![2, 3]);
}

#[test]
fn gc_mark_duplicate_blob_counts_once() {
    let mut s = Synth::new(&[1], Some(0));
    let shared = blob(1, 0);
    let ids = vec![shared, blob(2, 1)];
    s.shards.shards.push((
        [2; 32],
        pb::IndexShard {
            packs: vec![record(2, &ids)],
            supersedes: Vec::new(),
        },
    ));
    s.listing.push(listing(2, Some(0)));

    let p = s.plan(&opts());
    assert_eq!(keys_of(&p.delete), packs(&[2]));
    assert!(p.repack.is_empty());
}

#[test]
fn gc_mark_candidates_capped() {
    let s = Synth::new(&[0, 0, 0, 0, 0], Some(0));
    let o = PlanOptions {
        max_candidates: 2,
        ..opts()
    };
    assert_eq!(s.plan(&o).delete.len(), 2);
}

#[test]
fn gc_mark_unindexed_pack_reported() {
    let mut s = Synth::new(&[10], Some(0));
    s.listing.push(listing(9, Some(0)));
    let p = s.plan(&opts());
    assert!(p.delete.is_empty() && p.repack.is_empty());
    assert_eq!(p.unindexed.len(), 1);
    assert_eq!(p.unindexed[0].0, pack_object_key(&[9; 32]));
    assert_eq!(p.packs_total, 2);
}

#[test]
fn gc_mark_live_blob_in_unlisted_pack_fails() {
    let mut s = Synth::new(&[1], Some(0));
    s.listing.clear();
    let index = s.shards.index(&Default::default()).unwrap();
    let err = plan(&s.shards, &index, &s.live, &s.listing, &opts())
        .err()
        .unwrap();
    assert_eq!(corrupt_object(err), pack_object_key(&[1; 32]));
}

#[test]
fn gc_mark_index_honours_supersedes_of_attested_shards_only() {
    let (old, new) = ([1u8; 32], [2u8; 32]);
    let shared = blob(7, 0);
    let mut set = ShardSet::default();
    set.shards.push((
        old,
        pb::IndexShard {
            packs: vec![record(1, &[shared])],
            supersedes: Vec::new(),
        },
    ));
    set.shards.push((
        new,
        pb::IndexShard {
            packs: vec![record(2, &[shared])],
            supersedes: vec![old.to_vec()],
        },
    ));
    let pack_of = |set: &ShardSet| {
        set.index(&Default::default())
            .unwrap()
            .get(&shared, BLOB_TYPE_DATA)
            .unwrap()
            .pack_id
    };
    assert_eq!(pack_of(&set), [1; 32]);
    set.attested.insert(new);
    assert_eq!(pack_of(&set), [2; 32]);
}

#[tokio::test]
async fn gc_mark_load_shards_reads_attestations() {
    let fx = Fx::new().await;
    fx.backup(1).await;
    let keys = fx.repo.read_keys();
    let shards = load_shards(&fx.store, keys).await.unwrap();
    assert!(shards.attested.is_empty());
    let id = shards.shards[0].0;

    put_attestation(&fx.store, keys, &[id], &[], Vec::new(), &mut Entropy::os())
        .await
        .unwrap();
    let shards = load_shards(&fx.store, keys).await.unwrap();
    assert_eq!(shards.attested.len(), 1);
    assert!(shards.attested.contains(&id));
}
