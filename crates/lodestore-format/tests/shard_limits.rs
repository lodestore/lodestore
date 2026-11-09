//! Index shards are held whole in memory, so the size of one is checked
//! before it is downloaded.

mod adv_support;
mod gc_support;
#[path = "adv_support/planted.rs"]
mod planted;

use std::collections::HashSet;
use std::fs;
use std::ops::Range;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use adv_support::{attacker_keys, open_all_shards, plant_shard};
use bytes::Bytes;
use gc_support::Fx;
use lodestore_format::Error;
use lodestore_format::attest::{ATTEST_PREFIX, MARKER_LEGACY_SHARDS, put_attestation};
use lodestore_format::crypto::{BlobId, Entropy, HPKE_ENC_LEN, SEALED_KEY_LEN};
use lodestore_format::gc::mark::load_shards;
use lodestore_format::gc::sweep::confirm_unreferenced;
use lodestore_format::index::{
    Index, MAX_ATTESTED_SHARD_SIZE, MAX_SHARD_SIZE, fetch_attested_shard_bytes, fetch_shard_bytes,
    index_object_key, rebuild_index, seal_shard,
};
use lodestore_format::pack::{ENTRY_LEN, PACK_MAX_SIZE, PACK_PREFIX_LEN};
use lodestore_format::pb;
use lodestore_format::reader::Reader;
use lodestore_format::store::{BlobError, BlobStore, ListEntry, LocalFsStore};
use lodestore_format::verify_snapshot::verify_shard;
use planted::{PlantedListing, RangedOnly, planted_shard_entry};

/// Records the ranges asked of the store.
struct RangeLog {
    inner: LocalFsStore,
    ranges: Mutex<Vec<Range<u64>>>,
}

impl BlobStore for RangeLog {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<(), BlobError> {
        self.inner.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.ranges.lock().unwrap().push(range.clone());
        self.inner.get_range(key, range).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        self.inner.list(prefix).await
    }
}

/// A well-formed shard of honest records whose entry tables make the sealed
/// object larger than `MAX_SHARD_SIZE`.
fn big_shard(fx: &Fx) -> (BlobId, Vec<u8>) {
    big_shard_with(fx, Vec::new())
}

/// Like `big_shard`, with `real` records next to the filler one.
fn big_shard_with(fx: &Fx, real: Vec<pb::PackRecord>) -> (BlobId, Vec<u8>) {
    let mut entropy = Entropy::os();
    let count = MAX_SHARD_SIZE as usize / ENTRY_LEN + 1000;
    let mut entries = vec![0u8; count * ENTRY_LEN];
    entropy.fill(&mut entries);
    for e in entries.as_chunks_mut::<ENTRY_LEN>().0 {
        e[32] = 1;
        e[33..37].copy_from_slice(&(PACK_PREFIX_LEN as u32).to_le_bytes());
        e[37..41].copy_from_slice(&64u32.to_le_bytes());
        e[41..45].copy_from_slice(&16u32.to_le_bytes());
    }
    let records: Vec<pb::PackRecord> = entries
        .chunks(ENTRY_LEN * 20_000)
        .map(|chunk| pb::PackRecord {
            pack_id: entropy.bytes::<32>().to_vec(),
            hpke_enc: vec![0; HPKE_ENC_LEN],
            sealed_pack_key: vec![0; SEALED_KEY_LEN],
            pack_size: PACK_MAX_SIZE as u64,
            entries: chunk.to_vec(),
        })
        .collect();
    let keys = fx.repo.read_keys();
    let records: Vec<pb::PackRecord> = real.into_iter().chain(records).collect();
    let (id, bytes) = seal_shard(keys.k_idx(), keys.repo_id(), records, &mut entropy).unwrap();
    assert!(bytes.len() as u64 > MAX_SHARD_SIZE);
    (id, bytes)
}

async fn put_big_shard(fx: &Fx) -> (BlobId, String) {
    let (id, bytes) = big_shard(fx);
    let key = index_object_key(&id);
    fx.store
        .put_if_absent(&key, Bytes::from(bytes))
        .await
        .unwrap();
    (id, key)
}

async fn attest(fx: &Fx, shards: &[BlobId]) {
    put_attestation(
        &fx.store,
        fx.repo.read_keys(),
        shards,
        &[MARKER_LEGACY_SHARDS],
        Vec::new(),
        &mut Entropy::os(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn shard_limit_listed_size_is_checked_before_any_read() {
    let fx = Fx::new().await;
    let at = planted_shard_entry(MAX_SHARD_SIZE);
    let over = planted_shard_entry(MAX_SHARD_SIZE + 1);
    let (at_key, over_key) = (at.key.clone(), over.key.clone());
    let store = PlantedListing::new(&fx.store, vec![at, over]);

    let err = fetch_shard_bytes(&store, &over_key, Some(MAX_SHARD_SIZE + 1), MAX_SHARD_SIZE)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Corrupt { .. }));
    assert!(err.to_string().contains("more than a shard can be"));
    assert_eq!(store.reads_of(&over_key), 0);

    let _ = fetch_shard_bytes(&store, &at_key, Some(MAX_SHARD_SIZE), MAX_SHARD_SIZE).await;
    assert_eq!(store.reads_of(&at_key), 1);
}

#[tokio::test]
async fn shard_limit_without_listing_asks_for_one_byte_more() {
    let fx = Fx::new().await;
    for (key, len) in [("probe/at", 100usize), ("probe/over", 101)] {
        fx.store
            .put_if_absent(key, Bytes::from(vec![7u8; len]))
            .await
            .unwrap();
    }
    let fx_dir = fx.tmp.path().join("repo");
    let log = RangeLog {
        inner: LocalFsStore::new(fx_dir),
        ranges: Mutex::new(Vec::new()),
    };

    let got = fetch_shard_bytes(&log, "probe/at", None, 100)
        .await
        .unwrap();
    assert_eq!(got.unwrap().len(), 100);
    let err = fetch_shard_bytes(&log, "probe/over", None, 100)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Corrupt { .. }));
    assert_eq!(*log.ranges.lock().unwrap(), vec![0..101, 0..101]);

    let gone = fetch_shard_bytes(&log, "probe/none", None, 100)
        .await
        .unwrap();
    assert!(gone.is_none());
}

#[tokio::test]
async fn shard_limit_writer_index_skips_planted_listing() {
    let fx = Fx::new().await;
    fx.backup("a", &[("a", 1, 200 << 10)]).await;
    let entry = planted_shard_entry(5 << 30);
    let key = entry.key.clone();
    let store = PlantedListing::new(&fx.store, vec![entry]);
    let keys = fx.repo.read_keys();

    let mut index = Index::default();
    index
        .load_all(&store, keys.k_idx(), keys.repo_id())
        .await
        .unwrap();

    assert!(index.skipped().iter().any(|(k, _)| *k == key));
    assert!(!index.is_empty());
    assert_eq!(store.reads_of(&key), 0);
}

#[tokio::test]
async fn shard_limit_attested_legacy_shard_is_read_and_marked() {
    let fx = Fx::new().await;
    let (id, key) = put_big_shard(&fx).await;
    attest(&fx, &[id]).await;

    let set = load_shards(&fx.store, fx.repo.read_keys()).await.unwrap();
    assert!(set.shards.iter().any(|(s, _)| *s == id));
    assert!(set.oversized.contains(&id));
    assert!(set.skipped.iter().all(|(k, _)| *k != key));

    let report = rebuild_index(&fx.store, fx.repo.read_keys(), &|_, _| {})
        .await
        .unwrap();
    assert!(report.shards_skipped.iter().all(|(k, _)| *k != key));
}

#[tokio::test]
async fn shard_limit_unattested_shard_of_the_same_size_is_skipped() {
    let fx = Fx::new().await;
    let (id, key) = put_big_shard(&fx).await;

    let set = load_shards(&fx.store, fx.repo.read_keys()).await.unwrap();
    assert!(set.shards.iter().all(|(s, _)| *s != id));
    assert!(set.oversized.is_empty());
    assert!(set.skipped.iter().any(|(k, _)| *k == key));

    let report = rebuild_index(&fx.store, fx.repo.read_keys(), &|_, _| {})
        .await
        .unwrap();
    assert!(report.shards_skipped.iter().any(|(k, _)| *k == key));
}

#[tokio::test]
async fn shard_limit_entry_beyond_its_pack_is_skipped_not_fatal() {
    let fx = Fx::new().await;
    fx.backup("a", &[("a", 1, 200 << 10)]).await;
    let before = fx.survey(0).await;

    let keys = attacker_keys(&fx.repo);
    let mut record = open_all_shards(&fx.store, &keys).await[0].1.packs[0].clone();
    record.pack_size = PACK_PREFIX_LEN as u64 + 1;
    let id = plant_shard(&fx.store, &keys, vec![record], Vec::new()).await;

    let after = fx.survey(0).await;
    let planted = index_object_key(&id);
    assert!(after.shards.skipped.iter().any(|(k, _)| *k == planted));
    assert_eq!(after.snapshots, before.snapshots);
    assert_eq!(after.live.len(), before.live.len());
}

#[tokio::test]
async fn shard_limit_verify_rejects_pack_size_beyond_a_pack() {
    let fx = Fx::new().await;
    fx.backup("a", &[("a", 1, 200 << 10)]).await;
    let keys = attacker_keys(&fx.repo);
    let mut record = open_all_shards(&fx.store, &keys).await[0].1.packs[0].clone();
    record.pack_size = PACK_MAX_SIZE as u64 + 1;
    let id = plant_shard(&fx.store, &keys, vec![record], Vec::new()).await;

    let err = verify_shard(&fx.store, fx.repo.read_keys(), &id)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Corrupt { .. }));
    assert!(err.to_string().contains("larger than a pack can be"));
}

/// Records the ranges asked of `index/` and refuses to serve one whole.
struct IndexProbe {
    inner: LocalFsStore,
    ranges: Mutex<Vec<(String, Range<u64>)>>,
}

impl BlobStore for IndexProbe {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<(), BlobError> {
        self.inner.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        assert!(!key.starts_with("index/"), "a shard was read whole");
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, BlobError> {
        if key.starts_with("index/") {
            self.ranges
                .lock()
                .unwrap()
                .push((key.to_owned(), range.clone()));
        }
        self.inner.get_range(key, range).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        self.inner.list(prefix).await
    }
}

#[tokio::test]
async fn shard_limit_reader_probe_is_bounded() {
    let fx = Fx::new().await;
    let outcome = fx.backup("a", &[("a", 1, 200 << 10)]).await;
    let probe = IndexProbe {
        inner: LocalFsStore::new(fx.tmp.path().join("repo")),
        ranges: Mutex::new(Vec::new()),
    };
    let mut reader =
        Reader::open(&probe, fx.repo.read_keys().duplicate(), fx.repo.config()).unwrap();

    reader.list_dir(&outcome.snapshot_id, b"").await.unwrap();

    let named = reader
        .snapshot(&outcome.snapshot_id)
        .await
        .unwrap()
        .index_refs
        .len();
    let ranges = probe.ranges.lock().unwrap();
    assert!(named >= 1);
    assert_eq!(ranges.len(), named);
    assert!(ranges.iter().all(|(_, r)| *r == (0..MAX_SHARD_SIZE + 1)));
}

#[tokio::test]
async fn shard_limit_restore_reads_a_legacy_shard() {
    let fx = Fx::new().await;
    fx.backup("a", &[("a", 1, 300 << 10), ("b", 2, 200 << 10)])
        .await;
    let keys = attacker_keys(&fx.repo);
    let shards = open_all_shards(&fx.store, &keys).await;
    let real: Vec<_> = shards
        .iter()
        .flat_map(|(_, s)| s.packs.iter().cloned())
        .collect();
    for (id, _) in &shards {
        fs::remove_file(fx.tmp.path().join("repo").join(index_object_key(id))).unwrap();
    }

    let (id, bytes) = big_shard_with(&fx, real);
    fx.store
        .put_if_absent(&index_object_key(&id), Bytes::from(bytes))
        .await
        .unwrap();
    attest(&fx, &[id]).await;

    fx.assert_all_restore().await;
}

#[tokio::test]
async fn shard_limit_local_range_past_the_end_is_not_allocated() {
    let tmp = tempfile::tempdir().unwrap();
    let store = LocalFsStore::new(tmp.path().join("repo"));
    store
        .put_if_absent("obj/ten", Bytes::from_static(b"0123456789"))
        .await
        .unwrap();
    let got = store.get_range("obj/ten", 0..u64::MAX / 2).await.unwrap();
    assert_eq!(got.len(), 10);
}

#[tokio::test]
async fn shard_limit_confirm_unreferenced_reports_skipped() {
    let fx = Fx::new().await;
    fx.backup("a", &[("a", 1, 200 << 10)]).await;
    let keys = fx.repo.read_keys();
    let none = HashSet::new();
    assert!(
        confirm_unreferenced(&fx.store, keys, &none)
            .await
            .unwrap()
            .is_empty()
    );

    let (_, key) = put_big_shard(&fx).await;
    let skipped = confirm_unreferenced(&fx.store, keys, &none).await.unwrap();
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].0, key);
    assert!(skipped[0].1.contains("more than a shard can be"));
}

#[tokio::test]
async fn shard_limit_reader_skips_unattested_shard_over_limit() {
    let fx = Fx::new().await;
    fx.backup("a", &[("a", 1, 300 << 10), ("b", 2, 200 << 10)])
        .await;
    let outcome = fx.backup("c", &[("c", 3, 100 << 10)]).await;
    let keys = attacker_keys(&fx.repo);
    let shards = open_all_shards(&fx.store, &keys).await;
    let real: Vec<_> = shards
        .iter()
        .flat_map(|(_, s)| s.packs.iter().cloned())
        .collect();
    let honest: Vec<BlobId> = shards.iter().map(|(id, _)| *id).collect();
    attest(&fx, &honest).await;
    for id in &honest {
        fs::remove_file(fx.tmp.path().join("repo").join(index_object_key(id))).unwrap();
    }
    let (id, bytes) = big_shard_with(&fx, real);
    let key = index_object_key(&id);
    fx.store
        .put_if_absent(&key, Bytes::from(bytes))
        .await
        .unwrap();

    let probe = IndexProbe {
        inner: LocalFsStore::new(fx.tmp.path().join("repo")),
        ranges: Mutex::new(Vec::new()),
    };
    let mut reader =
        Reader::open(&probe, fx.repo.read_keys().duplicate(), fx.repo.config()).unwrap();
    let err = reader
        .list_dir(&outcome.snapshot_id, b"")
        .await
        .unwrap_err();

    assert!(matches!(err, Error::Corrupt { .. }), "{err}");
    assert!(err.to_string().contains("not in the index"), "{err}");
    assert!(
        probe
            .ranges
            .lock()
            .unwrap()
            .iter()
            .all(|(k, r)| *k != key || r.end <= MAX_SHARD_SIZE + 1)
    );
}

#[tokio::test]
async fn shard_limit_swapped_shard_is_read_by_range() {
    let fx = Fx::new().await;
    let (_, key) = put_big_shard(&fx).await;
    let ranged = RangedOnly::new(&fx.store, &["index/"]);

    let got = fetch_shard_bytes(&ranged, &key, Some(1024), MAX_SHARD_SIZE)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(got.len(), 1024);
    assert_eq!(ranged.ranges_of(&key), vec![0..1024]);
}

/// Lists one object with a size of its own, as a store that lies does.
struct Inflated {
    inner: LocalFsStore,
    key: String,
    size: u64,
}

impl BlobStore for Inflated {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<(), BlobError> {
        self.inner.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.inner.get_range(key, range).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        let mut out = self.inner.list(prefix).await?;
        for e in out.iter_mut().filter(|e| e.key == self.key) {
            e.size = self.size;
        }
        Ok(out)
    }
}

/// A repository whose only shard is a signed legacy one above
/// `MAX_SHARD_SIZE`, listed at `listed` bytes.
async fn signed_legacy_shard_listed_at(listed: u64) -> (Fx, BlobId, BlobId, Inflated) {
    let fx = Fx::new().await;
    let outcome = fx.backup("a", &[("a", 1, 300 << 10)]).await;
    let keys = attacker_keys(&fx.repo);
    let shards = open_all_shards(&fx.store, &keys).await;
    let real: Vec<_> = shards
        .iter()
        .flat_map(|(_, s)| s.packs.iter().cloned())
        .collect();
    for (id, _) in &shards {
        fs::remove_file(fx.tmp.path().join("repo").join(index_object_key(id))).unwrap();
    }
    let (id, bytes) = big_shard_with(&fx, real);
    let key = index_object_key(&id);
    fx.store
        .put_if_absent(&key, Bytes::from(bytes))
        .await
        .unwrap();
    attest(&fx, &[id]).await;
    let store = Inflated {
        inner: LocalFsStore::new(fx.tmp.path().join("repo")),
        key,
        size: listed,
    };
    (fx, outcome.snapshot_id, id, store)
}

#[tokio::test]
async fn shard_limit_attested_shard_is_read_up_to_the_signed_cap() {
    let (fx, snapshot, id, store) = signed_legacy_shard_listed_at(MAX_ATTESTED_SHARD_SIZE).await;

    let set = load_shards(&store, fx.repo.read_keys()).await.unwrap();
    assert!(set.shards.iter().any(|(s, _)| *s == id));
    assert!(set.oversized.contains(&id));
    assert!(set.skipped.iter().all(|(k, _)| *k != store.key));

    let mut reader =
        Reader::open(&store, fx.repo.read_keys().duplicate(), fx.repo.config()).unwrap();
    reader.list_dir(&snapshot, b"").await.unwrap();
}

#[tokio::test]
async fn shard_limit_attested_shard_over_the_signed_cap_is_skipped() {
    let (fx, snapshot, id, store) =
        signed_legacy_shard_listed_at(MAX_ATTESTED_SHARD_SIZE + 1).await;

    let set = load_shards(&store, fx.repo.read_keys()).await.unwrap();
    assert!(set.shards.iter().all(|(s, _)| *s != id));
    assert!(set.skipped.iter().any(|(k, _)| *k == store.key));

    let mut reader =
        Reader::open(&store, fx.repo.read_keys().duplicate(), fx.repo.config()).unwrap();
    let err = reader.list_dir(&snapshot, b"").await.unwrap_err();
    assert!(err.to_string().contains("not in the index"), "{err}");
}

/// One object of `size` zero bytes, served by range only, counting the
/// bytes it hands out.
struct Zeros {
    key: String,
    size: u64,
    served: Mutex<u64>,
}

impl BlobStore for Zeros {
    async fn put_if_absent(&self, _: &str, _: Bytes) -> Result<(), BlobError> {
        Err(BlobError::backend("read only"))
    }

    async fn get(&self, _: &str) -> Result<Bytes, BlobError> {
        Err(BlobError::backend("a whole object was read"))
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, BlobError> {
        if key != self.key {
            return Err(BlobError::NotFound);
        }
        let n = range.end.min(self.size).saturating_sub(range.start);
        *self.served.lock().unwrap() += n;
        Ok(Bytes::from(vec![0; n as usize]))
    }

    async fn list(&self, _: &str) -> Result<Vec<ListEntry>, BlobError> {
        Ok(Vec::new())
    }
}

fn zeros(id: &BlobId, size: u64) -> Zeros {
    Zeros {
        key: index_object_key(id),
        size,
        served: Mutex::new(0),
    }
}

#[tokio::test]
async fn shard_limit_attested_shard_without_listing_stops_at_the_signed_cap() {
    let id: BlobId = Entropy::os().bytes();
    let store = zeros(&id, MAX_ATTESTED_SHARD_SIZE + MAX_SHARD_SIZE);

    let err = fetch_attested_shard_bytes(&store, &id, None)
        .await
        .unwrap_err();

    assert!(matches!(err, Error::Corrupt { .. }), "{err}");
    assert!(err.to_string().contains("signed shard"), "{err}");
    let probe = MAX_SHARD_SIZE + 1;
    assert!(*store.served.lock().unwrap() <= probe + MAX_ATTESTED_SHARD_SIZE + 1);
}

#[tokio::test]
async fn shard_limit_attested_shard_of_whole_pieces_reads_to_its_end() {
    let size = 2 * MAX_SHARD_SIZE;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&vec![0; size as usize]);
    let id: BlobId = *hasher.finalize().as_bytes();
    let store = zeros(&id, size);

    let got = fetch_attested_shard_bytes(&store, &id, None)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(got.len() as u64, size);
}

#[tokio::test]
async fn shard_limit_overwritten_attested_shard_is_not_loaded() {
    let fx = Fx::new().await;
    let (id, mut bytes) = big_shard(&fx);
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    let key = index_object_key(&id);
    let len = bytes.len() as u64;
    fx.store
        .put_if_absent(&key, Bytes::from(bytes))
        .await
        .unwrap();
    attest(&fx, &[id]).await;
    let probe = IndexProbe {
        inner: LocalFsStore::new(fx.tmp.path().join("repo")),
        ranges: Mutex::new(Vec::new()),
    };

    for listed in [Some(len), None] {
        let err = fetch_attested_shard_bytes(&probe, &id, listed)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Corrupt { .. }), "{err}");
        assert!(err.to_string().contains("does not match its content"));
    }
    assert!(
        probe
            .ranges
            .lock()
            .unwrap()
            .iter()
            .all(|(_, r)| r.end - r.start <= MAX_SHARD_SIZE + 1)
    );

    let set = load_shards(&probe, fx.repo.read_keys()).await.unwrap();
    assert!(set.shards.iter().all(|(s, _)| *s != id));
    assert!(set.skipped.iter().any(|(k, _)| *k == key));
}

/// Fails the first read of an attestation, as a store that blips does.
struct AttestBlip {
    inner: LocalFsStore,
    failed: AtomicBool,
}

impl AttestBlip {
    fn blip(&self, key: &str) -> Result<(), BlobError> {
        if key.starts_with(ATTEST_PREFIX) && !self.failed.swap(true, Ordering::SeqCst) {
            return Err(BlobError::Denied {
                message: "blip".into(),
            });
        }
        Ok(())
    }
}

impl BlobStore for AttestBlip {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<(), BlobError> {
        self.inner.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        self.blip(key)?;
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.blip(key)?;
        self.inner.get_range(key, range).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        self.inner.list(prefix).await
    }
}

/// A reader lives as long as the server; one failed read of the attestations
/// must not leave the signed shards untrusted until a restart.
#[tokio::test]
async fn shard_limit_failed_attestation_read_is_retried() {
    let fx = Fx::new().await;
    let outcome = fx.backup("a", &[("a", 1, 300 << 10)]).await;
    let keys = attacker_keys(&fx.repo);
    let shards = open_all_shards(&fx.store, &keys).await;
    let real: Vec<_> = shards
        .iter()
        .flat_map(|(_, s)| s.packs.iter().cloned())
        .collect();
    for (id, _) in &shards {
        fs::remove_file(fx.tmp.path().join("repo").join(index_object_key(id))).unwrap();
    }
    let (id, bytes) = big_shard_with(&fx, real);
    fx.store
        .put_if_absent(&index_object_key(&id), Bytes::from(bytes))
        .await
        .unwrap();
    attest(&fx, &[id]).await;
    let store = AttestBlip {
        inner: LocalFsStore::new(fx.tmp.path().join("repo")),
        failed: AtomicBool::new(false),
    };

    let mut reader =
        Reader::open(&store, fx.repo.read_keys().duplicate(), fx.repo.config()).unwrap();
    reader.list_dir(&outcome.snapshot_id, b"").await.unwrap();
    assert!(store.failed.load(Ordering::SeqCst));
    reader.list_dir(&outcome.snapshot_id, b"").await.unwrap();
}
