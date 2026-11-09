//! Objects of a bucket are read with ranged reads bounded by the size the
//! listing showed or by the limit of their kind, never downloaded whole.

mod gc_support;
#[path = "adv_support/planted.rs"]
mod planted;

use std::collections::HashSet;
use std::ops::Range;

use gc_support::Fx;
use lodestore_format::config::MAX_CONFIG_SIZE;
use lodestore_format::error::Error;
use lodestore_format::gc::{Survey, survey};
use lodestore_format::reader::Reader;
use lodestore_format::snapshot::{MAX_SNAPSHOT_SIZE, list_snapshots, snapshot_object_key};
use lodestore_format::store::{BlobError, BlobStore, ListEntry};
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
