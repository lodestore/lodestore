//! Attestation lifecycle: upgrading a repository written before attestations
//! existed, resuming an interrupted upgrade, and consolidating the objects.

mod gc_support;

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use data_encoding::HEXLOWER;
use gc_support::Fx;
use lodestore_format::attest::{
    MARKER_LEGACY_SHARDS, MARKER_UPGRADE_PART, attest_key, consolidate_attestations,
    ensure_upgraded, open_attestation, put_attestation, read_attestations, seal_attestation,
};
use lodestore_format::crypto::{BlobId, Entropy};
use lodestore_format::fs::filter::PathMatcher;
use lodestore_format::index::{index_object_key, parse_hex32};
use lodestore_format::keys::{MasterKey, ReadKeys};
use lodestore_format::locks::new_lock_id;
use lodestore_format::pb;
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::repo::Repository;
use lodestore_format::store::{
    BlobDelete, BlobError, BlobPurge, BlobStore, ListEntry, LocalFsStore, PurgeOutcome,
};
use serde_json::Value;

type Hook = Box<dyn Fn() + Send + Sync>;
type DeleteHook = Box<dyn Fn(&str) + Send + Sync>;

/// A store that misbehaves on request.
struct Hooked<'a> {
    inner: &'a LocalFsStore,
    attest_puts: AtomicUsize,
    fail_attest_put: Option<usize>,
    hide_get: Option<String>,
    after_index_list: Mutex<Option<Hook>>,
    before_delete: Option<DeleteHook>,
}

impl<'a> Hooked<'a> {
    fn new(inner: &'a LocalFsStore) -> Self {
        Hooked {
            inner,
            attest_puts: AtomicUsize::new(0),
            fail_attest_put: None,
            hide_get: None,
            after_index_list: Mutex::new(None),
            before_delete: None,
        }
    }
}

impl BlobStore for Hooked<'_> {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<(), BlobError> {
        if key.starts_with("locks/attest-") {
            let n = self.attest_puts.fetch_add(1, Ordering::SeqCst) + 1;
            if self.fail_attest_put == Some(n) {
                return Err(BlobError::backend("injected failure"));
            }
        }
        self.inner.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        if self.hide_get.as_deref() == Some(key) {
            return Err(BlobError::NotFound);
        }
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.inner.get_range(key, range).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        let listed = self.inner.list(prefix).await?;
        if prefix == "index/"
            && let Some(hook) = self.after_index_list.lock().unwrap().take()
        {
            hook();
        }
        Ok(listed)
    }
}

impl BlobDelete for Hooked<'_> {
    async fn delete(&self, key: &str) -> Result<(), BlobError> {
        if let Some(hook) = &self.before_delete {
            hook(key);
        }
        self.inner.delete(key).await
    }
}

impl BlobPurge for Hooked<'_> {
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

fn keys() -> ReadKeys {
    ReadKeys::derive(MasterKey::from_bytes([1u8; 32]), [2u8; 16])
}

fn bare() -> (tempfile::TempDir, LocalFsStore) {
    let tmp = tempfile::tempdir().unwrap();
    let store = LocalFsStore::new(tmp.path().join("repo"));
    (tmp, store)
}

async fn plant(store: &LocalFsStore, id: u8) -> BlobId {
    let id = [id; 32];
    store
        .put_if_absent(&index_object_key(&id), Bytes::from_static(b"shard"))
        .await
        .unwrap();
    id
}

async fn locks(store: &LocalFsStore) -> BTreeSet<String> {
    store
        .list("locks/")
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.key)
        .collect()
}

async fn index_ids(store: &LocalFsStore) -> BTreeSet<BlobId> {
    store
        .list("index/")
        .await
        .unwrap()
        .into_iter()
        .map(|e| parse_hex32(e.key.strip_prefix("index/").unwrap()).unwrap())
        .collect()
}

async fn attestations(store: &LocalFsStore, keys: &ReadKeys) -> Vec<pb::Attestation> {
    let mut out = Vec::new();
    for key in locks(store).await {
        if let Ok(bytes) = store.get(&key).await
            && let Ok(a) = open_attestation(keys.k_srv(), keys.repo_id(), &key, &bytes)
        {
            out.push(a);
        }
    }
    out
}

async fn put(store: &LocalFsStore, keys: &ReadKeys, shards: &[BlobId], markers: &[u32]) -> String {
    put_attestation(store, keys, shards, markers, Vec::new(), &mut Entropy::os())
        .await
        .unwrap()
}

#[tokio::test]
async fn attest_upgrade_signs_existing_shards_once() {
    let fx = Fx::new().await;
    fx.backup("a", &[("one", 1, 50 << 10)]).await;
    fx.backup("b", &[("two", 2, 50 << 10)]).await;
    let keys = fx.repo.read_keys();
    let listed = index_ids(&fx.store).await;
    assert!(!listed.is_empty());

    let out = ensure_upgraded(&fx.store, keys, &mut Entropy::os())
        .await
        .unwrap();
    assert!(!out.already);
    assert_eq!(out.attested, listed.len());
    let set = read_attestations(&fx.store, keys).await.unwrap();
    assert_eq!(set.shards.iter().copied().collect::<BTreeSet<_>>(), listed);
    assert!(set.markers.contains(&MARKER_LEGACY_SHARDS));
    assert!(set.upgrade_parts >= 1);
    let done: Vec<_> = attestations(&fx.store, keys)
        .await
        .into_iter()
        .filter(|a| a.markers == [MARKER_LEGACY_SHARDS])
        .collect();
    assert_eq!(done.len(), 1);
    assert!(done[0].shard_ids.is_empty());

    let before = locks(&fx.store).await;
    let again = ensure_upgraded(&fx.store, keys, &mut Entropy::os())
        .await
        .unwrap();
    assert!(again.already);
    assert_eq!(locks(&fx.store).await, before);
}

#[tokio::test]
async fn attest_upgrade_resumes_after_interruption() {
    let fx = Fx::new().await;
    fx.backup("a", &[("one", 1, 50 << 10)]).await;
    let keys = fx.repo.read_keys();

    let mut hooked = Hooked::new(&fx.store);
    hooked.fail_attest_put = Some(2);
    assert!(
        ensure_upgraded(&hooked, keys, &mut Entropy::os())
            .await
            .is_err()
    );
    let set = read_attestations(&fx.store, keys).await.unwrap();
    assert!(!set.markers.contains(&MARKER_LEGACY_SHARDS));
    assert!(set.upgrade_parts >= 1);

    fx.backup("b", &[("two", 2, 50 << 10)]).await;
    let listed = index_ids(&fx.store).await;
    let missing = listed.iter().filter(|id| !set.shards.contains(*id)).count();
    assert!(missing >= 1);

    let out = ensure_upgraded(&fx.store, keys, &mut Entropy::os())
        .await
        .unwrap();
    assert!(!out.already);
    assert_eq!(out.attested, missing);
    let set = read_attestations(&fx.store, keys).await.unwrap();
    assert!(set.markers.contains(&MARKER_LEGACY_SHARDS));
    assert_eq!(set.shards.iter().copied().collect::<BTreeSet<_>>(), listed);
    assert!(
        ensure_upgraded(&fx.store, keys, &mut Entropy::os())
            .await
            .unwrap()
            .already
    );
}

#[tokio::test]
async fn attest_upgrade_ignores_shards_planted_after_marker() {
    let (_tmp, store) = bare();
    let keys = keys();
    plant(&store, 1).await;
    ensure_upgraded(&store, &keys, &mut Entropy::os())
        .await
        .unwrap();
    let before = locks(&store).await;

    let planted = plant(&store, 0xAA).await;
    let out = ensure_upgraded(&store, &keys, &mut Entropy::os())
        .await
        .unwrap();
    assert!(out.already);
    assert_eq!(locks(&store).await, before);
    assert!(
        !read_attestations(&store, &keys)
            .await
            .unwrap()
            .shards
            .contains(&planted)
    );
}

#[tokio::test]
async fn attest_upgrade_refuses_on_incomplete_read() {
    let (_tmp, store) = bare();
    let keys = keys();
    plant(&store, 1).await;
    let part = put(&store, &keys, &[[1u8; 32]], &[MARKER_UPGRADE_PART]).await;
    let before = locks(&store).await;

    let mut hooked = Hooked::new(&store);
    hooked.hide_get = Some(part);
    assert!(
        ensure_upgraded(&hooked, &keys, &mut Entropy::os())
            .await
            .is_err()
    );
    assert_eq!(locks(&store).await, before);
}

#[tokio::test]
async fn attest_upgrade_refuses_when_marker_is_missing() {
    let (_tmp, store) = bare();
    let keys = keys();
    plant(&store, 1).await;
    put(&store, &keys, &[[1u8; 32]], &[MARKER_UPGRADE_PART]).await;
    put(&store, &keys, &[[1u8; 32]], &[]).await;
    let planted = plant(&store, 0xAA).await;
    let before = locks(&store).await;

    assert!(
        ensure_upgraded(&store, &keys, &mut Entropy::os())
            .await
            .is_err()
    );
    assert_eq!(locks(&store).await, before);
    assert!(
        !read_attestations(&store, &keys)
            .await
            .unwrap()
            .shards
            .contains(&planted)
    );
}

#[tokio::test]
async fn attest_upgrade_ignores_junk_attest_objects() {
    let (_tmp, store) = bare();
    let keys = keys();
    plant(&store, 1).await;
    let junk = attest_key(&new_lock_id(&mut Entropy::os()));
    store
        .put_if_absent(
            &junk,
            Bytes::from_static(b"not an attestation at all, no mac"),
        )
        .await
        .unwrap();

    let out = ensure_upgraded(&store, &keys, &mut Entropy::os())
        .await
        .unwrap();
    assert!(!out.already);
    assert_eq!(out.attested, 1);
    let set = read_attestations(&store, &keys).await.unwrap();
    assert_eq!(set.rejected.len(), 1);
    assert_eq!(set.rejected[0].0, junk);
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1")
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn fixture_text(name: &str) -> String {
    fs::read_to_string(fixtures().join(name))
        .unwrap()
        .trim_end()
        .to_owned()
}

async fn fixture_copy() -> (tempfile::TempDir, LocalFsStore, Repository) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    copy_dir(&fixtures().join("repo"), &root);
    let store = LocalFsStore::new(root);
    let repo = Repository::open(&store, fixture_text("PASSWORD").as_bytes())
        .await
        .unwrap();
    (tmp, store, repo)
}

/// Restores every snapshot of the frozen fixture and checks the file hashes
/// `golden.rs` checks.
async fn assert_fixture_restores(store: &LocalFsStore, repo: &Repository) {
    let mut reader = Reader::open(store, repo.read_keys().duplicate(), repo.config()).unwrap();
    let include = (!cfg!(target_os = "linux")).then(|| {
        PathMatcher::new(["/lodestore-golden/src/*", "!/lodestore-golden/src/weird"]).unwrap()
    });
    let weird = HEXLOWER.encode(b"/lodestore-golden/src/weird/");
    let expected: Value = serde_json::from_str(&fixture_text("expected.json")).unwrap();
    for want in expected["snapshots"].as_array().unwrap() {
        let id = parse_hex32(want["id"].as_str().unwrap()).unwrap();
        let target = tempfile::tempdir().unwrap();
        let report = reader
            .restore(
                &id,
                target.path(),
                &RestoreOptions {
                    include: include.clone(),
                    ..Default::default()
                },
                &|_| {},
            )
            .await
            .unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let mut checked = 0;
        for node in want["nodes"].as_array().unwrap() {
            let Some(hash) = node["blake3"].as_str() else {
                continue;
            };
            let path_hex = node["path"].as_str().unwrap();
            if !cfg!(target_os = "linux") && path_hex.starts_with(&weird) {
                continue;
            }
            let path = HEXLOWER.decode(path_hex.as_bytes()).unwrap();
            let restored = target.path().join(OsStr::from_bytes(&path[1..]));
            let got = blake3::hash(&fs::read(&restored).unwrap());
            assert_eq!(got.to_hex().as_str(), hash, "{restored:?}");
            checked += 1;
        }
        assert!(checked >= 10, "only {checked} files were checked");
    }
}

#[tokio::test]
async fn attest_upgrade_v1_fixture_restores() {
    let (_tmp, store, repo) = fixture_copy().await;
    let listed = index_ids(&store).await;
    let out = ensure_upgraded(&store, repo.read_keys(), &mut Entropy::os())
        .await
        .unwrap();
    assert_eq!(out.attested, listed.len());
    assert_fixture_restores(&store, &repo).await;
}

#[tokio::test]
async fn attest_upgrade_not_needed_to_restore_v1_fixture() {
    let (_tmp, store, repo) = fixture_copy().await;
    assert!(locks(&store).await.iter().all(|k| !k.contains("attest-")));
    assert_fixture_restores(&store, &repo).await;
    assert!(locks(&store).await.iter().all(|k| !k.contains("attest-")));
}

struct Five {
    _tmp: tempfile::TempDir,
    store: LocalFsStore,
    keys: ReadKeys,
    provenance: Vec<Vec<u8>>,
}

/// Five verified objects: the completed upgrade marker, one with sealed
/// provenance, three with shards (id 9 is not in `index/`).
async fn five() -> Five {
    let (tmp, store) = bare();
    let keys = keys();
    for id in 1..=4 {
        plant(&store, id).await;
    }
    put(&store, &keys, &[], &[MARKER_LEGACY_SHARDS]).await;
    put(&store, &keys, &[[1u8; 32]], &[MARKER_UPGRADE_PART]).await;
    put(&store, &keys, &[[2u8; 32], [3u8; 32]], &[]).await;
    put(&store, &keys, &[[4u8; 32], [9u8; 32]], &[]).await;
    let provenance = vec![b"sealed one".to_vec(), b"sealed two".to_vec()];
    put_attestation(
        &store,
        &keys,
        &[[2u8; 32]],
        &[],
        provenance.clone(),
        &mut Entropy::os(),
    )
    .await
    .unwrap();
    Five {
        _tmp: tmp,
        store,
        keys,
        provenance,
    }
}

#[tokio::test]
async fn attest_consolidate_merges_and_deletes_sources() {
    let f = five().await;
    let before = locks(&f.store).await;
    assert_eq!(before.len(), 5);

    let out = consolidate_attestations(&f.store, &f.keys, false, &mut Entropy::os())
        .await
        .unwrap();
    assert_eq!((out.read, out.written.len(), out.deleted), (5, 1, 5));
    let after = locks(&f.store).await;
    assert_eq!(after, out.written.iter().cloned().collect());
    assert!(after.is_disjoint(&before));

    let set = read_attestations(&f.store, &f.keys).await.unwrap();
    assert_eq!(
        set.shards.iter().copied().collect::<BTreeSet<_>>(),
        index_ids(&f.store).await
    );
    assert_eq!(
        set.markers,
        BTreeSet::from([MARKER_LEGACY_SHARDS, MARKER_UPGRADE_PART])
    );
    assert_eq!(set.sealed_provenance, f.provenance);
}

#[tokio::test]
async fn attest_consolidate_dry_run_changes_nothing() {
    let f = five().await;
    let before = locks(&f.store).await;
    let out = consolidate_attestations(&f.store, &f.keys, true, &mut Entropy::os())
        .await
        .unwrap();
    assert_eq!((out.read, out.written.len(), out.deleted), (5, 0, 0));
    assert_eq!(locks(&f.store).await, before);
}

#[tokio::test]
async fn attest_consolidate_keeps_attestation_written_meanwhile() {
    let f = five().await;
    let root = f._tmp.path().join("repo");
    let keys = keys();
    let id = new_lock_id(&mut Entropy::os());
    let key = attest_key(&id);
    let bytes = seal_attestation(
        keys.k_srv(),
        keys.repo_id(),
        &pb::Attestation {
            attest_id: id.to_vec(),
            shard_ids: vec![vec![7u8; 32]],
            ..Default::default()
        },
    );
    let path = root.join(&key);
    let hooked = Hooked::new(&f.store);
    *hooked.after_index_list.lock().unwrap() = Some(Box::new(move || {
        fs::write(&path, &bytes).unwrap();
    }));

    let out = consolidate_attestations(&hooked, &f.keys, false, &mut Entropy::os())
        .await
        .unwrap();
    assert_eq!(out.deleted, 5);
    let after = locks(&f.store).await;
    assert!(after.contains(&key));
    assert_eq!(after.len(), 2);
    let set = read_attestations(&f.store, &f.keys).await.unwrap();
    assert!(set.shards.contains(&[7u8; 32]));
}

#[tokio::test]
async fn attest_consolidate_reader_never_sees_less() {
    let f = five().await;
    let want = index_ids(&f.store).await;
    let root = f._tmp.path().join("repo");
    let seen: std::sync::Arc<Mutex<Vec<BTreeSet<BlobId>>>> = Default::default();
    let mut hooked = Hooked::new(&f.store);
    let (sink, k) = (seen.clone(), keys());
    hooked.before_delete = Some(Box::new(move |_| {
        let mut union = BTreeSet::new();
        for entry in fs::read_dir(root.join("locks")).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            let key = format!("locks/{name}");
            let bytes = fs::read(root.join(&key)).unwrap();
            if let Ok(a) = open_attestation(k.k_srv(), k.repo_id(), &key, &bytes) {
                union.extend(
                    a.shard_ids
                        .iter()
                        .map(|s| <BlobId>::try_from(s.as_slice()).unwrap()),
                );
            }
        }
        sink.lock().unwrap().push(union);
    }));

    consolidate_attestations(&hooked, &f.keys, false, &mut Entropy::os())
        .await
        .unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 5);
    for union in seen.iter() {
        assert!(union.is_superset(&want));
    }
}

#[tokio::test]
async fn attest_consolidate_skips_rejected() {
    let f = five().await;
    let forged = [9u8; 32];
    let id = new_lock_id(&mut Entropy::os());
    let key = attest_key(&id);
    let bytes = seal_attestation(
        &forged,
        f.keys.repo_id(),
        &pb::Attestation {
            attest_id: id.to_vec(),
            shard_ids: vec![vec![4u8; 32]],
            markers: vec![MARKER_LEGACY_SHARDS],
            ..Default::default()
        },
    );
    f.store
        .put_if_absent(&key, Bytes::from(bytes))
        .await
        .unwrap();

    let out = consolidate_attestations(&f.store, &f.keys, false, &mut Entropy::os())
        .await
        .unwrap();
    assert_eq!(out.read, 5);
    let after = locks(&f.store).await;
    assert!(after.contains(&key));
    assert_eq!(after.len(), 2);
    let set = read_attestations(&f.store, &f.keys).await.unwrap();
    assert_eq!(set.objects.len(), 1);
    assert_eq!(set.rejected.len(), 1);
}

#[tokio::test]
async fn attest_consolidate_waits_for_upgrade_marker() {
    let (_tmp, store) = bare();
    let keys = keys();
    plant(&store, 1).await;
    put(&store, &keys, &[[1u8; 32]], &[MARKER_UPGRADE_PART]).await;
    put(&store, &keys, &[[1u8; 32]], &[MARKER_UPGRADE_PART]).await;
    let before = locks(&store).await;
    let out = consolidate_attestations(&store, &keys, false, &mut Entropy::os())
        .await
        .unwrap();
    assert_eq!((out.written.len(), out.deleted), (0, 0));
    assert_eq!(locks(&store).await, before);
}
