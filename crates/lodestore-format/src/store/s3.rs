use std::fmt;
use std::ops::Range;
use std::time::Duration;

use bytes::Bytes;
use opendal::layers::{RetryLayer, TimeoutLayer};
use opendal::{ErrorKind, Operator, services};
use zeroize::{Zeroize, ZeroizeOnDrop};

use super::{BlobDelete, BlobError, BlobStore, ListEntry};
use crate::error::{Error, Result};

const DEFAULT_REGION: &str = "us-east-1";
const RETRY_ATTEMPTS: usize = 6;

/// A parsed `s3:http(s)://host[:port]/bucket[/prefix]` location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Url {
    pub endpoint: String,
    pub bucket: String,
    pub prefix: String,
}

impl S3Url {
    pub fn parse(url: &str) -> Result<Self> {
        let bad = |why: &str| Error::InvalidInput(format!("invalid s3 repository location: {why}"));
        let rest = url
            .strip_prefix("s3:")
            .ok_or_else(|| bad("it must start with s3:"))?;
        let (scheme, rest) = if let Some(r) = rest.strip_prefix("https://") {
            ("https", r)
        } else if let Some(r) = rest.strip_prefix("http://") {
            ("http", r)
        } else {
            return Err(bad(
                "expected s3:http://host/bucket or s3:https://host/bucket",
            ));
        };
        if rest.contains(['?', '#', '@']) {
            return Err(bad(
                "credentials, query and fragment are not allowed; use the AWS credential chain",
            ));
        }
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        if host.is_empty() {
            return Err(bad("the host is empty"));
        }
        let (bucket, prefix) = path.split_once('/').unwrap_or((path, ""));
        if bucket.is_empty() {
            return Err(bad("the bucket is missing"));
        }
        let prefix = prefix.trim_matches('/');
        if prefix.split('/').any(|c| c == "." || c == "..") {
            return Err(bad("the prefix must not contain . or .. components"));
        }
        Ok(S3Url {
            endpoint: format!("{scheme}://{host}"),
            bucket: bucket.to_owned(),
            prefix: prefix.to_owned(),
        })
    }

    fn host(&self) -> &str {
        let authority = self.endpoint.split_once("://").map_or("", |(_, h)| h);
        authority.split(':').next().unwrap_or(authority)
    }
}

/// Explicit credentials for callers that must not touch the process
/// environment. Without them the standard AWS chain applies.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct StaticCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl fmt::Debug for StaticCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StaticCredentials").finish_non_exhaustive()
    }
}

/// Repository in an S3 bucket, path-style addressing. Every write carries
/// `If-None-Match: *`; there is no unconditional write and no delete.
pub struct S3Store {
    op: Operator,
}

impl fmt::Debug for S3Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Store").finish_non_exhaustive()
    }
}

fn region_from_env() -> String {
    ["AWS_REGION", "AWS_DEFAULT_REGION"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| DEFAULT_REGION.to_owned())
}

fn map_err(e: opendal::Error) -> BlobError {
    match e.kind() {
        ErrorKind::NotFound => BlobError::NotFound,
        _ => BlobError::Backend {
            message: e.to_string(),
            temporary: e.is_temporary(),
        },
    }
}

fn check_key(key: &str) -> std::result::Result<(), BlobError> {
    let bad = key.is_empty()
        || key.contains('\0')
        || key
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..");
    if bad {
        return Err(BlobError::backend(format!("invalid object key {key:?}")));
    }
    Ok(())
}

impl S3Store {
    pub fn open(url: &str, creds: Option<StaticCredentials>) -> Result<Self> {
        let url = S3Url::parse(url)?;
        // Idempotent; installs the reqwest transport that is not auto-registered.
        opendal::install_default();
        let mut builder = services::S3::default()
            .endpoint(&url.endpoint)
            .bucket(&url.bucket)
            .root(&format!("/{}", url.prefix))
            .region(&region_from_env());
        if !url.host().ends_with("amazonaws.com")
            && std::env::var_os("AWS_EC2_METADATA_DISABLED").is_none()
        {
            builder = builder.disable_ec2_metadata();
        }
        if let Some(c) = &creds {
            builder = builder
                .access_key_id(&c.access_key_id)
                .secret_access_key(&c.secret_access_key);
        }
        // The timeout sits inside the retry, so a timed-out attempt is retried.
        // Bodies are handed over as whole `Bytes`, which keeps a PUT replayable.
        let op = Operator::new(builder)
            .map_err(|e| Error::InvalidInput(format!("cannot configure the s3 backend: {e}")))?
            .layer(
                TimeoutLayer::new()
                    .with_timeout(Duration::from_secs(60))
                    .with_io_timeout(Duration::from_secs(120)),
            )
            .layer(
                RetryLayer::new()
                    .with_jitter()
                    .with_min_delay(Duration::from_millis(200))
                    .with_max_delay(Duration::from_secs(10))
                    .with_max_times(RETRY_ATTEMPTS),
            );
        Ok(S3Store { op })
    }
}

impl BlobStore for S3Store {
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
        let buf = self.op.read(key).await.map_err(map_err)?;
        Ok(buf.to_bytes())
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
        // S3 lists by directory-like prefix; the rest of `prefix` is a plain
        // string filter, which matches what the local store does.
        let dir = prefix.rsplit_once('/').map_or("", |(d, _)| d);
        let dir = if dir.is_empty() {
            "/".to_owned()
        } else {
            format!("{dir}/")
        };
        let entries = self
            .op
            .list_with(&dir)
            .recursive(true)
            .await
            .map_err(map_err)?;
        let mut out = Vec::new();
        for e in entries {
            let meta = e.metadata();
            if !meta.is_file() {
                continue;
            }
            let key = e.path().trim_start_matches('/');
            if key.starts_with(prefix) {
                out.push(ListEntry {
                    key: key.to_owned(),
                    size: meta.content_length(),
                });
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }
}

impl BlobDelete for S3Store {
    async fn delete(&self, key: &str) -> std::result::Result<(), BlobError> {
        check_key(key)?;
        self.op.delete(key).await.map_err(map_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_s3_url_parsing() {
        let u = S3Url::parse("s3:http://127.0.0.1:9000/bkt/a/b/").unwrap();
        assert_eq!(u.endpoint, "http://127.0.0.1:9000");
        assert_eq!(u.bucket, "bkt");
        assert_eq!(u.prefix, "a/b");
        assert_eq!(u.host(), "127.0.0.1");

        let u = S3Url::parse("s3:https://s3.eu-west-1.amazonaws.com/bkt").unwrap();
        assert_eq!(u.endpoint, "https://s3.eu-west-1.amazonaws.com");
        assert_eq!(u.prefix, "");
        assert!(u.host().ends_with("amazonaws.com"));

        for bad in [
            "s3:http://host",
            "s3:http://host/",
            "s3:///bkt",
            "s3:host/bkt",
            "s3:ftp://host/bkt",
            "s3:http://key:secret@host/bkt",
            "s3:http://host/bkt?x=1",
            "s3:http://host/bkt/../x",
            "http://host/bkt",
        ] {
            assert!(
                matches!(S3Url::parse(bad), Err(Error::InvalidInput(_))),
                "{bad}"
            );
        }
    }
}
