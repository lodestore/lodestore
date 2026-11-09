#![allow(dead_code)]

//! A store wrapper that lists objects the bucket does not hold. It names
//! `bytes::Bytes` through the `BlobStore` signatures, which the command line
//! crate does not depend on in the first release, so it lives apart from
//! `mod.rs`, which that crate also compiles.

use lodestore_format::crypto::Entropy;
use lodestore_format::index::index_object_key;
use lodestore_format::store::{BlobError, BlobStore, ListEntry};
use std::ops::Range;
use std::sync::Mutex;

/// A store that lists objects the bucket does not hold, as a hostile bucket
/// can with a size it makes up, and records every attempt to read one.
pub struct PlantedListing<S> {
    pub inner: S,
    pub planted: Vec<ListEntry>,
    pub reads: Mutex<Vec<String>>,
}

impl<S> PlantedListing<S> {
    pub fn new(inner: S, planted: Vec<ListEntry>) -> Self {
        Self {
            inner,
            planted,
            reads: Mutex::new(Vec::new()),
        }
    }

    pub fn reads_of(&self, key: &str) -> usize {
        self.reads
            .lock()
            .unwrap()
            .iter()
            .filter(|k| *k == key)
            .count()
    }

    fn trip(&self, key: &str) -> Result<(), BlobError> {
        if self.planted.iter().any(|e| e.key == key) {
            self.reads.lock().unwrap().push(key.to_owned());
            return Err(BlobError::backend("a planted object was read"));
        }
        Ok(())
    }
}

impl<S: BlobStore> BlobStore for PlantedListing<S> {
    async fn put_if_absent(&self, key: &str, data: bytes::Bytes) -> Result<(), BlobError> {
        self.inner.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<bytes::Bytes, BlobError> {
        self.trip(key)?;
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<bytes::Bytes, BlobError> {
        self.trip(key)?;
        self.inner.get_range(key, range).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        let mut out = self.inner.list(prefix).await?;
        out.extend(
            self.planted
                .iter()
                .filter(|e| e.key.starts_with(prefix))
                .cloned(),
        );
        Ok(out)
    }
}

/// A store that refuses to hand over a whole object under the given prefixes
/// and records every range read, to prove those objects are read by range only.
pub struct RangedOnly<S> {
    pub inner: S,
    pub prefixes: Vec<String>,
    pub ranges: Mutex<Vec<(String, Range<u64>)>>,
}

impl<S> RangedOnly<S> {
    pub fn new(inner: S, prefixes: &[&str]) -> Self {
        Self {
            inner,
            prefixes: prefixes.iter().map(|p| (*p).to_owned()).collect(),
            ranges: Mutex::new(Vec::new()),
        }
    }

    pub fn ranges_of(&self, key: &str) -> Vec<Range<u64>> {
        self.ranges
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, r)| r.clone())
            .collect()
    }

    pub fn ranges_under(&self, prefix: &str) -> Vec<(String, Range<u64>)> {
        self.ranges
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .cloned()
            .collect()
    }
}

impl<S: BlobStore> BlobStore for RangedOnly<S> {
    async fn put_if_absent(&self, key: &str, data: bytes::Bytes) -> Result<(), BlobError> {
        self.inner.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<bytes::Bytes, BlobError> {
        if self.prefixes.iter().any(|p| key.starts_with(p.as_str())) {
            return Err(BlobError::backend("a whole object was read"));
        }
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<bytes::Bytes, BlobError> {
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

/// A listing entry for an `index/` object of the given size that is not there.
pub fn planted_shard_entry(size: u64) -> ListEntry {
    let id: [u8; 32] = Entropy::os().bytes();
    ListEntry {
        key: index_object_key(&id),
        size,
        modified_ms: None,
    }
}
