//! Objects of a bucket are read with ranged reads bounded by the size the
//! listing showed or by the limit of their kind, never downloaded whole.

mod gc_support;
#[path = "adv_support/planted.rs"]
mod planted;

use std::collections::HashSet;
use std::ops::Range;

use gc_support::Fx;
use lodestore_format::attest::{
    MAX_ATTEST_SIZE, peek_attestations, put_attestation, read_attestations,
};
use lodestore_format::check::FindingKind;
use lodestore_format::check::packs::check_packs;
use lodestore_format::check::structure::check_structure;
use lodestore_format::config::MAX_CONFIG_SIZE;
use lodestore_format::crypto::{BlobId, Entropy};
use lodestore_format::error::Error;
use lodestore_format::gc::repack::repack;
use lodestore_format::gc::{Survey, survey};
use lodestore_format::index::parse_hex32;
use lodestore_format::pack::pack_object_key;
use lodestore_format::reader::Reader;
use lodestore_format::snapshot::{MAX_SNAPSHOT_SIZE, list_snapshots, snapshot_object_key};
use lodestore_format::store::{BlobError, BlobStore, ListEntry};
use lodestore_format::verify_snapshot::snapshot_index_refs;
use lodestore_format::writer::Writer;
use planted::RangedOnly;

/// A store whose listing shows `key` at `shown` bytes whatever it holds.
struct Swapped<S> {
    inner: S,
    key: String,
    shown: u64,
}

impl<S: BlobStore> BlobStore for Swapped<S> {
    async fn put_if_absent(&self, key: &str, data: bytes::Bytes) -> Result<(), BlobError> {
        self.inner.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<bytes::Bytes, BlobError> {
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<bytes::Bytes, BlobError> {
        self.inner.get_range(key, range).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        let mut out = self.inner.list(prefix).await?;
        for e in out.iter_mut().filter(|e| e.key == self.key) {
            e.size = self.shown;
        }
        Ok(out)
    }
}

/// A store whose listing still shows objects that are gone. Like the S3
/// store, it answers an empty range without asking the bucket.
struct Stale<S> {
    inner: S,
    gone: Vec<ListEntry>,
}

impl<S: BlobStore> BlobStore for Stale<S> {
    async fn put_if_absent(&self, key: &str, data: bytes::Bytes) -> Result<(), BlobError> {
        self.inner.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<bytes::Bytes, BlobError> {
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<bytes::Bytes, BlobError> {
        if range.is_empty() {
            return Ok(bytes::Bytes::new());
        }
        self.inner.get_range(key, range).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        let mut out = self.inner.list(prefix).await?;
        out.extend(
            self.gone
                .iter()
                .filter(|e| e.key.starts_with(prefix))
                .cloned(),
        );
        Ok(out)
    }
}

async fn survey_on<S: BlobStore>(store: &S, fx: &Fx) -> Survey {
    survey(
        store,
        fx.repo.read_keys(),
        fx.repo.config(),
        &gc_support::plan_opts(0),
        &|_| {},
    )
    .await
    .unwrap()
}

fn grow(fx: &Fx, key: &str, len: u64) {
    std::fs::OpenOptions::new()
        .write(true)
        .open(fx.store.root().join(key))
        .unwrap()
        .set_len(len)
        .unwrap();
}

#[tokio::test]
async fn bounded_reads_snapshots_are_ranged() {
    let fx = Fx::new().await;
    fx.backup("host-a", &[("a", 1, 100 << 10)]).await;
    fx.backup("host-b", &[("b", 2, 100 << 10)]).await;
    let ranged = RangedOnly::new(&fx.store, &["snapshots/"]);

    let list = list_snapshots(&ranged, fx.repo.read_keys()).await.unwrap();
    let survey = survey_on(&ranged, &fx).await;

    assert_eq!(list.entries.len(), 2);
    assert!(list.skipped.is_empty());
    assert_eq!(survey.snapshots.len(), 2);
    for entry in fx.store.list("snapshots/").await.unwrap() {
        let ranges = ranged.ranges_of(&entry.key);
        assert!(!ranges.is_empty());
        assert!(ranges.iter().all(|r| *r == (0..entry.size)));
    }
}

#[tokio::test]
async fn bounded_reads_swapped_snapshot_is_refused() {
    let fx = Fx::new().await;
    fx.backup("host-a", &[("a", 1, 100 << 10)]).await;
    let out = fx.backup("host-b", &[("b", 2, 100 << 10)]).await;
    let key = snapshot_object_key(&out.snapshot_id);
    grow(&fx, &key, MAX_SNAPSHOT_SIZE + 1);
    let swapped = Swapped {
        inner: &fx.store,
        key: key.clone(),
        shown: 1024,
    };
    let ranged = RangedOnly::new(&swapped, &["snapshots/"]);

    let list = list_snapshots(&ranged, fx.repo.read_keys()).await.unwrap();
    let survey = survey_on(&ranged, &fx).await;

    assert_eq!(list.entries.len(), 1);
    assert!(list.skipped.iter().any(|(k, _)| *k == key));
    assert_eq!(survey.snapshots.len(), 1);
    assert!(
        survey.ignored.iter().any(|(k, _)| *k == key)
            || survey.quarantined.iter().any(|q| q.key == key)
    );
    assert!(ranged.ranges_of(&key).iter().all(|r| r.end <= 1024));
}

#[tokio::test]
async fn bounded_reads_writer_config_is_ranged() {
    let fx = Fx::new().await;
    fx.backup("host-a", &[("a", 1, 100 << 10)]).await;
    let keys = || fx.repo.read_keys().write_keys(fx.repo.config_hash());
    {
        let ranged = RangedOnly::new(&fx.store, &["config"]);
        Writer::connect(&ranged, keys()).await.unwrap();
        Writer::connect_attested(&ranged, keys(), &HashSet::new(), &HashSet::new())
            .await
            .unwrap();
        assert!(
            ranged
                .ranges_of("config")
                .iter()
                .all(|r| *r == (0..MAX_CONFIG_SIZE + 1))
        );
        assert!(!ranged.ranges_of("config").is_empty());
    }

    grow(&fx, "config", MAX_CONFIG_SIZE + 1);
    let ranged = RangedOnly::new(&fx.store, &["config"]);
    for result in [
        Writer::connect(&ranged, keys()).await.err(),
        Writer::connect_attested(&ranged, keys(), &HashSet::new(), &HashSet::new())
            .await
            .err(),
    ] {
        match result {
            Some(Error::Corrupt { object, .. }) => assert_eq!(object, "config"),
            other => panic!("expected a corrupt config, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn bounded_reads_reader_snapshot_is_ranged() {
    let fx = Fx::new().await;
    let out = fx.backup("host-a", &[("a", 1, 100 << 10)]).await;
    let ranged = RangedOnly::new(&fx.store, &["snapshots/"]);
    let reader = Reader::open(&ranged, fx.repo.read_keys().duplicate(), fx.repo.config()).unwrap();

    reader.snapshot(&out.snapshot_id).await.unwrap();

    assert_eq!(
        ranged.ranges_of(&snapshot_object_key(&out.snapshot_id)),
        vec![0..MAX_SNAPSHOT_SIZE + 1]
    );
}

/// A store that answers every ranged read of `key` with the whole object.
struct Greedy<S> {
    inner: S,
    key: String,
}

impl<S: BlobStore> BlobStore for Greedy<S> {
    async fn put_if_absent(&self, key: &str, data: bytes::Bytes) -> Result<(), BlobError> {
        self.inner.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<bytes::Bytes, BlobError> {
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<bytes::Bytes, BlobError> {
        if key == self.key {
            return self.inner.get(key).await;
        }
        self.inner.get_range(key, range).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        self.inner.list(prefix).await
    }
}

#[tokio::test]
async fn bounded_reads_snapshot_check_and_attestations_are_ranged() {
    let fx = Fx::new().await;
    let out = fx.backup("host-a", &[("a", 1, 100 << 10)]).await;
    let keys = fx.repo.read_keys();
    let attest_key = put_attestation(
        &fx.store,
        keys,
        &[[7u8; 32]],
        &[],
        Vec::new(),
        &mut Entropy::os(),
    )
    .await
    .unwrap();
    let listed = |key: &str| {
        let key = key.to_owned();
        let store = &fx.store;
        async move {
            store
                .list(&key)
                .await
                .unwrap()
                .into_iter()
                .find(|e| e.key == key)
                .unwrap()
                .size
        }
    };
    let attest_size = listed(&attest_key).await;
    let snapshot_key = snapshot_object_key(&out.snapshot_id);
    let snapshot_size = listed(&snapshot_key).await;
    let ranged = RangedOnly::new(&fx.store, &["locks/", "snapshots/"]);

    let set = read_attestations(&ranged, keys).await.unwrap();
    let peeked = peek_attestations(&ranged).await.unwrap();
    let refs = snapshot_index_refs(&ranged, keys, &out.snapshot_id)
        .await
        .unwrap();

    assert!(set.shards.contains(&[7u8; 32]) && set.rejected.is_empty());
    assert!(peeked.shards.contains(&[7u8; 32]));
    assert!(!refs.is_empty());
    assert_eq!(
        ranged.ranges_of(&attest_key),
        vec![0..attest_size, 0..attest_size]
    );
    assert_eq!(ranged.ranges_of(&snapshot_key), vec![0..snapshot_size]);

    grow(&fx, &attest_key, MAX_ATTEST_SIZE + 1);
    let swapped = Swapped {
        inner: &fx.store,
        key: attest_key.clone(),
        shown: attest_size - 1,
    };
    let ranged = RangedOnly::new(&swapped, &["locks/"]);
    let set = read_attestations(&ranged, keys).await.unwrap();
    peek_attestations(&ranged).await.unwrap();
    assert!(!set.shards.contains(&[7u8; 32]));
    assert!(set.rejected.iter().any(|(k, _)| *k == attest_key));
    assert!(
        ranged
            .ranges_of(&attest_key)
            .iter()
            .all(|r| r.end < attest_size)
    );
}

#[tokio::test]
async fn bounded_reads_packs_and_check_are_ranged() {
    let fx = Fx::new().await;
    let a = fx
        .backup("a", &[("shared", 1, 100 << 10), ("only-a", 2, 200 << 10)])
        .await;
    fx.backup("b", &[("shared", 1, 100 << 10), ("only-b", 3, 200 << 10)])
        .await;
    fx.forget(&a.snapshot_id).await;
    let keys = fx.repo.read_keys();
    let ranged = RangedOnly::new(&fx.store, &["snapshots/", "packs/", "config"]);

    let report = check_structure(&ranged, keys, fx.repo.config(), &|_, _, _| {})
        .await
        .unwrap();
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert!(!ranged.ranges_of("config").is_empty());
    assert!(
        ranged
            .ranges_of("config")
            .iter()
            .all(|r| *r == (0..MAX_CONFIG_SIZE + 1))
    );

    let packs: Vec<(BlobId, u64)> = fx
        .packs()
        .await
        .into_iter()
        .map(|(key, size)| (parse_hex32(key.rsplit('/').next().unwrap()).unwrap(), size))
        .collect();
    let report = check_packs(&ranged, keys, &packs, u64::MAX, &|_, _, _| {})
        .await
        .unwrap();
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    for (id, size) in &packs {
        assert!(ranged.ranges_of(&pack_object_key(id)).contains(&(0..*size)));
    }

    let survey = fx.survey(50).await;
    let ranged = RangedOnly::new(&fx.store, &["packs/"]);
    let out = repack(&ranged, keys, &survey, &mut Entropy::os(), &|_, _| {})
        .await
        .unwrap();
    assert!(!out.replaced.is_empty());
    for stat in &survey.plan.repack {
        assert_eq!(
            ranged.ranges_of(&pack_object_key(&stat.pack_id)),
            vec![0..stat.size]
        );
    }

    let (id, size) = packs[0];
    let greedy = Greedy {
        inner: &fx.store,
        key: pack_object_key(&id),
    };
    let report = check_packs(&greedy, keys, &[(id, size - 1)], u64::MAX, &|_, _, _| {})
        .await
        .unwrap();
    assert!(report.read_errors.is_empty(), "{:?}", report.read_errors);
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.kind == FindingKind::UnreadablePack)
    );
}

#[tokio::test]
async fn bounded_reads_empty_listing_of_a_vanished_object_is_not_found() {
    let fx = Fx::new().await;
    let stale = Stale {
        inner: &fx.store,
        gone: vec![ListEntry {
            key: "locks/attest-gone".into(),
            size: 0,
            modified_ms: None,
        }],
    };
    let set = read_attestations(&stale, fx.repo.read_keys())
        .await
        .unwrap();
    assert!(set.incomplete);
    assert!(set.rejected.is_empty(), "{:?}", set.rejected);
}
