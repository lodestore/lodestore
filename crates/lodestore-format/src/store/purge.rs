use std::future::Future;

use opendal::Operator;

use super::s3::{check_key, map_err, timestamp_ms};
use super::{BlobError, Store};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedVersion {
    pub version: String,
    pub size: u64,
    pub modified_ms: Option<i64>,
}

#[derive(Debug)]
pub enum PurgeOutcome {
    Purged {
        versions: u32,
        bytes: u64,
    },
    NotFound,
    Retained {
        purged_versions: u32,
        purged_bytes: u64,
        retained: Vec<RetainedVersion>,
    },
    Failed(BlobError),
}

/// Physical removal of an object including every stored version, kept apart
/// from [`BlobDelete`] and `BlobStore` so that only code handed a store with
/// this capability (the server) can free space; the backup `Writer` cannot.
pub trait BlobPurge: Send + Sync {
    /// Deletes every version of `key`, then its delete markers. A version
    /// the store refuses to delete because it is locked is reported as
    /// retained, never as an error, and then the markers stay so the object
    /// does not reappear in listings.
    fn purge(&self, key: &str) -> impl Future<Output = PurgeOutcome> + Send;

    /// Keys under `prefix` whose current entry is a delete marker written at
    /// or before `older_than_ms`. Empty on stores without versions.
    fn deleted_keys(
        &self,
        prefix: &str,
        older_than_ms: i64,
    ) -> impl Future<Output = Result<Vec<String>, BlobError>> + Send;
}

impl<T: BlobPurge> BlobPurge for &T {
    fn purge(&self, key: &str) -> impl Future<Output = PurgeOutcome> + Send {
        (**self).purge(key)
    }

    fn deleted_keys(
        &self,
        prefix: &str,
        older_than_ms: i64,
    ) -> impl Future<Output = Result<Vec<String>, BlobError>> + Send {
        (**self).deleted_keys(prefix, older_than_ms)
    }
}

impl BlobPurge for Store {
    async fn purge(&self, key: &str) -> PurgeOutcome {
        match self {
            Store::Local(s) => s.purge(key).await,
            Store::S3(s) => s.purge(key).await,
            Store::Opendal(s) => s.purge(key).await,
        }
    }

    async fn deleted_keys(
        &self,
        prefix: &str,
        older_than_ms: i64,
    ) -> Result<Vec<String>, BlobError> {
        match self {
            Store::Local(s) => s.deleted_keys(prefix, older_than_ms).await,
            Store::S3(s) => s.deleted_keys(prefix, older_than_ms).await,
            Store::Opendal(s) => s.deleted_keys(prefix, older_than_ms).await,
        }
    }
}

/// Whether the store refused a delete because the version is locked. MinIO
/// answers 400 `InvalidRequest` ("Object is WORM protected"), AWS answers 403
/// `AccessDenied` ("...because object protected by object lock"). A refusal
/// that names no lock, such as a key without `s3:DeleteObjectVersion`, is a
/// failure: reporting it as retained would hide that nothing is ever freed.
pub fn is_lock_refusal(error: &BlobError) -> bool {
    let message = match error {
        BlobError::Denied { message } | BlobError::Backend { message, .. } => {
            message.to_ascii_lowercase()
        }
        BlobError::AlreadyExists | BlobError::NotFound => return false,
    };
    ["worm", "objectlocked", "object lock"]
        .iter()
        .any(|m| message.contains(m))
}

fn versioned(op: &Operator) -> bool {
    let cap = op.info().capability();
    cap.list_with_versions && cap.list_with_deleted && cap.delete_with_version
}

pub(super) async fn purge_op(op: &Operator, key: &str) -> PurgeOutcome {
    if let Err(e) = check_key(key) {
        return PurgeOutcome::Failed(e);
    }
    if !versioned(op) {
        return plain_delete(op, key).await;
    }
    let entries = match op.list_with(key).versions(true).deleted(true).await {
        Ok(entries) => entries,
        Err(e) => return PurgeOutcome::Failed(map_err(e)),
    };
    let ours: Vec<_> = entries
        .iter()
        .filter(|e| e.path().trim_start_matches('/') == key)
        .collect();
    if ours.is_empty() {
        return PurgeOutcome::NotFound;
    }

    let (mut purged_versions, mut purged_bytes) = (0u32, 0u64);
    let mut retained = Vec::new();
    let mut markers = Vec::new();
    for entry in ours {
        let meta = entry.metadata();
        let Some(version) = meta.version() else {
            // The backend lists the object without a version id, so it has
            // no versioning enabled for it and a plain delete removes it.
            return plain_delete(op, key).await;
        };
        if meta.is_deleted() {
            markers.push(version.to_owned());
            continue;
        }
        let size = meta.content_length();
        match op.delete_with(key).version(version).await.map_err(map_err) {
            Ok(()) | Err(BlobError::NotFound) => {
                purged_versions += 1;
                purged_bytes += size;
            }
            Err(e) if is_lock_refusal(&e) => retained.push(RetainedVersion {
                version: version.to_owned(),
                size,
                modified_ms: meta.last_modified().map(timestamp_ms),
            }),
            Err(e) => return PurgeOutcome::Failed(e),
        }
    }

    if retained.is_empty() {
        for version in markers {
            match op.delete_with(key).version(&version).await.map_err(map_err) {
                Ok(()) | Err(BlobError::NotFound) => {}
                Err(e) => return PurgeOutcome::Failed(e),
            }
        }
        PurgeOutcome::Purged {
            versions: purged_versions,
            bytes: purged_bytes,
        }
    } else {
        PurgeOutcome::Retained {
            purged_versions,
            purged_bytes,
            retained,
        }
    }
}

async fn plain_delete(op: &Operator, key: &str) -> PurgeOutcome {
    let bytes = match op.stat(key).await.map_err(map_err) {
        Ok(meta) => meta.content_length(),
        Err(BlobError::NotFound) => return PurgeOutcome::NotFound,
        Err(e) => return PurgeOutcome::Failed(e),
    };
    match op.delete(key).await.map_err(map_err) {
        Ok(()) => PurgeOutcome::Purged { versions: 1, bytes },
        Err(BlobError::NotFound) => PurgeOutcome::NotFound,
        Err(e) => PurgeOutcome::Failed(e),
    }
}

// ponytail: materialises every version under the prefix; stream the lister
// if a bucket's version listing stops fitting in memory.
pub(super) async fn deleted_keys_op(
    op: &Operator,
    prefix: &str,
    older_than_ms: i64,
) -> Result<Vec<String>, BlobError> {
    if !versioned(op) {
        return Ok(Vec::new());
    }
    let dir = prefix.rsplit_once('/').map_or("", |(d, _)| d);
    let dir = if dir.is_empty() {
        "/".to_owned()
    } else {
        format!("{dir}/")
    };
    let entries = op
        .list_with(&dir)
        .recursive(true)
        .versions(true)
        .deleted(true)
        .await
        .map_err(map_err)?;
    let mut out: Vec<String> = entries
        .iter()
        .filter(|e| {
            let meta = e.metadata();
            meta.is_deleted()
                && meta.is_current() == Some(true)
                && meta
                    .last_modified()
                    .is_some_and(|t| timestamp_ms(t) <= older_than_ms)
        })
        .map(|e| e.path().trim_start_matches('/').to_owned())
        .filter(|k| k.starts_with(prefix))
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_lock_refusal_classifies() {
        assert!(is_lock_refusal(&BlobError::Denied {
            message: "AccessDenied: Access Denied because object protected by object lock.".into()
        }));
        assert!(is_lock_refusal(&BlobError::backend(
            "Unexpected: InvalidRequest: Object is WORM protected and cannot be overwritten"
        )));
        assert!(!is_lock_refusal(&BlobError::Denied {
            message: "403".into()
        }));
        assert!(!is_lock_refusal(&BlobError::backend(
            "AccessDenied: Access Denied"
        )));
        assert!(!is_lock_refusal(&BlobError::backend("connection reset")));
        assert!(!is_lock_refusal(&BlobError::NotFound));
    }
}
