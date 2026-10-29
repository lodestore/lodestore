//! The repository checks against small repositories in temporary folders.

mod gc_support;

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use gc_support::*;
use lodestore_format::check::packs::{PacksReport, check_packs};
use lodestore_format::check::sample::{
    MAX_SAMPLE_NODES, SampleOptions, SampleReport, sample_restore,
};
use lodestore_format::check::structure::{StructureReport, check_structure};
use lodestore_format::check::{FindingKind, MAX_FINDINGS, NoteKind};
use lodestore_format::crypto::{BLOB_TYPE_DATA, BlobId, Entropy};
use lodestore_format::index::parse_hex32;
use lodestore_format::locks::{
    MAX_GC_MANIFEST_SIZE, gc_manifest_key, new_lock_id, publish_gc_manifest,
};
use lodestore_format::pack::{PACK_MAX_SIZE, PACK_PREFIX_LEN, pack_object_key, read_pack_header};
use lodestore_format::pb;
use lodestore_format::reader::Reader;
use lodestore_format::snapshot::{MAX_SNAPSHOT_SIZE, list_snapshots, snapshot_object_key};
use lodestore_format::store::{BlobError, BlobStore, ListEntry, LocalFsStore};
use lodestore_format::tree::encode_tree;

const A: Files<'static> = &[("one", 1, 200 << 10), ("dir/two", 2, 150 << 10)];
const B: Files<'static> = &[("one", 1, 200 << 10), ("three", 3, 100 << 10)];

fn repo_dir(fx: &Fx) -> std::path::PathBuf {
    fx.tmp.path().join("repo")
}

async fn structure(fx: &Fx) -> StructureReport {
    check_structure(
        &fx.store,
        fx.repo.read_keys(),
        fx.repo.config(),
        &|_, _, _| {},
    )
    .await
    .unwrap()
}

fn kinds(report: &StructureReport) -> Vec<FindingKind> {
    report.findings.iter().map(|f| f.kind).collect()
}

fn folder_bytes(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let path = e.unwrap().path();
            if path.is_dir() {
                folder_bytes(&path)
            } else {
                fs::metadata(path).unwrap().len()
            }
        })
        .sum()
}

#[tokio::test]
async fn check_structure_clean_repo_passes() {
    let fx = Fx::new().await;
    fx.backup("a", A).await;
    fx.backup("b", B).await;
    let report = structure(&fx).await;
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert_eq!(report.findings_total, 0);
    assert_eq!(report.snapshots_checked, 2);
    assert_eq!(report.snapshots.len(), 2);
    assert!(report.trees_checked >= 2);
    assert_eq!(report.packs_listed, fx.packs().await.len() as u64);
    assert_eq!(report.missing_packs, 0);
    assert_eq!(report.physical_bytes, folder_bytes(&repo_dir(&fx)));
    assert!(report.objects > 0);
}

#[tokio::test]
async fn check_structure_missing_pack_found() {
    let fx = Fx::new().await;
    fx.backup("a", A).await;
    fx.backup("b", B).await;
    let pack = fx.packs().await.into_keys().next().unwrap();
    fs::remove_file(repo_dir(&fx).join(&pack)).unwrap();
    let report = structure(&fx).await;
    assert_eq!(kinds(&report), [FindingKind::MissingPack]);
    assert_eq!(report.findings[0].key, pack);
    assert_eq!(report.missing_packs, 1);
}

#[tokio::test]
async fn check_structure_truncated_pack_found() {
    let fx = Fx::new().await;
    fx.backup("a", A).await;
    let (pack, size) = fx.packs().await.into_iter().next().unwrap();
    let path = repo_dir(&fx).join(&pack);
    let file = fs::File::options().write(true).open(&path).unwrap();
    file.set_len(size - 1).unwrap();
    let report = structure(&fx).await;
    assert_eq!(kinds(&report), [FindingKind::PackMismatch]);
    assert_eq!(report.findings[0].key, pack);
}

#[tokio::test]
async fn check_structure_unindexed_pack_is_note() {
    let fx = Fx::new().await;
    let out = fx.backup("a", A).await;
    let packs = fx.packs().await.len();
    for key in fx.listing("index/").await.into_keys() {
        fs::remove_file(repo_dir(&fx).join(key)).unwrap();
    }
    let report = structure(&fx).await;
    let snapshot_key = lodestore_format::snapshot::snapshot_object_key(&out.snapshot_id);
    assert!(!report.findings.is_empty());
    for f in &report.findings {
        assert_eq!(f.kind, FindingKind::UnknownBlob);
        assert_eq!(f.key, snapshot_key);
    }
    let unindexed = report
        .notes
        .iter()
        .filter(|n| n.kind == NoteKind::UnindexedPack)
        .count();
    assert_eq!(unindexed, packs);
}

#[tokio::test]
async fn check_structure_shared_bad_blob_reported_for_every_snapshot() {
    let fx = Fx::new().await;
    let a = fx.backup("a", A).await;
    let b = fx.backup("b", B).await;
    let keys = fx.repo.read_keys();
    let mut records = Vec::new();
    for (_, shard) in lodestore_format::gc::mark::load_shards(&fx.store, keys)
        .await
        .unwrap()
        .shards
    {
        for mut record in shard.packs {
            record.entries = record
                .entries
                .chunks(45)
                .filter(|e| e[32] != BLOB_TYPE_DATA)
                .flatten()
                .copied()
                .collect();
            records.push(record);
        }
    }
    for key in fx.listing("index/").await.into_keys() {
        fs::remove_file(repo_dir(&fx).join(key)).unwrap();
    }
    let (id, bytes) = lodestore_format::index::seal_shard(
        keys.k_idx(),
        keys.repo_id(),
        records,
        &mut Entropy::os(),
    )
    .unwrap();
    fx.store
        .put_if_absent(
            &lodestore_format::index::index_object_key(&id),
            bytes.into(),
        )
        .await
        .unwrap();
    let report = structure(&fx).await;
    let key_a = lodestore_format::snapshot::snapshot_object_key(&a.snapshot_id);
    let key_b = lodestore_format::snapshot::snapshot_object_key(&b.snapshot_id);
    let details = |key: &str| -> std::collections::BTreeSet<&str> {
        report
            .findings
            .iter()
            .filter(|f| f.key == key)
            .map(|f| f.detail.as_str())
            .collect()
    };
    let (da, db) = (details(&key_a), details(&key_b));
    assert!(!da.is_empty() && !db.is_empty());
    assert!(da.intersection(&db).count() >= 1, "{da:?} {db:?}");
}

#[tokio::test]
async fn check_structure_gc_candidate_is_pending() {
    let fx = Fx::new().await;
    fx.backup("a", A).await;
    let pack = fx.packs().await.into_keys().next().unwrap();
    let id = lodestore_format::index::parse_hex32(pack.rsplit('/').next().unwrap()).unwrap();
    let keys = fx.repo.read_keys();
    let mut entropy = Entropy::os();
    publish_gc_manifest(
        &fx.store,
        keys.k_idx(),
        keys.repo_id(),
        &new_lock_id(&mut entropy),
        &[id],
        &mut entropy,
    )
    .await
    .unwrap();
    fs::remove_file(repo_dir(&fx).join(&pack)).unwrap();
    let report = structure(&fx).await;
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert_eq!(report.missing_packs, 0);
    let pending: Vec<_> = report
        .notes
        .iter()
        .filter(|n| n.kind == NoteKind::PendingCleanup)
        .collect();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].key, pack);
}

#[tokio::test]
async fn check_structure_stale_gc_manifest_does_not_excuse_missing_pack() {
    let fx = Fx::new().await;
    fx.backup("a", A).await;
    let pack = fx.packs().await.into_keys().next().unwrap();
    let id = lodestore_format::index::parse_hex32(pack.rsplit('/').next().unwrap()).unwrap();
    let keys = fx.repo.read_keys();
    let mut entropy = Entropy::os();
    let lock = new_lock_id(&mut entropy);
    publish_gc_manifest(
        &fx.store,
        keys.k_idx(),
        keys.repo_id(),
        &lock,
        &[id],
        &mut entropy,
    )
    .await
    .unwrap();
    let manifest = repo_dir(&fx).join(lodestore_format::locks::gc_manifest_key(&lock));
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3 * 24 * 3600);
    fs::File::options()
        .write(true)
        .open(&manifest)
        .unwrap()
        .set_modified(old)
        .unwrap();
    fs::remove_file(repo_dir(&fx).join(&pack)).unwrap();
    let report = structure(&fx).await;
    assert_eq!(kinds(&report), [FindingKind::MissingPack]);
    assert_eq!(report.findings[0].key, pack);
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.kind == NoteKind::StaleCleanup)
    );
}

fn copy_missing(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            fs::create_dir_all(&target).unwrap();
            copy_missing(&entry.path(), &target);
        } else if !target.exists() {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Publishes a prepared backup into the repository right after the first
/// listing of `index/`, the way a backup that finishes mid-check would.
struct Interleaved<'a> {
    inner: &'a LocalFsStore,
    from: std::path::PathBuf,
    to: std::path::PathBuf,
    fired: AtomicBool,
}

impl BlobStore for Interleaved<'_> {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<(), BlobError> {
        self.inner.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &str, range: std::ops::Range<u64>) -> Result<Bytes, BlobError> {
        self.inner.get_range(key, range).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        let listed = self.inner.list(prefix).await;
        if prefix == "index/" && !self.fired.swap(true, Ordering::SeqCst) {
            for dir in ["packs", "index", "snapshots"] {
                fs::create_dir_all(self.to.join(dir)).unwrap();
                copy_missing(&self.from.join(dir), &self.to.join(dir));
            }
        }
        listed
    }
}

#[tokio::test]
async fn check_structure_backup_during_check() {
    let fx = Fx::new().await;
    fx.backup("a", A).await;
    let later = fx.fork().await;
    later.backup("b", B).await;
    let store = Interleaved {
        inner: &fx.store,
        from: repo_dir(&later),
        to: repo_dir(&fx),
        fired: AtomicBool::new(false),
    };
    let report = check_structure(&store, fx.repo.read_keys(), fx.repo.config(), &|_, _, _| {})
        .await
        .unwrap();
    assert!(store.fired.load(Ordering::SeqCst));
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert_eq!(report.snapshots_checked, 1);
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.kind == NoteKind::UnindexedPack),
        "{:?}",
        report.notes
    );
}

async fn pack_list(fx: &Fx) -> Vec<(BlobId, u64)> {
    fx.packs()
        .await
        .into_iter()
        .map(|(key, size)| (parse_hex32(key.rsplit('/').next().unwrap()).unwrap(), size))
        .collect()
}

async fn packs(fx: &Fx, list: &[(BlobId, u64)], max_bytes: u64) -> PacksReport {
    check_packs(
        &fx.store,
        fx.repo.read_keys(),
        list,
        max_bytes,
        &|_, _, _| {},
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn check_packs_clean_repo_passes() {
    let fx = Fx::new().await;
    fx.backup("a", A).await;
    fx.backup("b", B).await;
    let list = pack_list(&fx).await;
    let mut entries = 0u64;
    for (key, size) in fx.packs().await {
        let header = read_pack_header(&fx.store, &key, size, fx.repo.read_keys())
            .await
            .unwrap();
        entries += header.entries.len() as u64;
    }
    let report = packs(&fx, &list, 1 << 30).await;
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert_eq!(report.packs_read, list.len() as u64);
    assert_eq!(report.bytes_read, list.iter().map(|p| p.1).sum::<u64>());
    assert_eq!(report.blobs_verified, entries);
    assert_eq!(report.damaged_blobs, 0);
    assert_eq!(report.outcomes.len(), list.len());
}

#[tokio::test]
async fn check_packs_corrupted_pack_fails() {
    let fx = Fx::new().await;
    fx.backup("a", A).await;
    fx.backup("b", B).await;
    let list = pack_list(&fx).await;
    let pack = fx.packs().await.into_keys().next().unwrap();
    let path = repo_dir(&fx).join(&pack);
    let mut bytes = fs::read(&path).unwrap();
    bytes[PACK_PREFIX_LEN + 10] ^= 0x01;
    fs::write(&path, bytes).unwrap();

    let report = packs(&fx, &list, 1 << 30).await;
    assert!(report.findings_total >= 2);
    let kinds: Vec<_> = report.findings.iter().map(|f| f.kind).collect();
    assert!(kinds.contains(&FindingKind::PackMismatch), "{kinds:?}");
    assert!(kinds.contains(&FindingKind::DamagedBlob), "{kinds:?}");
    assert!(report.findings.iter().all(|f| f.key == pack));
    assert!(report.damaged_blobs >= 1);
    let damaged_packs = report.outcomes.iter().filter(|o| o.damaged > 0).count();
    assert_eq!(damaged_packs, 1);
}

#[tokio::test]
async fn check_packs_respects_byte_limit() {
    let fx = Fx::new().await;
    fx.backup("a", A).await;
    fx.backup("b", B).await;
    let list = pack_list(&fx).await;
    assert!(list.len() >= 2);
    let first = packs(&fx, &list, list[0].1).await;
    assert_eq!(first.packs_read, 1);
    assert_eq!(first.outcomes[0].pack_id, list[0].0);
    let two = packs(&fx, &list, list[0].1 + list[1].1).await;
    assert_eq!(two.packs_read, 2);
    let tiny = packs(&fx, &list, 1).await;
    assert_eq!(tiny.packs_read, 1);
    assert_eq!(tiny.outcomes.len(), 1);
}

#[tokio::test]
async fn check_packs_index_disagreeing_with_pack_found() {
    let fx = Fx::new().await;
    fx.backup("a", A).await;
    let list = pack_list(&fx).await;
    let keys = fx.repo.read_keys();
    let shards = lodestore_format::gc::mark::load_shards(&fx.store, keys)
        .await
        .unwrap()
        .shards;
    let mut records = Vec::new();
    for (_, shard) in &shards {
        records.extend(shard.packs.iter().cloned());
    }
    let ulen_at = 41;
    records[0].entries[ulen_at] ^= 1;
    for key in fx.listing("index/").await.into_keys() {
        fs::remove_file(repo_dir(&fx).join(key)).unwrap();
    }
    let (id, bytes) = lodestore_format::index::seal_shard(
        keys.k_idx(),
        keys.repo_id(),
        records,
        &mut Entropy::os(),
    )
    .unwrap();
    fx.store
        .put_if_absent(
            &lodestore_format::index::index_object_key(&id),
            bytes.into(),
        )
        .await
        .unwrap();
    let report = packs(&fx, &list, 1 << 30).await;
    let kinds: Vec<_> = report.findings.iter().map(|f| f.kind).collect();
    assert_eq!(kinds, [FindingKind::PackMismatch], "{:?}", report.findings);
    assert!(report.findings[0].detail.contains("index"));
}

#[tokio::test]
async fn check_packs_missing_pack_reported() {
    let fx = Fx::new().await;
    fx.backup("a", A).await;
    fx.backup("b", B).await;
    let list = pack_list(&fx).await;
    assert!(list.len() >= 2);
    let gone = lodestore_format::pack::pack_object_key(&list[0].0);
    fs::remove_file(repo_dir(&fx).join(&gone)).unwrap();
    let report = packs(&fx, &list, 1 << 30).await;
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].kind, FindingKind::MissingPack);
    assert_eq!(report.findings[0].key, gone);
    assert!(report.outcomes[0].missing);
    assert_eq!(report.packs_read, list.len() as u64 - 1);
    assert_eq!(report.outcomes.len(), list.len());
}

struct Refusing {
    inner: LocalFsStore,
    key: String,
}

impl BlobStore for Refusing {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<(), BlobError> {
        self.inner.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        if key == self.key {
            return Err(BlobError::Denied {
                message: "no".into(),
            });
        }
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &str, range: std::ops::Range<u64>) -> Result<Bytes, BlobError> {
        self.inner.get_range(key, range).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        self.inner.list(prefix).await
    }
}

#[tokio::test]
async fn check_packs_unreadable_pack_is_left_out() {
    let fx = Fx::new().await;
    fx.backup("a", A).await;
    fx.backup("b", B).await;
    let list = pack_list(&fx).await;
    assert!(list.len() >= 2);
    let refused = lodestore_format::pack::pack_object_key(&list[0].0);
    let store = Refusing {
        inner: LocalFsStore::new(repo_dir(&fx)),
        key: refused.clone(),
    };
    let report = check_packs(&store, fx.repo.read_keys(), &list, 1 << 30, &|_, _, _| {})
        .await
        .unwrap();
    assert_eq!(report.read_errors.len(), 1);
    assert_eq!(report.read_errors[0].0, refused);
    assert_eq!(report.outcomes.len(), list.len() - 1);
    assert!(report.outcomes.iter().all(|o| o.pack_id != list[0].0));
    assert!(report.findings.is_empty());
}

const TEN: [Files<'static>; 3] = [
    &[
        ("f0", 10, 20 << 10),
        ("f1", 11, 20 << 10),
        ("f2", 12, 20 << 10),
        ("f3", 13, 20 << 10),
        ("f4", 14, 20 << 10),
        ("f5", 15, 20 << 10),
        ("f6", 16, 20 << 10),
        ("f7", 17, 20 << 10),
        ("f8", 18, 20 << 10),
        ("f9", 19, 20 << 10),
    ],
    &[
        ("g0", 20, 20 << 10),
        ("g1", 21, 20 << 10),
        ("g2", 22, 20 << 10),
        ("g3", 23, 20 << 10),
        ("g4", 24, 20 << 10),
        ("g5", 25, 20 << 10),
        ("g6", 26, 20 << 10),
        ("g7", 27, 20 << 10),
        ("g8", 28, 20 << 10),
        ("g9", 29, 20 << 10),
    ],
    &[
        ("h0", 30, 20 << 10),
        ("h1", 31, 20 << 10),
        ("h2", 32, 20 << 10),
        ("h3", 33, 20 << 10),
        ("h4", 34, 20 << 10),
        ("h5", 35, 20 << 10),
        ("h6", 36, 20 << 10),
        ("h7", 37, 20 << 10),
        ("h8", 38, 20 << 10),
        ("h9", 39, 20 << 10),
    ],
];

struct Sample {
    scratch: tempfile::TempDir,
    report: SampleReport,
}

impl Sample {
    fn scratch_is_empty(&self) -> bool {
        fs::read_dir(self.scratch.path()).unwrap().next().is_none()
    }
}

async fn sample(
    fx: &Fx,
    snapshots: &[BlobId],
    files: u32,
    max_bytes: u64,
    should_stop: &dyn Fn() -> bool,
) -> Sample {
    sample_with(
        fx,
        snapshots,
        files,
        max_bytes,
        MAX_SAMPLE_NODES,
        &|_, _| {},
        should_stop,
    )
    .await
}

async fn sample_with(
    fx: &Fx,
    snapshots: &[BlobId],
    files: u32,
    max_bytes: u64,
    max_nodes: u64,
    progress: &dyn Fn(u64, u64),
    should_stop: &dyn Fn() -> bool,
) -> Sample {
    let scratch = tempfile::tempdir_in(fx.tmp.path()).unwrap();
    let mut reader =
        Reader::open(&fx.store, fx.repo.read_keys().duplicate(), fx.repo.config()).unwrap();
    let opts = SampleOptions {
        files,
        max_bytes,
        scratch: scratch.path().to_path_buf(),
        max_nodes,
    };
    let report = sample_restore(
        &mut reader,
        snapshots,
        &opts,
        &mut Entropy::os(),
        progress,
        should_stop,
    )
    .await
    .unwrap();
    Sample { scratch, report }
}

#[tokio::test]
async fn check_sample_restores_and_matches() {
    let fx = Fx::new().await;
    let mut ids = Vec::new();
    for (i, files) in TEN.iter().enumerate() {
        ids.push(fx.backup(&format!("s{i}"), files).await.snapshot_id);
    }
    let run = sample(&fx, &ids, 5, 1 << 30, &|| false).await;
    let report = &run.report;
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert_eq!(report.files_restored, 5);
    assert_eq!(report.bytes_restored, 5 * (20 << 10));
    assert_eq!(report.hash_mismatches, 0);
    assert_eq!(report.skipped, 0);
    assert!(!report.stopped_at_limit);
    assert!(report.snapshots_sampled >= 1);
    assert!(run.scratch_is_empty());
}

#[tokio::test]
async fn check_sample_reports_damaged_file() {
    let fx = Fx::new().await;
    let id = fx.backup("a", &[("one", 1, 200 << 10)]).await.snapshot_id;
    let mut data_pack = None;
    for (key, size) in fx.packs().await {
        let header = read_pack_header(&fx.store, &key, size, fx.repo.read_keys())
            .await
            .unwrap();
        if header.entries.iter().any(|e| e.blob_type == BLOB_TYPE_DATA) {
            assert!(header.entries.iter().all(|e| e.blob_type == BLOB_TYPE_DATA));
            data_pack = Some(key);
        }
    }
    let path = repo_dir(&fx).join(data_pack.expect("a pack with data"));
    let mut bytes = fs::read(&path).unwrap();
    bytes[PACK_PREFIX_LEN + 10] ^= 0x01;
    fs::write(&path, bytes).unwrap();

    let run = sample(&fx, &[id], 1, 1 << 30, &|| false).await;
    let report = &run.report;
    assert_eq!(report.hash_mismatches, 1);
    assert_eq!(report.files_restored, 0);
    assert_eq!(report.findings.len(), 1);
    let finding = &report.findings[0];
    assert!(matches!(
        finding.kind,
        FindingKind::RestoreFailed | FindingKind::RestoreMismatch
    ));
    assert!(finding.key.ends_with("/one"), "{}", finding.key);
    assert!(run.scratch_is_empty());
}

#[tokio::test]
async fn check_sample_stops_at_size_limit() {
    let fx = Fx::new().await;
    let id = fx.backup("a", A).await.snapshot_id;
    let none = sample(&fx, &[id], 5, 1000, &|| false).await;
    assert_eq!(none.report.files_restored, 0);
    assert!(none.report.stopped_at_limit);
    assert!(none.scratch_is_empty());

    let one = sample(&fx, &[id], 5, 160 << 10, &|| false).await;
    assert_eq!(one.report.files_restored, 1);
    assert!(one.report.bytes_restored <= 160 << 10);
    assert!(one.scratch_is_empty());
}

#[tokio::test]
async fn check_sample_should_stop() {
    let fx = Fx::new().await;
    let mut ids = Vec::new();
    for (i, files) in TEN.iter().enumerate() {
        ids.push(fx.backup(&format!("s{i}"), files).await.snapshot_id);
    }
    let stop = std::cell::Cell::new(false);
    let run = sample_with(
        &fx,
        &ids,
        5,
        1 << 30,
        MAX_SAMPLE_NODES,
        &|_, _| stop.set(true),
        &|| stop.get(),
    )
    .await;
    assert_eq!(run.report.files_restored, 1);
    assert!(run.report.findings.is_empty());
    assert!(run.scratch_is_empty());
}

#[tokio::test]
async fn check_sample_empty_repo() {
    let fx = Fx::new().await;
    let run = sample(&fx, &[], 5, 1 << 30, &|| false).await;
    let report = &run.report;
    assert_eq!(report.files_restored, 0);
    assert_eq!(report.snapshots_sampled, 0);
    assert!(report.findings.is_empty());
    assert!(!report.stopped_at_limit);
    assert!(run.scratch_is_empty());
}

/// A file of `size` bytes that takes no space on disk.
fn sparse(fx: &Fx, key: &str, size: u64) {
    let path = repo_dir(fx).join(key);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::File::create(&path).unwrap().set_len(size).unwrap();
}

#[tokio::test]
async fn check_packs_oversized_pack_not_downloaded() {
    let fx = Fx::new().await;
    let id = [7u8; 32];
    let size = PACK_MAX_SIZE as u64 + 1;
    sparse(&fx, &pack_object_key(&id), size);
    let report = packs(&fx, &[(id, size)], u64::MAX).await;
    let found: Vec<_> = report.findings.iter().map(|f| f.kind).collect();
    assert_eq!(
        found,
        [FindingKind::UnreadablePack],
        "{:?}",
        report.findings
    );
    assert!(
        report.findings[0].detail.contains(&size.to_string()),
        "{}",
        report.findings[0].detail
    );
    assert_eq!(report.outcomes[0].bytes, 0);
    assert_eq!(report.bytes_read, 0);
}

#[tokio::test]
async fn check_structure_oversized_snapshot_not_read() {
    let fx = Fx::new().await;
    fx.backup("a", A).await;
    let id = [9u8; 32];
    let key = snapshot_object_key(&id);
    let size = MAX_SNAPSHOT_SIZE + 1;
    sparse(&fx, &key, size);

    let report = structure(&fx).await;
    let finding = report
        .findings
        .iter()
        .find(|f| f.key == key)
        .expect("a finding about the snapshot");
    assert_eq!(finding.kind, FindingKind::UnreadableSnapshot);
    assert!(
        finding.detail.contains(&size.to_string()),
        "{}",
        finding.detail
    );
    assert!(!report.snapshots.contains(&id));

    let listed = list_snapshots(&fx.store, fx.repo.read_keys())
        .await
        .unwrap();
    let (_, why) = listed
        .skipped
        .iter()
        .find(|(k, _)| *k == key)
        .expect("skipped");
    assert!(why.contains(&size.to_string()), "{why}");

    let keys = fx.repo.read_keys();
    let index = lodestore_format::gc::mark::load_shards(&fx.store, keys)
        .await
        .unwrap()
        .index(&Default::default())
        .unwrap();
    let marked =
        lodestore_format::gc::mark::mark(&fx.store, keys, fx.repo.config(), &index, &|_, _| {})
            .await;
    let Err(err) = marked else {
        panic!("the cleanup must not treat the snapshot as gone");
    };
    assert!(err.to_string().contains(&size.to_string()), "{err}");
}

#[tokio::test]
async fn check_structure_oversized_gc_manifest_not_read() {
    let fx = Fx::new().await;
    fx.backup("a", A).await;
    let size = MAX_GC_MANIFEST_SIZE + 1;
    sparse(
        &fx,
        &gc_manifest_key(&new_lock_id(&mut Entropy::os())),
        size,
    );
    let report = structure(&fx).await;
    let note = report
        .notes
        .iter()
        .find(|n| n.kind == NoteKind::LocksUnreadable)
        .expect("a note about the locks");
    assert!(note.detail.contains(&size.to_string()), "{}", note.detail);
}

#[tokio::test]
async fn check_structure_notes_are_capped() {
    let fx = Fx::new().await;
    let dir = repo_dir(&fx).join("snapshots");
    fs::create_dir_all(&dir).unwrap();
    for n in 0..MAX_FINDINGS + 3 {
        fs::write(dir.join(format!("junk-{n}")), b"").unwrap();
    }
    let report = structure(&fx).await;
    assert_eq!(report.notes.len(), MAX_FINDINGS);
    assert_eq!(report.notes_total, MAX_FINDINGS as u64 + 3);
}

/// A snapshot whose every directory holds two directories `a` and `b`
/// sharing one subtree, `depth` levels down: 2^depth empty files.
async fn shared_subtrees(fx: &Fx, depth: usize) -> BlobId {
    let mut w = fx.open_writer().await;
    let leaf = pb::Node {
        name: b"f".to_vec(),
        r#type: pb::NodeType::File as i32,
        mode: 0o644,
        ..Default::default()
    };
    let mut tree = w.put_tree_raw(&encode_tree(vec![leaf])).unwrap();
    for _ in 0..depth {
        let dir = |name: &[u8]| pb::Node {
            name: name.to_vec(),
            r#type: pb::NodeType::Dir as i32,
            mode: 0o755,
            subtrees: vec![tree.to_vec()],
            ..Default::default()
        };
        tree = w
            .put_tree_raw(&encode_tree(vec![dir(b"a"), dir(b"b")]))
            .unwrap();
    }
    w.commit_snapshot_raw(tree, vec![b"/".to_vec()])
        .await
        .unwrap()
}

#[tokio::test]
async fn check_sample_walk_stops_when_asked() {
    let fx = Fx::new().await;
    let id = shared_subtrees(&fx, 10).await;
    let asked = std::cell::Cell::new(0u32);
    let run = sample(&fx, &[id], 5, 1 << 30, &|| {
        asked.set(asked.get() + 1);
        asked.get() >= 50
    })
    .await;
    assert_eq!(run.report.files_restored, 0);
    assert!(run.scratch_is_empty());
}

#[tokio::test]
async fn check_sample_walk_stops_at_node_budget() {
    let fx = Fx::new().await;
    let id = shared_subtrees(&fx, 20).await;
    let run = sample_with(&fx, &[id], 5, 1 << 30, 1000, &|_, _| {}, &|| false).await;
    assert!(run.report.stopped_at_limit);
    assert_eq!(run.report.files_restored, 0);
}

#[tokio::test]
async fn check_sample_oversized_file_counts_as_skipped() {
    let fx = Fx::new().await;
    let id = fx
        .backup("a", &[("big", 2, 200 << 10), ("empty", 1, 0)])
        .await
        .snapshot_id;
    let run = sample(&fx, &[id], 5, 100 << 10, &|| false).await;
    assert!(run.report.skipped >= 1, "{:?}", run.report);
    assert!(run.report.stopped_at_limit);
}
