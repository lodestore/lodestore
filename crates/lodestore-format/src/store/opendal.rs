use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;

use bytes::Bytes;
use opendal::{ErrorKind, Operator};

use super::s3::{check_key, layered, list_op, map_err};
use super::{BlobDelete, BlobError, BlobStore, ListEntry};
use crate::error::{Error, Result};

/// Any OpenDAL service opened by scheme name at run time. Used for the
/// experimental backends; a scheme whose service was not compiled in is
/// reported as unavailable instead of failing the build.
pub struct OpendalStore {
    op: Operator,
    conditional: bool,
}

impl fmt::Debug for OpendalStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpendalStore").finish_non_exhaustive()
    }
}

impl OpendalStore {
    pub fn open(scheme: &str, options: &BTreeMap<String, String>) -> Result<Self> {
        opendal::install_default();
        let op = Operator::via_iter(scheme, options.clone()).map_err(|e| {
            if e.kind() == ErrorKind::Unsupported {
                Error::InvalidInput(format!("backend {scheme} is not available in this build"))
            } else {
                Error::InvalidInput(format!(
                    "cannot configure the {scheme} backend ({})",
                    e.kind()
                ))
            }
        })?;
        let conditional = op.info().capability().write_with_if_not_exists;
        Ok(OpendalStore {
            op: layered(op),
            conditional,
        })
    }

    pub fn conditional_write_supported(&self) -> bool {
        self.conditional
    }
}

impl OpendalStore {
    pub(super) async fn overwrite_for_probe(
        &self,
        key: &str,
        data: Bytes,
    ) -> std::result::Result<(), BlobError> {
        check_key(key)?;
        self.op.write(key, data).await.map(|_| ()).map_err(map_err)
    }
}

impl BlobStore for OpendalStore {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> std::result::Result<(), BlobError> {
        check_key(key)?;
        if self.conditional {
            return match self.op.write_with(key, data).if_not_exists(true).await {
                Ok(_) => Ok(()),
                Err(e) if e.kind() == ErrorKind::ConditionNotMatch => Err(BlobError::AlreadyExists),
                Err(e) => Err(map_err(e)),
            };
        }
        // ponytail: stat then write is racy; without a conditional write on the
        // backend (webdav) two writers can both pass the stat.
        match self.op.stat(key).await {
            Ok(_) => return Err(BlobError::AlreadyExists),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(map_err(e)),
        }
        self.op.write(key, data).await.map(|_| ()).map_err(map_err)
    }

    async fn get(&self, key: &str) -> std::result::Result<Bytes, BlobError> {
        check_key(key)?;
        Ok(self.op.read(key).await.map_err(map_err)?.to_bytes())
    }

    async fn get_range(
        &self,
        key: &str,
        range: Range<u64>,
    ) -> std::result::Result<Bytes, BlobError> {
        check_key(key)?;
        if range.is_empty() {
            return Ok(Bytes::new());
        }
        let buf = self.op.read_with(key).range(range).await.map_err(map_err)?;
        Ok(buf.to_bytes())
    }

    async fn list(&self, prefix: &str) -> std::result::Result<Vec<ListEntry>, BlobError> {
        list_op(&self.op, prefix).await
    }
}

impl BlobDelete for OpendalStore {
    async fn delete(&self, key: &str) -> std::result::Result<(), BlobError> {
        check_key(key)?;
        self.op.delete(key).await.map_err(map_err)
    }
}
