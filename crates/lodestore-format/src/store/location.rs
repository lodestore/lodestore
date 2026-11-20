use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use super::s3::{DEFAULT_REGION, S3Options, S3Provider, S3Store, StaticCredentials};
use super::sftp::Publish;
use super::{LocalFsStore, OpendalStore, SftpStore, Store};
use crate::error::{Error, Result};

fn with_root(base: String, root: &str) -> String {
    let root = root.trim_matches('/');
    if root.is_empty() {
        base
    } else {
        format!("{base}/{root}")
    }
}

/// Compares the host, not a substring: `backblazeb2.com.evil.example` is not B2.
fn is_b2_endpoint(endpoint: &str) -> bool {
    let rest = endpoint.split_once("://").map_or(endpoint, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = authority.split(':').next().unwrap_or(authority);
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "backblazeb2.com" || host.ends_with(".backblazeb2.com")
}

/// Where a repository lives: a backend name plus that backend's options.
#[derive(Clone, PartialEq, Eq)]
pub struct RepoLocation {
    pub backend: String,
    pub options: BTreeMap<String, String>,
}

fn is_secret_option(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "key",
        "secret",
        "token",
        "password",
        "passphrase",
        "credential",
    ]
    .iter()
    .any(|m| key.contains(m))
}

impl fmt::Debug for RepoLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let options: BTreeMap<&str, &str> = self
            .options
            .iter()
            .map(|(k, v)| {
                let shown = if is_secret_option(k) { "<redacted>" } else { v };
                (k.as_str(), shown)
            })
            .collect();
        f.debug_struct("RepoLocation")
            .field("backend", &self.backend)
            .field("options", &options)
            .finish()
    }
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

    /// The location an agent opens. A GCS store never takes credentials from the
    /// agent host's environment, well-known files or VM metadata, and never
    /// reads `credential_path` from the host's disk: it signs with what the
    /// server sent, or not at all. Other backends are unchanged.
    pub fn for_agent(&self) -> RepoLocation {
        let mut agent = self.clone();
        if agent.backend == "gcs" {
            agent.options.remove("credential_path");
            for flag in ["disable_vm_metadata", "disable_config_load"] {
                agent.options.insert(flag.to_owned(), "true".to_owned());
            }
        }
        agent
    }

    /// Why an agent could not authenticate to this store with the options it
    /// would receive from [`Self::for_agent`], or `None`.
    pub fn agent_credentials_problem(&self) -> Option<&'static str> {
        if self.backend != "gcs" {
            return None;
        }
        let anonymous = |k: &str| self.opt(k) == Some("true");
        let signs = self.opt("credential").is_some()
            || self.opt("token").is_some()
            || anonymous("skip_signature")
            || anonymous("allow_anonymous");
        if signs {
            None
        } else if self.options.contains_key("credential_path") {
            Some(
                "agents cannot read credential_path; put the service account key into the credential option",
            )
        } else {
            Some(
                "agents cannot use the credentials of their host; put the service account key into the credential option",
            )
        }
    }

    fn require_static_azure_credentials(&self) -> Result<()> {
        if self.opt("sas_token").is_some()
            || (self.opt("account_key").is_some() && self.opt("account_name").is_some())
        {
            Ok(())
        } else {
            Err(Error::InvalidInput(
                "azblob needs static credentials: sas_token, or account_key together with account_name"
                    .into(),
            ))
        }
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
                if self.s3_provider()? == S3Provider::Default
                    && self.opt("endpoint").is_some_and(is_b2_endpoint)
                {
                    return Err(Error::InvalidInput(
                        "Backblaze B2 needs the option provider = b2: B2 does not apply If-None-Match, and a delete there hides the file instead of removing it".into(),
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
            "sftp" => {
                for option in ["endpoint", "user", "key"] {
                    self.require(option)?;
                }
                if self.opt("host_key").is_none() {
                    return Err(Error::InvalidInput(
                        "Enter the SFTP server's host key: one line of `ssh-keyscan -p <port> <host>` output"
                            .into(),
                    ));
                }
                Publish::parse(self.opt("publish"))?;
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
            "azblob" => with_root(format!("azblob://{}", get("container")), get("root")),
            "gcs" => with_root(format!("gcs://{}", get("bucket")), get("root")),
            "sftp" => with_root(
                format!("sftp://{}@{}", get("user"), get("endpoint")),
                get("root"),
            ),
            other => format!("{other}://{}", get("endpoint")),
        }
    }

    fn s3_provider(&self) -> Result<S3Provider> {
        match self.opt("provider") {
            None => Ok(S3Provider::Default),
            Some("b2") => Ok(S3Provider::B2),
            Some(other) => Err(Error::InvalidInput(format!(
                "unknown S3 provider {other:?}; the only value is `b2`"
            ))),
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
            provider: self.s3_provider().unwrap_or(S3Provider::Default),
        }
    }

    pub fn open(&self, creds: Option<StaticCredentials>) -> Result<Store> {
        self.validate()?;
        match self.backend.as_str() {
            "s3" => Ok(Store::S3(S3Store::with_options(&self.s3_options(), creds)?)),
            "local" => Ok(Store::Local(LocalFsStore::new(PathBuf::from(
                self.require("path")?,
            )))),
            "sftp" => Ok(Store::Sftp(SftpStore::open(&self.options)?)),
            "azblob" => {
                self.require_static_azure_credentials()?;
                Ok(Store::Opendal(OpendalStore::open("azblob", &self.options)?))
            }
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
            RepoLocation::new("sftp", [("endpoint", "h"), ("user", "u"), ("key", "k")]),
            RepoLocation::new(
                "sftp",
                [
                    ("endpoint", "h"),
                    ("user", "u"),
                    ("key", "k"),
                    ("host_key", "h"),
                    ("publish", "rename"),
                ],
            ),
            RepoLocation::new("webdav", [("endpoint", "h")]),
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
    fn location_backblaze_endpoint_needs_the_b2_provider() {
        let s3 = |endpoint: &str, provider: Option<&str>| {
            let mut o = vec![("bucket", "b"), ("endpoint", endpoint)];
            if let Some(p) = provider {
                o.push(("provider", p));
            }
            RepoLocation::new("s3", o)
        };
        let err = s3("https://s3.us-west-004.backblazeb2.com", None)
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("provider = b2"), "{err}");
        assert!(
            s3("https://BackblazeB2.com:443/x", None)
                .validate()
                .is_err()
        );
        s3("https://s3.us-west-004.backblazeb2.com", Some("b2"))
            .validate()
            .unwrap();
        s3("https://backblazeb2.com.evil.example", None)
            .validate()
            .unwrap();
        s3("https://notbackblazeb2.com", None).validate().unwrap();
        assert!(s3("http://127.0.0.1:9000", Some("gcs")).validate().is_err());
        let b2 = s3("http://127.0.0.1:9000", Some("b2"));
        assert_eq!(b2.s3_options().provider, S3Provider::B2);
        assert_eq!(
            s3("http://127.0.0.1:9000", None).s3_options().provider,
            S3Provider::Default
        );
    }

    #[test]
    fn location_display_keeps_the_root() {
        let loc = RepoLocation::new("azblob", [("container", "c"), ("root", "r/x")]);
        assert_eq!(loc.display(), "azblob://c/r/x");
        let loc = RepoLocation::new("gcs", [("bucket", "b"), ("root", "/r/")]);
        assert_eq!(loc.display(), "gcs://b/r");
        assert_eq!(
            RepoLocation::new("azblob", [("container", "c")]).display(),
            "azblob://c"
        );
        assert_eq!(
            RepoLocation::new("gcs", [("bucket", "b")]).display(),
            "gcs://b"
        );
    }

    #[test]
    fn location_sftp_validates_and_displays() {
        let loc = RepoLocation::new(
            "sftp",
            [
                ("endpoint", "backup.example:2222"),
                ("user", "lode"),
                ("key", "k"),
                ("host_key", "ssh-ed25519 AAAA"),
                ("root", "/srv/repo/"),
            ],
        );
        loc.validate().unwrap();
        assert_eq!(loc.display(), "sftp://lode@backup.example:2222/srv/repo");
        let direct = RepoLocation::new(
            "sftp",
            [
                ("endpoint", "h"),
                ("user", "u"),
                ("key", "k"),
                ("host_key", "h"),
                ("publish", "direct"),
            ],
        );
        direct.validate().unwrap();
        assert_eq!(direct.display(), "sftp://u@h");
        let unpinned = RepoLocation::new("sftp", [("endpoint", "h"), ("user", "u"), ("key", "k")]);
        let err = unpinned.validate().unwrap_err();
        assert!(err.to_string().contains("ssh-keyscan"), "{err}");
    }

    #[test]
    fn for_agent_closes_gcs_to_ambient_credentials() {
        let gcs = RepoLocation::new(
            "gcs",
            [
                ("bucket", "b"),
                ("credential_path", "/etc/sa.json"),
                ("credential", "c2E="),
                ("skip_signature", "true"),
            ],
        );
        let agent = gcs.for_agent();
        assert!(!agent.options.contains_key("credential_path"));
        assert_eq!(agent.opt("disable_vm_metadata"), Some("true"));
        assert_eq!(agent.opt("disable_config_load"), Some("true"));
        assert_eq!(agent.opt("credential"), Some("c2E="));
        assert_eq!(agent.opt("skip_signature"), Some("true"));
        assert_eq!(gcs.agent_credentials_problem(), None);
        assert_eq!(gcs.opt("credential_path"), Some("/etc/sa.json"));

        let path_only = RepoLocation::new("gcs", [("bucket", "b"), ("credential_path", "/x")]);
        let problem = path_only.agent_credentials_problem().unwrap();
        assert!(problem.contains("credential_path"), "{problem}");
        let none = RepoLocation::new("gcs", [("bucket", "b")]);
        assert!(none.agent_credentials_problem().is_some());
    }

    #[test]
    fn for_agent_leaves_other_backends_alone() {
        for loc in [
            RepoLocation::new("s3", [("bucket", "b")]),
            RepoLocation::new("local", [("path", "/p")]),
            RepoLocation::new("azblob", [("container", "c"), ("account_key", "k")]),
            RepoLocation::new("sftp", [("endpoint", "h"), ("host_key", "k")]),
        ] {
            assert_eq!(loc.for_agent(), loc);
            assert_eq!(loc.agent_credentials_problem(), None);
        }
    }

    #[test]
    fn azblob_open_needs_static_credentials() {
        let azblob = |extra: &[(&'static str, &'static str)]| {
            let mut options = vec![("container", "c"), ("endpoint", "http://127.0.0.1:1")];
            options.extend_from_slice(extra);
            RepoLocation::new("azblob", options)
        };
        for loc in [
            azblob(&[]),
            azblob(&[("account_name", "a")]),
            azblob(&[("account_key", "a2V5")]),
        ] {
            let err = loc.open(None).err().unwrap();
            assert!(err.to_string().contains("static credentials"), "{err}");
        }
        azblob(&[("account_name", "a"), ("account_key", "a2V5")])
            .open(None)
            .unwrap();
        azblob(&[("sas_token", "sv=1&sig=x")]).open(None).unwrap();
        azblob(&[]).validate().unwrap();
    }

    #[tokio::test]
    async fn azblob_transport_error_does_not_leak_sas_token() {
        use crate::store::BlobStore;
        let loc = RepoLocation::new(
            "azblob",
            [
                ("container", "c"),
                ("endpoint", "http://127.0.0.1:1"),
                ("sas_token", "sv=1&sig=SECRETSIG"),
            ],
        );
        let store = loc.open(None).unwrap();
        let err = store
            .put_if_absent("k", bytes::Bytes::from_static(b"x"))
            .await
            .unwrap_err();
        for text in [err.to_string(), format!("{err:?}")] {
            assert!(!text.contains("SECRETSIG"), "{text}");
        }
    }

    #[test]
    fn debug_redacts_secret_options() {
        let loc = RepoLocation::new(
            "sftp",
            [
                ("host", "h.example"),
                ("private_key", "-----BEGIN KEY-----"),
                ("account_key", "KEYVAL"),
                ("sas_token", "sv=1&sig=SECRETSIG"),
            ],
        );
        let shown = format!("{loc:?}");
        for secret in ["BEGIN KEY", "KEYVAL", "SECRETSIG"] {
            assert!(!shown.contains(secret), "{shown}");
        }
        assert!(
            shown.contains("h.example") && shown.contains("private_key"),
            "{shown}"
        );
    }

    #[test]
    fn location_local_opens() {
        let loc = RepoLocation::new("local", [("path", "/tmp/lode-repo")]);
        assert!(matches!(loc.open(None), Ok(Store::Local(_))));
        assert_eq!(loc.display(), "/tmp/lode-repo");
    }
}
