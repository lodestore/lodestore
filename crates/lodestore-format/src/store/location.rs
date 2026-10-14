use std::collections::BTreeMap;
use std::path::PathBuf;

use super::s3::{DEFAULT_REGION, S3Options, S3Store, StaticCredentials};
use super::{LocalFsStore, OpendalStore, Store};
use crate::error::{Error, Result};

const EXPERIMENTAL: [&str; 4] = ["azblob", "gcs", "sftp", "webdav"];

/// Where a repository lives: a backend name plus that backend's options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoLocation {
    pub backend: String,
    pub options: BTreeMap<String, String>,
}

impl RepoLocation {
    pub fn new<'a>(backend: &str, options: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        RepoLocation {
            backend: backend.to_owned(),
            options: options
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        }
    }

    pub fn is_experimental(&self) -> bool {
        EXPERIMENTAL.contains(&self.backend.as_str())
    }

    fn opt(&self, key: &str) -> Option<&str> {
        self.options
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    fn require(&self, key: &str) -> Result<&str> {
        self.opt(key).ok_or_else(|| {
            Error::InvalidInput(format!(
                "{} location needs the `{key}` option",
                self.backend
            ))
        })
    }

    pub fn validate(&self) -> Result<()> {
        match self.backend.as_str() {
            "s3" => {
                self.require("bucket")?;
                if let Some(v) = self.opt("path_style")
                    && v != "true"
                    && v != "false"
                {
                    return Err(Error::InvalidInput(
                        "`path_style` must be `true` or `false`".into(),
                    ));
                }
            }
            "local" => {
                if !PathBuf::from(self.require("path")?).is_absolute() {
                    return Err(Error::InvalidInput(
                        "the local repository path must be absolute".into(),
                    ));
                }
            }
            "azblob" => {
                self.require("container")?;
            }
            "gcs" => {
                self.require("bucket")?;
            }
            "sftp" | "webdav" => {
                self.require("endpoint")?;
            }
            other => {
                return Err(Error::InvalidInput(format!("unknown backend {other:?}")));
            }
        }
        Ok(())
    }

    pub fn display(&self) -> String {
        let get = |k: &str| self.opt(k).unwrap_or("");
        match self.backend.as_str() {
            "s3" => {
                let prefix = get("prefix").trim_matches('/');
                if prefix.is_empty() {
                    format!("s3://{}", get("bucket"))
                } else {
                    format!("s3://{}/{prefix}", get("bucket"))
                }
            }
            "local" => get("path").to_owned(),
            "azblob" => format!("azblob://{}", get("container")),
            "gcs" => format!("gcs://{}", get("bucket")),
            other => format!("{other}://{}", get("endpoint")),
        }
    }

    fn s3_options(&self) -> S3Options {
        let endpoint = self.opt("endpoint").map(str::to_owned);
        let virtual_host_style = match self.opt("path_style") {
            Some(v) => v == "false",
            None => endpoint.is_none(),
        };
        S3Options {
            bucket: self.opt("bucket").unwrap_or_default().to_owned(),
            prefix: self.opt("prefix").unwrap_or_default().to_owned(),
            region: self.opt("region").unwrap_or(DEFAULT_REGION).to_owned(),
            virtual_host_style,
            endpoint,
        }
    }

    pub fn open(&self, creds: Option<StaticCredentials>) -> Result<Store> {
        self.validate()?;
        match self.backend.as_str() {
            "s3" => Ok(Store::S3(S3Store::with_options(&self.s3_options(), creds)?)),
            "local" => Ok(Store::Local(LocalFsStore::new(PathBuf::from(
                self.require("path")?,
            )))),
            other => Ok(Store::Opendal(OpendalStore::open(other, &self.options)?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_aws_defaults_to_virtual_host() {
        let loc = RepoLocation::new("s3", [("bucket", "b"), ("region", "eu-west-1")]);
        loc.validate().unwrap();
        let o = loc.s3_options();
        assert_eq!(o.endpoint, None);
        assert!(o.virtual_host_style);
        assert_eq!(o.region, "eu-west-1");
        assert_eq!(loc.display(), "s3://b");
    }

    #[test]
    fn location_default_region_and_endpoint_path_style() {
        let loc = RepoLocation::new(
            "s3",
            [
                ("bucket", "b"),
                ("endpoint", "http://127.0.0.1:9000"),
                ("prefix", "p/q"),
            ],
        );
        let o = loc.s3_options();
        assert!(!o.virtual_host_style);
        assert_eq!(o.region, "us-east-1");
        assert_eq!(loc.display(), "s3://b/p/q");

        let loc = RepoLocation::new(
            "s3",
            [
                ("bucket", "b"),
                ("endpoint", "https://x"),
                ("path_style", "false"),
            ],
        );
        assert!(loc.s3_options().virtual_host_style);
    }

    #[test]
    fn location_aws_store_opens() {
        let loc = RepoLocation::new("s3", [("bucket", "b"), ("region", "eu-west-1")]);
        assert!(loc.open(None).is_ok());
    }

    #[test]
    fn location_invalid_inputs_rejected() {
        let bad = [
            RepoLocation::new("s3", []),
            RepoLocation::new("s3", [("bucket", "b"), ("path_style", "yes")]),
            RepoLocation::new("local", [("path", "relative/dir")]),
            RepoLocation::new("local", []),
            RepoLocation::new("nosuch", [("bucket", "b")]),
            RepoLocation::new("azblob", []),
            RepoLocation::new("sftp", []),
        ];
        for loc in bad {
            assert!(
                matches!(loc.validate(), Err(Error::InvalidInput(_))),
                "{loc:?}"
            );
            assert!(loc.open(None).is_err());
        }
    }

    #[test]
    fn location_experimental_flag() {
        for b in ["azblob", "gcs", "sftp", "webdav"] {
            assert!(RepoLocation::new(b, []).is_experimental());
        }
        assert!(!RepoLocation::new("s3", []).is_experimental());
        assert!(!RepoLocation::new("local", []).is_experimental());
    }

    #[test]
    fn location_local_opens() {
        let loc = RepoLocation::new("local", [("path", "/tmp/lode-repo")]);
        assert!(matches!(loc.open(None), Ok(Store::Local(_))));
        assert_eq!(loc.display(), "/tmp/lode-repo");
    }
}
