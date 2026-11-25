use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use opendal::services::AzblobConfig;
use opendal::{Configurator, ErrorKind, Operator};
use reqsign_azure_storage::Credential;
use reqsign_core::time::Timestamp;
use reqsign_core::{Context, ProvideCredential, ProvideCredentialChain};
use zeroize::Zeroizing;

use super::purge::{BlobPurge, PurgeOutcome, deleted_keys_op, purge_op, rewritten_keys_op};
use super::s3::{check_key, get_range_op, layered, list_op, map_err, modified_op, presign_op};
use super::{BlobDelete, BlobError, BlobStore, ListEntry, Presigned};
use crate::error::{Error, Result};

/// An OpenDAL service opened by scheme name at run time: Azure Blob and GCS.
/// A scheme whose service was not compiled in is reported as unavailable
/// instead of failing the build.
pub struct OpendalStore {
    pub(super) op: Operator,
}

impl fmt::Debug for OpendalStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpendalStore").finish_non_exhaustive()
    }
}

/// Where an Azure store re-reads its SAS token; `None` means there is none now.
pub type SasSource = Arc<dyn Fn() -> Option<Zeroizing<String>> + Send + Sync>;

struct SasProvider {
    source: SasSource,
    refresh: Duration,
}

impl fmt::Debug for SasProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SasProvider").finish_non_exhaustive()
    }
}

impl ProvideCredential for SasProvider {
    type Credential = Credential;

    async fn provide_credential(&self, _: &Context) -> reqsign_core::Result<Option<Credential>> {
        Ok((self.source)().map(|token| {
            Credential::with_sas_token_expires_at(&token, Timestamp::now() + self.refresh)
        }))
    }
}

impl OpendalStore {
    /// An Azure Blob store that signs with whatever `source` returns, asked
    /// again once `refresh` has passed. The provider chain is replaced whole,
    /// so no credential of the host (environment, IMDS, az cli) is ever used.
    pub fn open_azblob_refreshing(
        options: &BTreeMap<String, String>,
        source: SasSource,
        refresh: Duration,
    ) -> Result<Self> {
        if refresh.is_zero() {
            return Err(Error::InvalidInput("refresh must not be zero".into()));
        }
        let options = options
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "sas_token" | "account_key"))
            .map(|(k, v)| (k.clone(), v.clone()));
        let invalid = |e: opendal::Error| {
            Error::InvalidInput(format!(
                "cannot configure the azblob backend ({})",
                e.kind()
            ))
        };
        let builder = AzblobConfig::from_iter(options)
            .map_err(invalid)?
            .into_builder()
            .credential_provider_chain(
                ProvideCredentialChain::new().push(SasProvider { source, refresh }),
            );
        opendal::install_default();
        let op = Operator::new(builder).map_err(invalid)?;
        Ok(OpendalStore { op: layered(op) })
    }

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
        Ok(OpendalStore { op: layered(op) })
    }

    pub fn conditional_write_supported(&self) -> bool {
        self.op.info().capability().write_with_if_not_exists
    }
}

impl OpendalStore {
    pub(super) async fn modified_ms(
        &self,
        key: &str,
    ) -> std::result::Result<Option<i64>, BlobError> {
        modified_op(&self.op, key).await
    }

    pub async fn presign_read(
        &self,
        key: &str,
        ttl: Duration,
    ) -> std::result::Result<Option<Presigned>, BlobError> {
        presign_op(&self.op, key, ttl).await
    }

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
        match self.op.write_with(key, data).if_not_exists(true).await {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == ErrorKind::ConditionNotMatch => Err(BlobError::AlreadyExists),
            Err(e) => Err(map_err(e)),
        }
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
        get_range_op(&self.op, key, range).await
    }

    async fn list(&self, prefix: &str) -> std::result::Result<Vec<ListEntry>, BlobError> {
        list_op(&self.op, prefix).await
    }

    fn publishes_atomically(&self) -> bool {
        true
    }
}

impl BlobDelete for OpendalStore {
    async fn delete(&self, key: &str) -> std::result::Result<(), BlobError> {
        check_key(key)?;
        self.op.delete(key).await.map_err(map_err)
    }
}

impl BlobPurge for OpendalStore {
    async fn purge(&self, key: &str) -> PurgeOutcome {
        purge_op(&self.op, key).await
    }

    async fn deleted_keys(
        &self,
        prefix: &str,
        older_than_ms: i64,
    ) -> std::result::Result<Vec<String>, BlobError> {
        deleted_keys_op(&self.op, prefix, older_than_ms).await
    }

    async fn rewritten_keys(&self, prefix: &str) -> std::result::Result<Vec<String>, BlobError> {
        rewritten_keys_op(&self.op, prefix).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn refreshing_provider_caches_for_the_refresh_window() {
        let provider = SasProvider {
            source: Arc::new(|| Some(Zeroizing::new("sig=abc".to_owned()))),
            refresh: Duration::from_secs(300),
        };
        let before = Timestamp::now();
        let Some(Credential::SasToken { expires_at, .. }) =
            provider.provide_credential(&Context::new()).await.unwrap()
        else {
            panic!("expected a SAS credential");
        };
        let at = expires_at.unwrap();
        assert!(at >= before + Duration::from_secs(299));
        assert!(at <= Timestamp::now() + Duration::from_secs(300));
        assert!(!format!("{provider:?}").contains("sig"));
    }

    #[test]
    fn refreshing_store_rejects_a_zero_window() {
        let source: SasSource = Arc::new(|| None);
        let err = OpendalStore::open_azblob_refreshing(&BTreeMap::new(), source, Duration::ZERO);
        assert!(matches!(err, Err(Error::InvalidInput(_))));
    }
}
