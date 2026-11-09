mod local;
pub mod location;
pub mod opendal;
pub mod policy;
pub mod probe;
pub mod purge;
pub mod s3;

use std::fmt;
use std::future::Future;
use std::ops::Range;
use std::time::Duration;

use bytes::Bytes;

use crate::error::{Error, Result};

pub use local::LocalFsStore;
pub use location::RepoLocation;
pub use opendal::OpendalStore;
pub use purge::{BlobPurge, PurgeOutcome, RetainedVersion};
pub use s3::{S3Options, S3Store, S3Url, StaticCredentials};

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("object already exists")]
    AlreadyExists,
    #[error("object not found")]
    NotFound,
    #[error("access denied: {message}")]
    Denied { message: String },
    #[error("backend error: {message}")]
    Backend { message: String, temporary: bool },
}

impl BlobError {
    pub fn backend(message: impl Into<String>) -> Self {
        BlobError::Backend {
            message: message.into(),
            temporary: false,
        }
    }
}

/// A signed, time-limited GET URL for one object. The URL grants read access,
/// so it stays out of `Debug` output.
#[derive(Clone, PartialEq, Eq)]
pub struct Presigned {
    pub url: String,
    pub expires_at_ms: i64,
}

impl fmt::Debug for Presigned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Presigned")
            .field("expires_at_ms", &self.expires_at_ms)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListEntry {
    pub key: String,
    pub size: u64,
    /// When the store last wrote the object, in unix milliseconds; `None`
    /// when the backend does not report it.
    pub modified_ms: Option<i64>,
}

/// Create-only object storage. There is deliberately no unconditional write
/// and no delete here: every object is written once under a name derived from
/// its content, and a second write of the same key reports `AlreadyExists`.
pub trait BlobStore: Send + Sync {
    fn put_if_absent(
        &self,
        key: &str,
        data: Bytes,
    ) -> impl Future<Output = std::result::Result<(), BlobError>> + Send;

    fn get(&self, key: &str) -> impl Future<Output = std::result::Result<Bytes, BlobError>> + Send;

    fn get_range(
        &self,
        key: &str,
        range: Range<u64>,
    ) -> impl Future<Output = std::result::Result<Bytes, BlobError>> + Send;

    fn list(
        &self,
        prefix: &str,
    ) -> impl Future<Output = std::result::Result<Vec<ListEntry>, BlobError>> + Send;

    /// Makes sure the given key prefixes exist as containers. Object stores
    /// have no directories, so the default does nothing.
    fn ensure_prefixes(
        &self,
        _prefixes: &[&str],
    ) -> impl Future<Output = std::result::Result<(), BlobError>> + Send {
        async { Ok(()) }
    }
}

/// Deletion, kept apart from [`BlobStore`] so that code which only needs to
/// write (the backup `Writer`) cannot be handed a way to remove anything.
pub trait BlobDelete: Send + Sync {
    /// Removes the object; `NotFound` when it is not there.
    fn delete(&self, key: &str) -> impl Future<Output = std::result::Result<(), BlobError>> + Send;
}

/// Writes an object whose name is derived from its bytes. `AlreadyExists`
/// then means the identical object is already there, which is success.
pub async fn put_named<S: BlobStore>(store: &S, key: &str, bytes: Vec<u8>) -> Result<()> {
    match store.put_if_absent(key, Bytes::from(bytes)).await {
        Ok(()) | Err(BlobError::AlreadyExists) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Reads an object of at most `max` bytes with one ranged read. With the size
/// the listing showed it reads exactly that many bytes, so an object swapped
/// for a larger one after the listing is never downloaded beyond it; without
/// one it asks for a byte more than `max` to recognise an excess. The body is
/// checked again, for a store that returned more than was asked.
pub async fn read_bounded<S: BlobStore>(
    store: &S,
    key: &str,
    listed: Option<u64>,
    max: u64,
    what: &str,
) -> Result<Bytes> {
    match listed {
        Some(size) => {
            crate::error::within_size(key, size, max, what)?;
            // An empty range is answered without asking the store; one byte
            // makes a vanished object a `NotFound`.
            let bytes = store.get_range(key, 0..size.max(1)).await?;
            if bytes.len() as u64 > size {
                return Err(Error::corrupt(key, "longer than the listing"));
            }
            Ok(bytes)
        }
        None => {
            let bytes = store.get_range(key, 0..max.saturating_add(1)).await?;
            crate::error::within_size(key, bytes.len() as u64, max, what)?;
            Ok(bytes)
        }
    }
}

impl<T: BlobStore> BlobStore for &T {
    fn put_if_absent(
        &self,
        key: &str,
        data: Bytes,
    ) -> impl Future<Output = std::result::Result<(), BlobError>> + Send {
        (**self).put_if_absent(key, data)
    }

    fn get(&self, key: &str) -> impl Future<Output = std::result::Result<Bytes, BlobError>> + Send {
        (**self).get(key)
    }

    fn get_range(
        &self,
        key: &str,
        range: Range<u64>,
    ) -> impl Future<Output = std::result::Result<Bytes, BlobError>> + Send {
        (**self).get_range(key, range)
    }

    fn list(
        &self,
        prefix: &str,
    ) -> impl Future<Output = std::result::Result<Vec<ListEntry>, BlobError>> + Send {
        (**self).list(prefix)
    }

    fn ensure_prefixes(
        &self,
        prefixes: &[&str],
    ) -> impl Future<Output = std::result::Result<(), BlobError>> + Send {
        (**self).ensure_prefixes(prefixes)
    }
}

impl<T: BlobDelete> BlobDelete for &T {
    fn delete(&self, key: &str) -> impl Future<Output = std::result::Result<(), BlobError>> + Send {
        (**self).delete(key)
    }
}

pub enum Store {
    Local(LocalFsStore),
    S3(S3Store),
    Opendal(OpendalStore),
}

impl Store {
    pub fn conditional_write_supported(&self) -> bool {
        match self {
            Store::Local(_) | Store::S3(_) => true,
            Store::Opendal(s) => s.conditional_write_supported(),
        }
    }

    /// A URL that serves `key` to whoever holds it, for `ttl`. `None` when the
    /// store cannot sign URLs (a local folder). Not part of [`BlobStore`]: the
    /// trait stays what a writer may be handed.
    pub async fn presign_read(
        &self,
        key: &str,
        ttl: Duration,
    ) -> std::result::Result<Option<Presigned>, BlobError> {
        match self {
            Store::Local(_) => Ok(None),
            Store::S3(s) => s.presign_read(key, ttl).await,
            Store::Opendal(s) => s.presign_read(key, ttl).await,
        }
    }

    /// The store's own last-modified time for `key` in unix milliseconds;
    /// `None` when the backend does not report one. Not part of [`BlobStore`].
    pub async fn modified_ms(&self, key: &str) -> std::result::Result<Option<i64>, BlobError> {
        match self {
            Store::Local(s) => s.modified_ms(key).await,
            Store::S3(s) => s.modified_ms(key).await,
            Store::Opendal(s) => s.modified_ms(key).await,
        }
    }

    async fn overwrite_for_probe(
        &self,
        key: &str,
        data: Bytes,
    ) -> std::result::Result<(), BlobError> {
        match self {
            Store::Local(_) => Err(BlobError::backend("not supported on a local folder")),
            Store::S3(s) => s.overwrite_for_probe(key, data).await,
            Store::Opendal(s) => s.overwrite_for_probe(key, data).await,
        }
    }
}

impl BlobStore for Store {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> std::result::Result<(), BlobError> {
        match self {
            Store::Local(s) => s.put_if_absent(key, data).await,
            Store::S3(s) => s.put_if_absent(key, data).await,
            Store::Opendal(s) => s.put_if_absent(key, data).await,
        }
    }

    async fn get(&self, key: &str) -> std::result::Result<Bytes, BlobError> {
        match self {
            Store::Local(s) => s.get(key).await,
            Store::S3(s) => s.get(key).await,
            Store::Opendal(s) => s.get(key).await,
        }
    }

    async fn get_range(
        &self,
        key: &str,
        range: Range<u64>,
    ) -> std::result::Result<Bytes, BlobError> {
        match self {
            Store::Local(s) => s.get_range(key, range).await,
            Store::S3(s) => s.get_range(key, range).await,
            Store::Opendal(s) => s.get_range(key, range).await,
        }
    }

    async fn list(&self, prefix: &str) -> std::result::Result<Vec<ListEntry>, BlobError> {
        match self {
            Store::Local(s) => s.list(prefix).await,
            Store::S3(s) => s.list(prefix).await,
            Store::Opendal(s) => s.list(prefix).await,
        }
    }

    async fn ensure_prefixes(&self, prefixes: &[&str]) -> std::result::Result<(), BlobError> {
        match self {
            Store::Local(s) => s.ensure_prefixes(prefixes).await,
            Store::S3(s) => s.ensure_prefixes(prefixes).await,
            Store::Opendal(s) => s.ensure_prefixes(prefixes).await,
        }
    }
}

impl BlobDelete for Store {
    async fn delete(&self, key: &str) -> std::result::Result<(), BlobError> {
        match self {
            Store::Local(s) => s.delete(key).await,
            Store::S3(s) => s.delete(key).await,
            Store::Opendal(s) => s.delete(key).await,
        }
    }
}

/// Opens a repository location: a folder path, absolute or relative, or
/// `s3:http(s)://host[:port]/bucket[/prefix]`.
pub fn open_url(url: &str) -> Result<Store> {
    if url.is_empty() {
        return Err(Error::InvalidInput("repository location is empty".into()));
    }
    if url.starts_with("s3:") {
        return Ok(Store::S3(S3Store::open(url, None)?));
    }
    let root = std::path::absolute(url)?;
    Ok(Store::Local(LocalFsStore::new(root)))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Generous(Bytes);

    impl BlobStore for Generous {
        async fn put_if_absent(&self, _: &str, _: Bytes) -> std::result::Result<(), BlobError> {
            Ok(())
        }
        async fn get(&self, _: &str) -> std::result::Result<Bytes, BlobError> {
            Ok(self.0.clone())
        }
        async fn get_range(&self, _: &str, _: Range<u64>) -> std::result::Result<Bytes, BlobError> {
            Ok(self.0.clone())
        }
        async fn list(&self, _: &str) -> std::result::Result<Vec<ListEntry>, BlobError> {
            Ok(Vec::new())
        }
    }

    /// Answers an empty range without asking the store, as `get_range_op` does.
    struct EmptyRangeUnasked(LocalFsStore);

    impl BlobStore for EmptyRangeUnasked {
        async fn put_if_absent(&self, k: &str, d: Bytes) -> std::result::Result<(), BlobError> {
            self.0.put_if_absent(k, d).await
        }
        async fn get(&self, k: &str) -> std::result::Result<Bytes, BlobError> {
            self.0.get(k).await
        }
        async fn get_range(&self, k: &str, r: Range<u64>) -> std::result::Result<Bytes, BlobError> {
            if r.is_empty() {
                return Ok(Bytes::new());
            }
            self.0.get_range(k, r).await
        }
        async fn list(&self, p: &str) -> std::result::Result<Vec<ListEntry>, BlobError> {
            self.0.list(p).await
        }
    }

    async fn local(body: &[u8]) -> (tempfile::TempDir, LocalFsStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalFsStore::new(dir.path().to_path_buf());
        store
            .put_if_absent("obj", Bytes::copy_from_slice(body))
            .await
            .unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn read_bounded_accepts_a_body_of_exactly_max() {
        let (_d, store) = local(&[7; 16]).await;
        assert_eq!(
            read_bounded(&store, "obj", None, 16, "x")
                .await
                .unwrap()
                .len(),
            16
        );
        assert_eq!(
            read_bounded(&store, "obj", Some(16), 16, "x")
                .await
                .unwrap()
                .len(),
            16
        );
    }

    #[tokio::test]
    async fn read_bounded_refuses_one_byte_over_without_a_listing() {
        let (_d, store) = local(&[7; 17]).await;
        assert!(matches!(
            read_bounded(&store, "obj", None, 16, "x").await,
            Err(Error::Corrupt { .. })
        ));
    }

    #[tokio::test]
    async fn read_bounded_refuses_a_listing_over_max_and_a_longer_body() {
        let (_d, store) = local(&[7; 8]).await;
        assert!(matches!(
            read_bounded(&store, "obj", Some(17), 16, "x").await,
            Err(Error::Corrupt { .. })
        ));
        let greedy = Generous(Bytes::from_static(&[1; 32]));
        assert!(matches!(
            read_bounded(&greedy, "obj", Some(8), 16, "x").await,
            Err(Error::Corrupt { .. })
        ));
    }

    #[tokio::test]
    async fn read_bounded_reports_a_missing_object() {
        let (_d, store) = local(&[]).await;
        assert!(matches!(
            read_bounded(&store, "absent", None, 16, "x").await,
            Err(Error::Store(BlobError::NotFound))
        ));
        let store = EmptyRangeUnasked(store);
        assert!(matches!(
            read_bounded(&store, "absent", Some(0), 16, "x").await,
            Err(Error::Store(BlobError::NotFound))
        ));
        assert!(
            read_bounded(&store, "obj", Some(0), 16, "x")
                .await
                .unwrap()
                .is_empty()
        );
        let greedy = Generous(Bytes::from_static(&[1]));
        assert!(matches!(
            read_bounded(&greedy, "obj", Some(0), 16, "x").await,
            Err(Error::Corrupt { .. })
        ));
    }
}
