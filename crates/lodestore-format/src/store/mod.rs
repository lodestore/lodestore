mod local;
pub mod location;
pub mod opendal;
pub mod policy;
pub mod probe;
pub mod purge;
pub mod s3;
pub mod sftp;
mod url;

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
pub use s3::{S3Options, S3Provider, S3Store, S3Url, StaticCredentials};
pub use sftp::SftpStore;

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
    #[error("the SFTP server's host key changed: pinned {pinned}, offered {offered}")]
    HostKeyMismatch { pinned: String, offered: String },
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

    /// The size of the object, `None` when it is not there. The default asks
    /// the listing for the exact key; a store with a cheaper stat overrides it.
    fn object_size(
        &self,
        key: &str,
    ) -> impl Future<Output = std::result::Result<Option<u64>, BlobError>> + Send {
        async move {
            let listed = self.list(key).await?;
            Ok(listed.into_iter().find(|e| e.key == key).map(|e| e.size))
        }
    }

    /// Whether an object becomes visible only once it is complete. A store
    /// that can leave a torn object under the final name (a direct SFTP write)
    /// returns `false`, and then an existing name is trusted only after its
    /// bytes are compared. No default: a wrapper that forgot to forward the
    /// answer would trust a torn object on size alone.
    fn publishes_atomically(&self) -> bool;
}

/// Deletion, kept apart from [`BlobStore`] so that code which only needs to
/// write (the backup `Writer`) cannot be handed a way to remove anything.
pub trait BlobDelete: Send + Sync {
    /// Removes the object; `NotFound` when it is not there.
    fn delete(&self, key: &str) -> impl Future<Output = std::result::Result<(), BlobError>> + Send;
}

/// Writes an object whose name is derived from its bytes. `AlreadyExists`
/// counts as success only when the object under the name is the one written;
/// otherwise the name is burned and the caller must pick another.
pub async fn put_named<S: BlobStore>(store: &S, key: &str, bytes: Vec<u8>) -> Result<()> {
    put_named_bytes(store, key, &Bytes::from(bytes)).await
}

/// [`put_named`] for bytes the caller keeps, so that a burned name can be
/// retried without a copy.
pub async fn put_named_bytes<S: BlobStore>(store: &S, key: &str, bytes: &Bytes) -> Result<()> {
    match store.put_if_absent(key, bytes.clone()).await {
        Ok(()) => Ok(()),
        Err(BlobError::AlreadyExists) => match store.object_size(key).await {
            Ok(Some(size)) => confirm_present(store, key, bytes, size).await,
            Ok(None) => Err(BlobError::backend(format!(
                "{key} reported as existing, then not found"
            ))
            .into()),
            // An atomic store never shows an incomplete object, and a
            // write-only key may not stat what it just created.
            Err(BlobError::Denied { .. }) if store.publishes_atomically() => Ok(()),
            Err(e) => Err(e.into()),
        },
        // A key that may not overwrite can be refused instead of told the
        // object exists; that is a presence only if the object is seen.
        Err(denied @ BlobError::Denied { .. }) => match store.object_size(key).await {
            Ok(Some(size)) => confirm_present(store, key, bytes, size).await,
            _ => Err(denied.into()),
        },
        Err(e) => Err(e.into()),
    }
}

/// An object of `size` bytes holds the name. It is the one written when the
/// size agrees and, on a store that can leave a torn object under the final
/// name, when the bytes agree too: a direct write arrives out of order, so a
/// full-size object can still have a hole. The read is capped at one byte
/// past what was written.
async fn confirm_present<S: BlobStore>(
    store: &S,
    key: &str,
    bytes: &Bytes,
    size: u64,
) -> Result<()> {
    let len = bytes.len() as u64;
    if size != len {
        return Err(Error::NameBurned { key: key.into() });
    }
    if store.publishes_atomically() {
        return Ok(());
    }
    let seen = store.get_range(key, 0..len + 1).await?;
    if seen == *bytes {
        Ok(())
    } else {
        Err(Error::NameBurned { key: key.into() })
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

    fn object_size(
        &self,
        key: &str,
    ) -> impl Future<Output = std::result::Result<Option<u64>, BlobError>> + Send {
        (**self).object_size(key)
    }

    fn publishes_atomically(&self) -> bool {
        (**self).publishes_atomically()
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
    Sftp(SftpStore),
}

impl Store {
    pub fn conditional_write_supported(&self) -> bool {
        match self {
            Store::Local(_) => true,
            Store::S3(s) => s.conditional_write_supported(),
            Store::Opendal(s) => s.conditional_write_supported(),
            Store::Sftp(_) => true,
        }
    }

    /// Whether a delete marker may be swept as a removal. False on B2, where a
    /// host's hide marker is the only trace of its delete.
    pub fn sweeps_delete_markers(&self) -> bool {
        match self {
            Store::S3(s) => s.sweeps_delete_markers(),
            Store::Local(_) | Store::Opendal(_) | Store::Sftp(_) => true,
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
            Store::Sftp(_) => Ok(None),
        }
    }

    /// The store's own last-modified time for `key` in unix milliseconds;
    /// `None` when the backend does not report one. Not part of [`BlobStore`].
    pub async fn modified_ms(&self, key: &str) -> std::result::Result<Option<i64>, BlobError> {
        match self {
            Store::Local(s) => s.modified_ms(key).await,
            Store::S3(s) => s.modified_ms(key).await,
            Store::Opendal(s) => s.modified_ms(key).await,
            Store::Sftp(s) => s.modified_ms(key).await,
        }
    }

    fn probe_op(&self, key: &str) -> std::result::Result<Option<&::opendal::Operator>, BlobError> {
        if !probe::is_probe_key(key) {
            return Err(BlobError::backend(format!("{key:?} is not a probe object")));
        }
        Ok(match self {
            Store::Local(_) => None,
            Store::S3(s) => Some(&s.op),
            Store::Opendal(s) => Some(&s.op),
            Store::Sftp(_) => None,
        })
    }

    /// The versions of a probe object as (id, size); `None` when the store
    /// keeps no versions.
    async fn versions_for_probe(
        &self,
        key: &str,
    ) -> std::result::Result<Option<Vec<(String, u64)>>, BlobError> {
        match self.probe_op(key)? {
            Some(op) => purge::versions_op(op, key).await,
            None => Ok(None),
        }
    }

    async fn read_version_for_probe(
        &self,
        key: &str,
        version: &str,
        size: u64,
    ) -> std::result::Result<Bytes, BlobError> {
        match self.probe_op(key)? {
            Some(op) => purge::read_version_op(op, key, version, size).await,
            None => Err(BlobError::backend("not supported on a local folder")),
        }
    }

    async fn delete_version_for_probe(
        &self,
        key: &str,
        version: &str,
    ) -> std::result::Result<(), BlobError> {
        match self.probe_op(key)? {
            Some(op) => purge::delete_version_op(op, key, version).await,
            None => Err(BlobError::backend("not supported on a local folder")),
        }
    }

    async fn overwrite_for_probe(
        &self,
        key: &str,
        data: Bytes,
    ) -> std::result::Result<(), BlobError> {
        if !probe::is_probe_key(key) {
            return Err(BlobError::backend(format!("{key:?} is not a probe object")));
        }
        match self {
            Store::Local(_) => Err(BlobError::backend("not supported on a local folder")),
            Store::S3(s) => s.overwrite_for_probe(key, data).await,
            Store::Opendal(s) => s.overwrite_for_probe(key, data).await,
            Store::Sftp(s) => s.overwrite_for_probe(key, data).await,
        }
    }
}

impl BlobStore for Store {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> std::result::Result<(), BlobError> {
        match self {
            Store::Local(s) => s.put_if_absent(key, data).await,
            Store::S3(s) => s.put_if_absent(key, data).await,
            Store::Opendal(s) => s.put_if_absent(key, data).await,
            Store::Sftp(s) => s.put_if_absent(key, data).await,
        }
    }

    async fn get(&self, key: &str) -> std::result::Result<Bytes, BlobError> {
        match self {
            Store::Local(s) => s.get(key).await,
            Store::S3(s) => s.get(key).await,
            Store::Opendal(s) => s.get(key).await,
            Store::Sftp(s) => s.get(key).await,
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
            Store::Sftp(s) => s.get_range(key, range).await,
        }
    }

    async fn list(&self, prefix: &str) -> std::result::Result<Vec<ListEntry>, BlobError> {
        match self {
            Store::Local(s) => s.list(prefix).await,
            Store::S3(s) => s.list(prefix).await,
            Store::Opendal(s) => s.list(prefix).await,
            Store::Sftp(s) => s.list(prefix).await,
        }
    }

    async fn ensure_prefixes(&self, prefixes: &[&str]) -> std::result::Result<(), BlobError> {
        match self {
            Store::Local(s) => s.ensure_prefixes(prefixes).await,
            Store::S3(s) => s.ensure_prefixes(prefixes).await,
            Store::Opendal(s) => s.ensure_prefixes(prefixes).await,
            Store::Sftp(s) => s.ensure_prefixes(prefixes).await,
        }
    }

    async fn object_size(&self, key: &str) -> std::result::Result<Option<u64>, BlobError> {
        match self {
            Store::Local(s) => s.object_size(key).await,
            Store::S3(s) => s.object_size(key).await,
            Store::Opendal(s) => s.object_size(key).await,
            Store::Sftp(s) => s.object_size(key).await,
        }
    }

    fn publishes_atomically(&self) -> bool {
        match self {
            Store::Local(s) => s.publishes_atomically(),
            Store::S3(s) => s.publishes_atomically(),
            Store::Opendal(s) => s.publishes_atomically(),
            Store::Sftp(s) => s.publishes_atomically(),
        }
    }
}

impl BlobDelete for Store {
    async fn delete(&self, key: &str) -> std::result::Result<(), BlobError> {
        match self {
            Store::Local(s) => s.delete(key).await,
            Store::S3(s) => s.delete(key).await,
            Store::Opendal(s) => s.delete(key).await,
            Store::Sftp(s) => s.delete(key).await,
        }
    }
}

/// Opens a repository location: a folder path, absolute or relative, or
/// `s3:http(s)://host[:port]/bucket[/prefix]`, `azblob://`, `gcs://` or `sftp://`.
pub fn open_url(url: &str) -> Result<Store> {
    if url.is_empty() {
        return Err(Error::InvalidInput("repository location is empty".into()));
    }
    if url.starts_with("s3:") {
        return Ok(Store::S3(S3Store::open(url, None)?));
    }
    if ["azblob://", "gcs://", "sftp://"]
        .iter()
        .any(|scheme| url.starts_with(scheme))
    {
        return RepoLocation::from_url(url)?.open(None);
    }
    let root = std::path::absolute(url)?;
    Ok(Store::Local(LocalFsStore::new(root)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweeps_delete_markers_is_false_only_for_b2() {
        let s3 = |provider| {
            Store::S3(
                S3Store::with_options(
                    &S3Options {
                        endpoint: Some("http://127.0.0.1:9".into()),
                        bucket: "b".into(),
                        prefix: String::new(),
                        region: "us-east-1".into(),
                        virtual_host_style: false,
                        provider,
                    },
                    None,
                )
                .unwrap(),
            )
        };
        assert!(s3(S3Provider::Default).sweeps_delete_markers());
        assert!(s3(S3Provider::Default).conditional_write_supported());
        assert!(!s3(S3Provider::B2).sweeps_delete_markers());
        assert!(!s3(S3Provider::B2).conditional_write_supported());
        let dir = tempfile::tempdir().unwrap();
        let local = Store::Local(LocalFsStore::new(dir.path().to_path_buf()));
        assert!(local.sweeps_delete_markers());
    }

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

        fn publishes_atomically(&self) -> bool {
            true
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

        fn publishes_atomically(&self) -> bool {
            true
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
