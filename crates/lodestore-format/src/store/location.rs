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

/// Whether an option holds a secret (a key, a token or a signature).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Plain,
    Secret,
}
use Kind::{Plain, Secret};

/// Every option a new location of each backend may carry, matched exactly.
/// S3 keys never travel in the options.
const OPTIONS: &[(&str, &[(&str, Kind)])] = &[
    (
        "s3",
        &[
            ("bucket", Plain),
            ("endpoint", Plain),
            ("region", Plain),
            ("path_style", Plain),
            ("prefix", Plain),
            ("provider", Plain),
        ],
    ),
    ("local", &[("path", Plain)]),
    (
        "azblob",
        &[
            ("container", Plain),
            ("root", Plain),
            ("endpoint", Plain),
            ("account_name", Plain),
            ("account_key", Secret),
            ("sas_token", Secret),
            ("encryption_key", Secret),
            ("encryption_key_sha256", Plain),
            ("encryption_algorithm", Plain),
        ],
    ),
    (
        "gcs",
        &[
            ("bucket", Plain),
            ("root", Plain),
            ("endpoint", Plain),
            ("credential", Secret),
            ("token", Secret),
            ("credential_path", Plain),
            ("skip_signature", Plain),
            ("allow_anonymous", Plain),
            ("disable_vm_metadata", Plain),
            ("disable_config_load", Plain),
        ],
    ),
    (
        "sftp",
        &[
            ("endpoint", Plain),
            ("user", Plain),
            ("key", Secret),
            ("host_key", Plain),
            ("root", Plain),
            ("publish", Plain),
        ],
    ),
];

fn backend_options(backend: &str) -> &'static [(&'static str, Kind)] {
    OPTIONS
        .iter()
        .find(|(b, _)| *b == backend)
        .map_or(&[], |(_, o)| o)
}

/// Whether a store of `backend` may read `key`; checked in debug builds on
/// every read so the table cannot fall behind the stores.
pub(super) fn is_known_option(backend: &str, key: &str) -> bool {
    backend_options(backend).iter().any(|(k, _)| *k == key)
}

/// The options that hold a secret, by backend: they are sealed on their own.
pub fn secret_option_names(backend: &str) -> Vec<&'static str> {
    backend_options(backend)
        .iter()
        .filter(|(_, kind)| *kind == Secret)
        .map(|(k, _)| *k)
        .collect()
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
        debug_assert!(
            backend_options(&self.backend).is_empty() || is_known_option(&self.backend, key),
            "{} reads `{key}`, which is not in its option table",
            self.backend
        );
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
        self.validate_inner(true)
    }

    fn validate_inner(&self, need_host_key: bool) -> Result<()> {
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
                if need_host_key && self.opt("host_key").is_none() {
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

    /// [`Self::validate`] for a location being added: every option must be one
    /// Lodestore supports for the backend, spelled exactly. A stored location
    /// is opened without this check, so an older one keeps working.
    pub fn validate_new(&self) -> Result<()> {
        self.validate_inner(true)?;
        self.check_option_names()
    }

    /// For the first contact of a TOFU check: everything [`Self::validate_new`]
    /// checks except the host key.
    pub fn validate_new_without_host_key(&self) -> Result<()> {
        self.validate_inner(false)?;
        self.check_option_names()
    }

    fn check_option_names(&self) -> Result<()> {
        if let Some(unknown) = self
            .options
            .keys()
            .find(|k| !is_known_option(&self.backend, k))
        {
            let known: Vec<&str> = backend_options(&self.backend)
                .iter()
                .map(|(k, _)| *k)
                .collect();
            return Err(Error::InvalidInput(format!(
                "{} location does not support the option `{unknown}`; supported options: {}",
                self.backend,
                known.join(", ")
            )));
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
    use proptest::prelude::*;

    const KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";

    fn outcome(r: Result<()>) -> std::result::Result<(), String> {
        r.map_err(|e| e.to_string())
    }

    #[test]
    fn validate_new_without_host_key_accepts_only_the_missing_key() {
        let draft = |extra: &[(&'static str, &'static str)]| {
            let mut options = vec![("endpoint", "h:22"), ("user", "u"), ("key", "k")];
            options.extend_from_slice(extra);
            RepoLocation::new("sftp", options)
        };
        draft(&[]).validate_new_without_host_key().unwrap();
        assert!(draft(&[]).validate_new().is_err());
        for bad in [
            RepoLocation::new("sftp", [("user", "u"), ("key", "k")]),
            RepoLocation::new("sftp", [("endpoint", "h"), ("key", "k")]),
            RepoLocation::new("sftp", [("endpoint", "h"), ("user", "u")]),
            draft(&[("publish", "rename")]),
            draft(&[("foo", "x")]),
        ] {
            let with_key = {
                let mut loc = bad.clone();
                loc.options.insert("host_key".into(), KEY.into());
                loc
            };
            assert_eq!(
                outcome(bad.validate_new_without_host_key()),
                outcome(with_key.validate_new())
            );
            assert!(bad.validate_new_without_host_key().is_err());
        }
    }

    proptest! {
        #[test]
        fn validate_new_without_host_key_differs_only_in_host_key(
            backend in prop::sample::select(vec!["s3", "azblob", "gcs", "sftp", "local"]),
            picks in prop::collection::vec(
                prop::option::of(prop::sample::select(vec![
                    "", "x", "rename", "hardlink", "direct", "127.0.0.1:22", "/srv", KEY,
                ])),
                16,
            ),
        ) {
            let mut names: Vec<&str> =
                backend_options(backend).iter().map(|(k, _)| *k).collect();
            names.extend(["Host_Key", "endpoint_url", "foo", "publish"]);
            if backend != "sftp" {
                names.push("host_key");
            }
            let mut draft = RepoLocation::new(backend, []);
            for (name, value) in names.iter().zip(&picks) {
                if let Some(value) = value {
                    draft.options.insert((*name).to_owned(), (*value).to_owned());
                }
            }
            let strict = if backend == "sftp" {
                let mut with_key = draft.clone();
                with_key.options.insert("host_key".into(), KEY.into());
                with_key.validate_new()
            } else {
                draft.validate_new()
            };
            prop_assert_eq!(outcome(strict), outcome(draft.validate_new_without_host_key()));
        }
    }

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

    #[test]
    fn secret_options_are_redacted_in_debug() {
        for backend in ["azblob", "gcs", "sftp"] {
            for name in secret_option_names(backend) {
                assert!(is_secret_option(name), "{backend}: {name}");
            }
        }
    }

    #[test]
    fn secret_option_names_come_from_the_table() {
        assert_eq!(
            secret_option_names("azblob"),
            ["account_key", "sas_token", "encryption_key"]
        );
        assert_eq!(secret_option_names("gcs"), ["credential", "token"]);
        assert_eq!(secret_option_names("sftp"), ["key"]);
        for backend in ["s3", "local", "nosuch"] {
            assert!(secret_option_names(backend).is_empty(), "{backend}");
        }
        for (backend, options) in OPTIONS {
            for (name, _) in *options {
                assert_eq!(name.to_ascii_lowercase(), *name, "{backend}");
            }
        }
    }

    #[test]
    fn validate_new_takes_every_supported_option() {
        let full = [
            RepoLocation::new(
                "s3",
                [
                    ("bucket", "b"),
                    ("endpoint", "http://127.0.0.1:9000"),
                    ("region", "r"),
                    ("path_style", "true"),
                    ("prefix", "p"),
                    ("provider", "b2"),
                ],
            ),
            RepoLocation::new("local", [("path", "/srv/repo")]),
            RepoLocation::new(
                "azblob",
                [
                    ("container", "c"),
                    ("root", "r"),
                    ("endpoint", "http://127.0.0.1:1"),
                    ("account_name", "a"),
                    ("account_key", "a2V5"),
                    ("sas_token", "sv=1"),
                    ("encryption_key", "k"),
                    ("encryption_key_sha256", "h"),
                    ("encryption_algorithm", "AES256"),
                ],
            ),
            RepoLocation::new(
                "gcs",
                [
                    ("bucket", "b"),
                    ("root", "r"),
                    ("endpoint", "http://127.0.0.1:1"),
                    ("credential", "c2E="),
                    ("token", "t"),
                    ("credential_path", "/etc/sa.json"),
                    ("skip_signature", "true"),
                    ("allow_anonymous", "true"),
                    ("disable_vm_metadata", "true"),
                    ("disable_config_load", "true"),
                ],
            ),
            RepoLocation::new(
                "sftp",
                [
                    ("endpoint", "h:22"),
                    ("user", "u"),
                    ("key", "k"),
                    ("host_key", "ssh-ed25519 AAAA"),
                    ("root", "/srv"),
                    ("publish", "direct"),
                ],
            ),
        ];
        assert_eq!(full.len(), OPTIONS.len());
        for loc in full {
            loc.validate_new().unwrap();
            assert_eq!(
                loc.options.len(),
                backend_options(&loc.backend).len(),
                "{}",
                loc.backend
            );
        }
    }

    #[test]
    fn validate_new_refuses_aliases_and_other_spellings() {
        let cases = [
            ("azblob", "sas_key"),
            ("azblob", "azure_storage_sas_token"),
            ("azblob", "azure_storage_account_key"),
            ("azblob", "access_key"),
            ("azblob", "master_key"),
            ("azblob", "Account_Key"),
            ("azblob", "SAS_TOKEN"),
            ("gcs", "Token"),
            ("gcs", "google_service_account_key"),
            ("gcs", "service_account"),
            ("gcs", "Credential"),
            ("s3", "secret_access_key"),
            ("s3", "Bucket"),
            ("sftp", "password"),
            ("sftp", "Key"),
            ("local", "Path"),
        ];
        for (backend, name) in cases {
            let required: &[(&str, &str)] = match backend {
                "azblob" => &[("container", "c")],
                "gcs" | "s3" => &[("bucket", "b")],
                "sftp" => &[
                    ("endpoint", "h"),
                    ("user", "u"),
                    ("key", "k"),
                    ("host_key", "h"),
                ],
                _ => &[("path", "/srv/repo")],
            };
            let mut options = required.to_vec();
            options.push((name, "SECRETVALUE"));
            let loc = RepoLocation::new(backend, options);
            loc.validate().unwrap();
            let err = loc.validate_new().unwrap_err().to_string();
            assert!(err.contains(&format!("`{name}`")), "{err}");
            assert!(!err.contains("SECRETVALUE"), "{err}");
        }
    }

    #[tokio::test]
    async fn azblob_transport_error_does_not_leak_sas_token() {
        use crate::store::BlobStore;
        let loc = RepoLocation::new(
            "azblob",
            [
                ("container", "c"),
                ("endpoint", "http://127.0.0.1:1"),
                ("sas_token", "sv=1&spr=https,http&sig=SECRETSIG"),
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

    #[test]
    fn gcs_agent_signs_with_any_one_credential() {
        for (key, value) in [
            ("token", "t"),
            ("skip_signature", "true"),
            ("allow_anonymous", "true"),
            ("credential", "c2E="),
        ] {
            let loc = RepoLocation::new("gcs", [("bucket", "b"), (key, value)]);
            assert_eq!(loc.agent_credentials_problem(), None, "{key}");
        }
    }
}
