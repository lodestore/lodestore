use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::io::SeekFrom;
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use russh::client::{self, Handle};
use russh::keys::{
    Algorithm, EcdsaCurve, HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey,
    PublicKeyOrCertificate,
};
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::fs::Metadata;
use russh_sftp::client::{Config as SftpConfig, RawSftpSession, SftpSession};
use russh_sftp::extensions::{FSYNC, HARDLINK};
use russh_sftp::protocol::{FileAttributes, FileType, OpenFlags, StatusCode};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use super::purge::{BlobPurge, PurgeOutcome};
use super::s3::check_key;
use super::{BlobDelete, BlobError, BlobStore, ListEntry};
use crate::error::{Error, Result};

const DEFAULT_PORT: u16 = 22;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const KEEPALIVE: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT_SECS: u64 = 120;
const FILE_MODE: u32 = 0o444;
const DIR_MODE: u32 = 0o2775;
const MAX_LIST_DEPTH: usize = 6;
const TMP_DIR: &str = "tmp";
const SFTP_SUBSYSTEM: &str = "sftp";
const HELD_FIRST: &[u8] = b"lodestore capability probe";
const HELD_LATER: &[u8] = b", changed through a handle held open";

const HOST_KEY_ALGORITHMS: &[Algorithm] = &[
    Algorithm::Ed25519,
    Algorithm::Ecdsa {
        curve: EcdsaCurve::NistP256,
    },
    Algorithm::Ecdsa {
        curve: EcdsaCurve::NistP384,
    },
    Algorithm::Ecdsa {
        curve: EcdsaCurve::NistP521,
    },
];

/// How an object becomes visible under its final name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Publish {
    /// Write a temporary file, then `hardlink@openssh.com` it into place: the
    /// object appears whole or not at all, and an existing name is refused.
    Hardlink,
    /// Create the final name with `O_EXCL` and write into it. A write that is
    /// cut off leaves a short object behind, so callers compare bytes.
    Direct,
}

impl Publish {
    pub fn parse(value: Option<&str>) -> Result<Self> {
        match value {
            None | Some("hardlink") => Ok(Publish::Hardlink),
            Some("direct") => Ok(Publish::Direct),
            Some(other) => Err(Error::InvalidInput(format!(
                "unknown SFTP publish mode {other:?}; use `hardlink` or `direct`"
            ))),
        }
    }
}

/// A failed SFTP request: the status the server answered with, or a broken
/// connection.
#[derive(Debug)]
struct SftpFail {
    status: Option<StatusCode>,
    message: String,
    transport: bool,
}

impl SftpFail {
    fn from_sftp(error: &SftpError) -> Self {
        match error {
            SftpError::Status(s) => SftpFail {
                status: Some(s.status_code),
                message: error.to_string(),
                transport: matches!(
                    s.status_code,
                    StatusCode::NoConnection | StatusCode::ConnectionLost
                ),
            },
            SftpError::Limited(_) => SftpFail {
                status: None,
                message: error.to_string(),
                transport: false,
            },
            _ => SftpFail {
                status: None,
                message: error.to_string(),
                transport: true,
            },
        }
    }

    fn from_io(error: &std::io::Error) -> Self {
        match error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<SftpError>())
        {
            Some(inner) => Self::from_sftp(inner),
            None => SftpFail {
                status: None,
                message: error.to_string(),
                transport: error.kind() == std::io::ErrorKind::TimedOut,
            },
        }
    }

    fn is(&self, code: StatusCode) -> bool {
        self.status == Some(code)
    }

    fn into_blob(self) -> BlobError {
        match self.status {
            Some(StatusCode::PermissionDenied) => BlobError::Denied {
                message: "the SFTP server refused the request".into(),
            },
            Some(StatusCode::NoSuchFile) => BlobError::NotFound,
            _ if self.transport => BlobError::Backend {
                message: format!("SFTP connection error: {}", self.message),
                temporary: true,
            },
            _ => BlobError::backend(format!("SFTP error: {}", self.message)),
        }
    }
}

impl From<SftpError> for SftpFail {
    fn from(error: SftpError) -> Self {
        Self::from_sftp(&error)
    }
}

impl From<std::io::Error> for SftpFail {
    fn from(error: std::io::Error) -> Self {
        Self::from_io(&error)
    }
}

/// How one publication attempt ended when it did not succeed.
enum Attempt {
    Retry(BlobError),
    Done(BlobError),
}

struct Pinned {
    pinned: PublicKey,
    offered: Arc<std::sync::Mutex<Option<String>>>,
}

impl client::Handler for Pinned {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        let seen = match offered_public_key(key) {
            Ok(key) if key.key_data() == self.pinned.key_data() => return Ok(true),
            Ok(key) => fingerprint(&key),
            Err(certificate) => certificate,
        };
        if let Ok(mut slot) = self.offered.lock() {
            *slot = Some(seen);
        }
        Ok(false)
    }
}

/// The plain key a server offered; a host certificate is described instead,
/// because it is never a key that can be pinned.
fn offered_public_key(key: &PublicKeyOrCertificate) -> std::result::Result<PublicKey, String> {
    match key {
        PublicKeyOrCertificate::PublicKey { key, .. } => Ok(key.clone()),
        PublicKeyOrCertificate::Certificate(cert) => Err(format!(
            "a host certificate for {}",
            fingerprint(&PublicKey::from(cert.public_key().clone()))
        )),
    }
}

type OfferedSlot = Arc<std::sync::Mutex<Option<std::result::Result<PublicKey, String>>>>;

/// Remembers the host key and refuses it, so the handshake ends before any
/// authentication.
struct Capture {
    slot: OfferedSlot,
}

impl client::Handler for Capture {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        if let Ok(mut slot) = self.slot.lock() {
            *slot = Some(offered_public_key(key));
        }
        Ok(false)
    }
}

/// The host key a server presented on first contact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferedHostKey {
    pub openssh: String,
    pub fingerprint: String,
}

fn connect_error(e: &russh::Error) -> BlobError {
    match e {
        russh::Error::NoCommonAlgo {
            kind: russh::AlgorithmKind::Key,
            ..
        } => BlobError::backend(
            "the SFTP server offers no ed25519 or ECDSA host key; Lodestore pins only those",
        ),
        _ => temporary(format!("cannot connect to the SFTP server: {e}")),
    }
}

fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

struct Conn {
    sftp: SftpSession,
    _handle: Handle<Pinned>,
    hardlink: bool,
    fsync: bool,
}

#[cfg(feature = "test-hooks")]
type BeforeHardlink = Arc<dyn Fn(&str) + Send + Sync>;

/// How a direct write is cut off by [`SftpStore::tear_next_put`].
#[cfg(feature = "test-hooks")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tear {
    /// Hand the server this many bytes (at most all but the last), then break
    /// the connection.
    Short(usize),
    /// Write the second half of the object before the first, then break the
    /// connection: the file has its full length and zeros at the front.
    Hole,
}

#[cfg(feature = "test-hooks")]
#[derive(Default)]
struct Hooks {
    missing: std::sync::Mutex<Vec<String>>,
    before_hardlink: std::sync::Mutex<Option<BeforeHardlink>>,
    tears: std::sync::Mutex<std::collections::VecDeque<Option<Tear>>>,
}

struct Inner {
    host: String,
    port: u16,
    user: String,
    key: Arc<PrivateKey>,
    host_key: PublicKey,
    root: String,
    publish: Publish,
    conn: tokio::sync::Mutex<Option<Arc<Conn>>>,
    #[cfg(feature = "test-hooks")]
    hooks: Hooks,
}

/// Repository on an SFTP server, reached with an ed25519 or ECDSA key and a
/// pinned host key. The SSH stack is russh with aws-lc-rs.
#[derive(Clone)]
pub struct SftpStore {
    inner: Arc<Inner>,
}

impl fmt::Debug for SftpStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SftpStore").finish_non_exhaustive()
    }
}

fn parse_endpoint(endpoint: &str) -> Result<(String, u16)> {
    let bad = || {
        Error::InvalidInput(format!(
            "invalid SFTP endpoint {endpoint:?}; use host[:port]"
        ))
    };
    if endpoint.contains("://") {
        return Err(Error::InvalidInput(format!(
            "invalid SFTP endpoint {endpoint:?}; use host[:port], without ssh://"
        )));
    }
    let (host, port) = if let Some(rest) = endpoint.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').ok_or_else(bad)?;
        match tail {
            "" => (host, None),
            t => (host, Some(t.strip_prefix(':').ok_or_else(bad)?)),
        }
    } else if endpoint.matches(':').count() == 1 {
        let (host, port) = endpoint.split_once(':').ok_or_else(bad)?;
        (host, Some(port))
    } else {
        (endpoint, None)
    };
    if host.is_empty() {
        return Err(bad());
    }
    let port = match port {
        None => DEFAULT_PORT,
        Some(p) => p.parse::<u16>().map_err(|_| bad())?,
    };
    Ok((host.to_owned(), port))
}

fn parse_private_key(text: &str) -> Result<PrivateKey> {
    let key = PrivateKey::from_openssh(text.trim())
        .map_err(|_| Error::InvalidInput("the SFTP key is not an OpenSSH private key".into()))?;
    if key.is_encrypted() {
        return Err(Error::InvalidInput(
            "the SFTP key is encrypted; use a key without a passphrase".into(),
        ));
    }
    match key.algorithm() {
        Algorithm::Ed25519 | Algorithm::Ecdsa { .. } => Ok(key),
        _ => Err(Error::InvalidInput(
            "the SFTP key must be ed25519 or ECDSA; RSA and other key types are not supported"
                .into(),
        )),
    }
}

fn parse_host_key(line: &str) -> Result<PublicKey> {
    let key = PublicKey::from_openssh(line.trim()).map_err(|_| {
        Error::InvalidInput(
            "the SFTP host key is not an OpenSSH public key line such as `ssh-ed25519 AAAA...`; certificates are not accepted"
                .into(),
        )
    })?;
    match key.algorithm() {
        Algorithm::Ed25519 | Algorithm::Ecdsa { .. } => Ok(key),
        _ => Err(Error::InvalidInput(
            "the pinned SFTP host key must be ed25519 or ECDSA".into(),
        )),
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn mtime_ms(attrs: &Metadata) -> Option<i64> {
    attrs.mtime.map(|s| i64::from(s) * 1000)
}

/// What a directory entry is, as far as listing cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawKind {
    File,
    Dir,
    Other,
}

/// One entry of a directory the server listed: everything in it is the
/// server's say-so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEntry {
    pub name: String,
    pub size: Option<u64>,
    pub mtime_s: Option<u32>,
    pub kind: RawKind,
}

fn raw_entry(name: String, attrs: &Metadata) -> RawEntry {
    RawEntry {
        name,
        size: attrs.size,
        mtime_s: attrs.mtime,
        kind: match attrs.file_type() {
            FileType::File => RawKind::File,
            FileType::Dir => RawKind::Dir,
            FileType::Symlink | FileType::Other => RawKind::Other,
        },
    }
}

/// The key of entry `name` of the directory whose key prefix is `dir`, or
/// `None` for a name that is not a plain file name or a key that is not valid.
fn entry_key(dir: &str, name: &str) -> Option<String> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return None;
    }
    let key = format!("{dir}{name}");
    check_key(&key).ok()?;
    Some(key)
}

/// The files of one directory listing, as keys under `dir` that start with
/// `prefix`. A name with a separator, a dot name or a NUL is dropped, and so
/// is anything that is not a regular file. A file without a size counts as
/// empty, which a size check then flags.
pub fn listing_entries(prefix: &str, dir: &str, raw: &[RawEntry]) -> Vec<ListEntry> {
    raw.iter()
        .filter(|e| e.kind == RawKind::File)
        .filter_map(|e| {
            let key = entry_key(dir, &e.name).filter(|k| k.starts_with(prefix))?;
            Some(ListEntry {
                key,
                size: e.size.unwrap_or(0),
                modified_ms: e.mtime_s.and_then(|s| i64::from(s).checked_mul(1000)),
            })
        })
        .collect()
}

/// The subdirectories of one directory listing, as key prefixes ending in `/`.
pub fn listing_dirs(dir: &str, raw: &[RawEntry]) -> Vec<String> {
    raw.iter()
        .filter(|e| e.kind == RawKind::Dir)
        .filter_map(|e| Some(format!("{}/", entry_key(dir, &e.name)?)))
        .collect()
}

/// What [`listing_entries`] and [`listing_dirs`] promise, stated once for the
/// property test and the fuzz target.
#[cfg(any(test, feature = "test-hooks"))]
pub fn check_listing(prefix: &str, dir: &str, raw: &[RawEntry]) -> std::result::Result<(), String> {
    let sound = |key: &str, name: &str| {
        !key.is_empty()
            && !key.contains('\0')
            && key.starts_with(dir)
            && key.get(dir.len()..) == Some(name)
            && !name.is_empty()
            && !name.contains(['/', '\0'])
            && name != "."
            && name != ".."
            && key
                .split('/')
                .all(|c| !c.is_empty() && c != "." && c != "..")
    };
    for entry in listing_entries(prefix, dir, raw) {
        let name = entry.key.get(dir.len()..).unwrap_or_default();
        if !sound(&entry.key, name) || !entry.key.starts_with(prefix) {
            return Err(format!("unsound file key {:?}", entry.key));
        }
        let listed = raw.iter().any(|e| {
            e.kind == RawKind::File
                && e.name == name
                && entry.size == e.size.unwrap_or(0)
                && entry.modified_ms == e.mtime_s.map(|s| i64::from(s) * 1000)
        });
        if !listed {
            return Err(format!("{:?} is not a listed file", entry.key));
        }
    }
    for sub in listing_dirs(dir, raw) {
        let key = sub.strip_suffix('/').unwrap_or_default();
        let name = key.get(dir.len()..).unwrap_or_default();
        if !sound(key, name) || !raw.iter().any(|e| e.kind == RawKind::Dir && e.name == name) {
            return Err(format!("unsound directory key {sub:?}"));
        }
    }
    Ok(())
}

fn client_config() -> Arc<client::Config> {
    Arc::new(client::Config {
        keepalive_interval: Some(KEEPALIVE),
        preferred: russh::Preferred {
            key: std::borrow::Cow::Borrowed(HOST_KEY_ALGORITHMS),
            host_key_certificates: std::borrow::Cow::Borrowed(&[]),
            ..russh::Preferred::DEFAULT
        },
        ..Default::default()
    })
}

impl SftpStore {
    /// First contact: the host key the server presents, learned by the SSH
    /// handshake alone. The handshake is cut off before authentication, so no
    /// credential is sent, no channel is opened and nothing is written.
    pub async fn offered_host_key(endpoint: &str) -> Result<OfferedHostKey> {
        let (host, port) = parse_endpoint(endpoint)?;
        let slot: OfferedSlot = Arc::default();
        let handler = Capture {
            slot: Arc::clone(&slot),
        };
        let connecting = client::connect(client_config(), (host.as_str(), port), handler);
        let outcome = tokio::time::timeout(CONNECT_TIMEOUT, connecting).await;
        let seen = slot.lock().ok().and_then(|mut s| s.take());
        match (seen, outcome) {
            (Some(Ok(mut key)), _) => {
                key.set_comment("");
                let openssh = key.to_openssh().map_err(|e| {
                    Error::Store(BlobError::backend(format!(
                        "cannot encode the SFTP host key: {e}"
                    )))
                })?;
                Ok(OfferedHostKey {
                    openssh,
                    fingerprint: fingerprint(&key),
                })
            }
            (Some(Err(_)), _) => Err(Error::Store(BlobError::backend(
                "the SFTP server offered a host certificate; Lodestore pins plain ed25519 or ECDSA host keys only",
            ))),
            (None, Err(_)) => Err(Error::Store(temporary(
                "cannot connect to the SFTP server: timed out",
            ))),
            (None, Ok(Err(e))) => Err(Error::Store(connect_error(&e))),
            (None, Ok(Ok(_))) => Err(Error::Store(temporary(
                "cannot connect to the SFTP server: no host key was offered",
            ))),
        }
    }

    /// The SHA256 fingerprint of a pinned host key line.
    pub fn host_key_fingerprint(line: &str) -> Result<String> {
        Ok(fingerprint(&parse_host_key(line)?))
    }

    pub fn open(options: &BTreeMap<String, String>) -> Result<Self> {
        let opt = |k: &str| {
            debug_assert!(super::location::is_known_option("sftp", k), "{k}");
            options.get(k).map(String::as_str).filter(|v| !v.is_empty())
        };
        let need = |k: &str| {
            opt(k)
                .ok_or_else(|| Error::InvalidInput(format!("sftp location needs the `{k}` option")))
        };
        let (host, port) = parse_endpoint(need("endpoint")?)?;
        let user = need("user")?.to_owned();
        let key = parse_private_key(need("key")?)?;
        let host_key = parse_host_key(need("host_key")?)?;
        let publish = Publish::parse(opt("publish"))?;
        let root = opt("root").unwrap_or_default();
        if root.contains('\0') || root.split('/').any(|c| c == "..") {
            return Err(Error::InvalidInput("invalid SFTP root".into()));
        }
        let root = if root == "/" {
            "/".to_owned()
        } else {
            root.trim_end_matches('/').to_owned()
        };
        Ok(Self {
            inner: Arc::new(Inner {
                host,
                port,
                user,
                key: Arc::new(key),
                host_key,
                root,
                publish,
                conn: tokio::sync::Mutex::new(None),
                #[cfg(feature = "test-hooks")]
                hooks: Hooks::default(),
            }),
        })
    }

    pub fn publish_mode(&self) -> Publish {
        self.inner.publish
    }

    /// Whether the server advertises `fsync@openssh.com`. Without it a write
    /// is acknowledged before it is on disk.
    pub async fn durable_fsync(&self) -> std::result::Result<bool, BlobError> {
        let conn = self.conn().await?;
        Ok(self.advertises(&conn, FSYNC))
    }

    #[cfg(feature = "test-hooks")]
    pub fn pretend_missing_extension(&self, name: &str) {
        if let Ok(mut missing) = self.inner.hooks.missing.lock() {
            missing.push(name.to_owned());
        }
    }

    #[cfg(feature = "test-hooks")]
    pub fn on_before_hardlink(&self, hook: impl Fn(&str) + Send + Sync + 'static) {
        if let Ok(mut slot) = self.inner.hooks.before_hardlink.lock() {
            *slot = Some(Arc::new(hook));
        }
    }

    /// Cuts off the next direct write of an object under `packs/`; later
    /// ones are queued behind it. Index shards and snapshots are never torn:
    /// only packs are published again under a new name.
    #[cfg(feature = "test-hooks")]
    pub fn tear_next_put(&self, tear: Tear) {
        self.tear_puts([Some(tear)]);
    }

    /// Queues one entry per direct write under `packs/`: `Some` tears it,
    /// `None` lets it through.
    #[cfg(feature = "test-hooks")]
    pub fn tear_puts(&self, plan: impl IntoIterator<Item = Option<Tear>>) {
        if let Ok(mut queue) = self.inner.hooks.tears.lock() {
            queue.extend(plan);
        }
    }

    fn advertises(&self, conn: &Conn, extension: &str) -> bool {
        let offered = if extension == HARDLINK {
            conn.hardlink
        } else if extension == FSYNC {
            conn.fsync
        } else {
            false
        };
        #[cfg(feature = "test-hooks")]
        if let Ok(missing) = self.inner.hooks.missing.lock()
            && missing.iter().any(|m| m == extension)
        {
            return false;
        }
        offered
    }

    fn path(&self, key: &str) -> String {
        match self.inner.root.as_str() {
            "" => key.to_owned(),
            "/" => format!("/{key}"),
            root => format!("{root}/{key}"),
        }
    }

    fn tmp_path(&self) -> std::result::Result<String, BlobError> {
        let mut rnd = [0u8; 16];
        getrandom::fill(&mut rnd).map_err(|e| BlobError::backend(e.to_string()))?;
        Ok(self.path(&format!("{TMP_DIR}/{}", hex(&rnd))))
    }

    async fn conn(&self) -> std::result::Result<Arc<Conn>, BlobError> {
        let mut slot = self.inner.conn.lock().await;
        if let Some(conn) = slot.as_ref() {
            return Ok(Arc::clone(conn));
        }
        let conn = Arc::new(self.connect().await?);
        *slot = Some(Arc::clone(&conn));
        Ok(conn)
    }

    async fn reset(&self, broken: &Arc<Conn>) {
        let mut slot = self.inner.conn.lock().await;
        if slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, broken)) {
            *slot = None;
        }
    }

    async fn connect(&self) -> std::result::Result<Conn, BlobError> {
        let inner = &self.inner;
        let config = client_config();
        let offered = Arc::new(std::sync::Mutex::new(None));
        let handler = Pinned {
            pinned: inner.host_key.clone(),
            offered: Arc::clone(&offered),
        };
        let connecting = client::connect(config, (inner.host.as_str(), inner.port), handler);
        let mut handle = match tokio::time::timeout(CONNECT_TIMEOUT, connecting).await {
            Err(_) => return Err(temporary("timed out connecting to the SFTP server")),
            Ok(Ok(handle)) => handle,
            Ok(Err(e)) => {
                let seen = offered.lock().ok().and_then(|mut s| s.take());
                return Err(match seen {
                    Some(offered) => BlobError::HostKeyMismatch {
                        pinned: fingerprint(&inner.host_key),
                        offered,
                    },
                    None => connect_error(&e),
                });
            }
        };

        let key = PrivateKeyWithHashAlg::new(Arc::clone(&inner.key), None);
        let auth = handle
            .authenticate_publickey(inner.user.as_str(), key)
            .await
            .map_err(|e| temporary(format!("SFTP authentication failed: {e}")))?;
        if !auth.success() {
            return Err(BlobError::Denied {
                message: format!("the SFTP server refused the key of user {}", inner.user),
            });
        }

        let channel = handle
            .channel_open_session()
            .await
            .map_err(|e| temporary(format!("cannot open an SSH channel: {e}")))?;
        channel
            .request_subsystem(true, SFTP_SUBSYSTEM)
            .await
            .map_err(|e| temporary(format!("the SFTP subsystem was refused: {e}")))?;
        let raw = RawSftpSession::new(channel.into_stream());
        let version = raw
            .init()
            .await
            .map_err(|e| temporary(format!("SFTP handshake failed: {e}")))?;
        let _ = raw.close_session();
        let offers = |name: &str| version.extensions.get(name).is_some_and(|v| v == "1");
        let (hardlink, fsync) = (offers(HARDLINK), offers(FSYNC));

        let channel = handle
            .channel_open_session()
            .await
            .map_err(|e| temporary(format!("cannot open an SSH channel: {e}")))?;
        channel
            .request_subsystem(true, SFTP_SUBSYSTEM)
            .await
            .map_err(|e| temporary(format!("the SFTP subsystem was refused: {e}")))?;
        let sftp = SftpSession::new_with_config(
            channel.into_stream(),
            SftpConfig {
                request_timeout_secs: REQUEST_TIMEOUT_SECS,
                ..Default::default()
            },
        )
        .await
        .map_err(|e| temporary(format!("SFTP handshake failed: {e}")))?;
        Ok(Conn {
            sftp,
            _handle: handle,
            hardlink,
            fsync,
        })
    }

    /// Runs an idempotent request, once more on a fresh connection when the
    /// first one turns out to be broken.
    async fn run<T, F, Fut>(&self, op: F) -> std::result::Result<T, BlobError>
    where
        F: Fn(Arc<Conn>) -> Fut + Send + Sync,
        Fut: Future<Output = std::result::Result<T, SftpFail>> + Send,
        T: Send,
    {
        let mut again = true;
        loop {
            let conn = self.conn().await?;
            match op(Arc::clone(&conn)).await {
                Ok(value) => return Ok(value),
                Err(fail) if fail.transport => {
                    self.reset(&conn).await;
                    if again {
                        again = false;
                        continue;
                    }
                    return Err(fail.into_blob());
                }
                Err(fail) => return Err(fail.into_blob()),
            }
        }
    }

    async fn write_new(conn: &Conn, path: &str, data: &[u8]) -> std::result::Result<(), SftpFail> {
        let attrs = FileAttributes {
            permissions: Some(FILE_MODE),
            ..FileAttributes::empty()
        };
        let mut file = conn
            .sftp
            .open_with_flags_and_attributes(
                path,
                OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
                attrs,
            )
            .await?;
        file.write_all(data).await?;
        file.flush().await?;
        file.shutdown().await?;
        Ok(())
    }

    #[cfg(feature = "test-hooks")]
    async fn write_torn(
        conn: &Conn,
        path: &str,
        data: &[u8],
        tear: Tear,
    ) -> std::result::Result<(), SftpFail> {
        let attrs = FileAttributes {
            permissions: Some(FILE_MODE),
            ..FileAttributes::empty()
        };
        let mut file = conn
            .sftp
            .open_with_flags_and_attributes(
                path,
                OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
                attrs,
            )
            .await?;
        match tear {
            Tear::Short(len) => {
                let cut = len.min(data.len().saturating_sub(1));
                file.write_all(&data[..cut]).await?;
            }
            Tear::Hole => {
                let half = data.len() / 2;
                file.seek(SeekFrom::Start(half as u64)).await?;
                file.write_all(&data[half..]).await?;
            }
        }
        file.flush().await?;
        Err(SftpFail {
            status: None,
            message: "the connection was cut by a test hook".into(),
            transport: true,
        })
    }

    async fn put_hardlink(&self, key: &str, data: &Bytes) -> std::result::Result<(), BlobError> {
        let target = self.path(key);
        let mut retries_left = 1u8;
        loop {
            match self.publish_once(&target, data).await {
                Ok(()) => return Ok(()),
                Err(Attempt::Retry(error)) if retries_left > 0 => {
                    retries_left -= 1;
                    let _ = error;
                }
                Err(Attempt::Retry(error) | Attempt::Done(error)) => return Err(error),
            }
        }
    }

    async fn publish_once(&self, target: &str, data: &Bytes) -> std::result::Result<(), Attempt> {
        let conn = self.conn().await.map_err(Attempt::Done)?;
        if !self.advertises(&conn, HARDLINK) {
            return Err(Attempt::Done(BlobError::backend(
                "this SFTP server does not offer hardlink@openssh.com; create the repository with publish = direct",
            )));
        }
        let tmp = self.tmp_path().map_err(Attempt::Done)?;
        if let Err(fail) = Self::write_new(&conn, &tmp, data).await {
            return Err(self.tmp_write_failed(&conn, &tmp, fail).await);
        }

        #[cfg(feature = "test-hooks")]
        {
            let hook = self
                .inner
                .hooks
                .before_hardlink
                .lock()
                .ok()
                .and_then(|h| h.clone());
            if let Some(hook) = hook {
                hook(&tmp);
            }
        }

        let mut made_dir = false;
        loop {
            let fail = match conn.sftp.hardlink(&tmp, target).await {
                Ok(true) => return Ok(()),
                Ok(false) => {
                    return Err(Attempt::Done(BlobError::backend(
                        "this SFTP server does not offer hardlink@openssh.com; create the repository with publish = direct",
                    )));
                }
                Err(e) => SftpFail::from_sftp(&e),
            };
            if fail.transport {
                self.reset(&conn).await;
                return Err(Attempt::Retry(fail.into_blob()));
            }
            match fail.status {
                Some(StatusCode::Failure) => {
                    return Err(Attempt::Done(self.after_refusal(&conn, target).await));
                }
                Some(StatusCode::PermissionDenied) => {
                    return Err(Attempt::Done(BlobError::Denied {
                        message: "the SFTP server refused hardlink@openssh.com; create the repository with publish = direct if it cannot be allowed".into(),
                    }));
                }
                Some(StatusCode::NoSuchFile) => match conn.sftp.metadata(&tmp).await {
                    Err(SftpError::Status(s)) if s.status_code == StatusCode::NoSuchFile => {
                        return Err(Attempt::Retry(temporary(
                            "the temporary upload disappeared before it was published",
                        )));
                    }
                    Err(e) => {
                        let seen = SftpFail::from_sftp(&e);
                        if seen.transport {
                            self.reset(&conn).await;
                            return Err(Attempt::Retry(seen.into_blob()));
                        }
                        return Err(Attempt::Done(BlobError::backend(format!(
                            "cannot tell why {tmp} could not be linked to {target}: {}",
                            seen.message
                        ))));
                    }
                    Ok(_) => {
                        if made_dir {
                            return Err(Attempt::Done(BlobError::backend(format!(
                                "the directory of {target} does not exist"
                            ))));
                        }
                        made_dir = true;
                        let parent = target.rsplit_once('/').map_or("", |(p, _)| p);
                        if let Err(e) = conn.sftp.create_dir(parent).await {
                            let seen = SftpFail::from_sftp(&e);
                            if seen.transport {
                                self.reset(&conn).await;
                                return Err(Attempt::Retry(seen.into_blob()));
                            }
                            if seen.is(StatusCode::PermissionDenied) {
                                return Err(Attempt::Done(BlobError::Denied {
                                    message: format!(
                                        "the directory of {target} does not exist and may not be created"
                                    ),
                                }));
                            }
                        }
                    }
                },
                _ => {
                    return Err(Attempt::Done(fail.into_blob()));
                }
            }
        }
    }

    async fn tmp_write_failed(&self, conn: &Arc<Conn>, tmp: &str, fail: SftpFail) -> Attempt {
        if fail.transport {
            self.reset(conn).await;
            return Attempt::Retry(fail.into_blob());
        }
        if fail.is(StatusCode::Failure) {
            return Attempt::Retry(BlobError::backend(format!(
                "cannot create the temporary upload {tmp}: {}",
                fail.message
            )));
        }
        if fail.is(StatusCode::NoSuchFile) {
            return Attempt::Done(BlobError::backend(format!(
                "the upload directory of {tmp} does not exist; the repository was not initialized on this server"
            )));
        }
        Attempt::Done(fail.into_blob())
    }

    /// A refused hardlink or exclusive create answers `Failure` whether the
    /// name is taken or something else went wrong; only the stat tells.
    async fn after_refusal(&self, conn: &Arc<Conn>, target: &str) -> BlobError {
        match conn.sftp.metadata(target).await {
            Ok(_) => BlobError::AlreadyExists,
            Err(e) => {
                let seen = SftpFail::from_sftp(&e);
                if seen.transport {
                    self.reset(conn).await;
                }
                if seen.is(StatusCode::NoSuchFile) {
                    BlobError::backend(format!(
                        "the SFTP server refused to publish {target} and the object is not there"
                    ))
                } else {
                    seen.into_blob()
                }
            }
        }
    }

    async fn put_direct(&self, key: &str, data: &Bytes) -> std::result::Result<(), BlobError> {
        let target = self.path(key);
        let mut retries_left = 1u8;
        #[cfg(feature = "test-hooks")]
        let mut tear = if key.starts_with("packs/") {
            self.inner
                .hooks
                .tears
                .lock()
                .ok()
                .and_then(|mut queue| queue.pop_front().flatten())
        } else {
            None
        };
        loop {
            let conn = self.conn().await?;
            #[cfg(feature = "test-hooks")]
            let written = match tear.take() {
                Some(tear) => Self::write_torn(&conn, &target, data, tear).await,
                None => Self::write_new(&conn, &target, data).await,
            };
            #[cfg(not(feature = "test-hooks"))]
            let written = Self::write_new(&conn, &target, data).await;
            let fail = match written {
                Ok(()) => return Ok(()),
                Err(fail) => fail,
            };
            if fail.transport {
                self.reset(&conn).await;
                if retries_left > 0 {
                    retries_left -= 1;
                    continue;
                }
                return Err(fail.into_blob());
            }
            return Err(if fail.is(StatusCode::Failure) {
                self.after_refusal(&conn, &target).await
            } else if fail.is(StatusCode::NoSuchFile) {
                BlobError::backend(format!("the directory of {target} does not exist"))
            } else {
                fail.into_blob()
            });
        }
    }

    async fn ensure_dir(&self, conn: &Arc<Conn>, path: &str) -> std::result::Result<(), SftpFail> {
        let mut current = String::new();
        let absolute = path.starts_with('/');
        for part in path.split('/').filter(|p| !p.is_empty()) {
            current = if current.is_empty() && !absolute {
                part.to_owned()
            } else {
                format!("{current}/{part}")
            };
            if matches!(conn.sftp.metadata(current.as_str()).await, Ok(m) if m.is_dir()) {
                continue;
            }
            if let Err(e) = conn.sftp.create_dir(current.as_str()).await {
                let seen = SftpFail::from_sftp(&e);
                let exists = matches!(
                    conn.sftp.metadata(current.as_str()).await,
                    Ok(m) if m.is_dir()
                );
                if !exists {
                    return Err(seen);
                }
                continue;
            }
            let attrs = FileAttributes {
                permissions: Some(DIR_MODE),
                ..FileAttributes::empty()
            };
            if let Err(e) = conn.sftp.set_metadata(current.as_str(), attrs).await {
                let seen = SftpFail::from_sftp(&e);
                if seen.transport {
                    return Err(seen);
                }
            }
        }
        Ok(())
    }

    pub(super) async fn modified_ms(
        &self,
        key: &str,
    ) -> std::result::Result<Option<i64>, BlobError> {
        check_key(key)?;
        let path = self.path(key);
        self.run(|conn| {
            let path = path.clone();
            async move { Ok(mtime_ms(&conn.sftp.metadata(path).await?)) }
        })
        .await
    }

    /// Opens an existing object for writing and truncates it: the overwrite a
    /// compromised host would attempt. Probe objects only; the caller checks.
    pub(super) async fn overwrite_for_probe(
        &self,
        key: &str,
        data: Bytes,
    ) -> std::result::Result<(), BlobError> {
        check_key(key)?;
        let path = self.path(key);
        self.run(|conn| {
            let path = path.clone();
            let data = data.clone();
            async move {
                let mut file = conn
                    .sftp
                    .open_with_flags(path, OpenFlags::WRITE | OpenFlags::TRUNCATE)
                    .await?;
                file.write_all(&data).await?;
                file.flush().await?;
                file.shutdown().await?;
                Ok(())
            }
        })
        .await
    }

    /// Publishes a probe object and, with the write handle still open, writes
    /// more into it: whether the file the server then reads has changed. Mode
    /// 0444 and a deny-list do not reach a handle that is already open, so on
    /// OpenSSH this is `true`. Probe objects only.
    pub(super) async fn rewrite_held_handle_for_probe(
        &self,
        key: &str,
    ) -> std::result::Result<bool, BlobError> {
        check_key(key)?;
        if !super::probe::is_probe_key(key) {
            return Err(BlobError::backend(format!("{key:?} is not a probe object")));
        }
        let target = self.path(key);
        let tmp = match self.inner.publish {
            Publish::Hardlink => Some(self.tmp_path()?),
            Publish::Direct => None,
        };
        let conn = self.conn().await?;
        let held = async {
            let attrs = FileAttributes {
                permissions: Some(FILE_MODE),
                ..FileAttributes::empty()
            };
            let mut file = conn
                .sftp
                .open_with_flags_and_attributes(
                    tmp.as_deref().unwrap_or(&target),
                    OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
                    attrs,
                )
                .await?;
            file.write_all(HELD_FIRST).await?;
            file.flush().await?;
            if let Some(tmp) = &tmp
                && !conn.sftp.hardlink(tmp, &target).await?
            {
                return Ok(None);
            }
            file.write_all(HELD_LATER).await?;
            file.flush().await?;
            file.shutdown().await?;
            let mut seen = Vec::new();
            conn.sftp
                .open(&target)
                .await?
                .read_to_end(&mut seen)
                .await?;
            Ok::<_, SftpFail>(Some(seen.ends_with(HELD_LATER)))
        };
        match held.await {
            Ok(Some(changed)) => Ok(changed),
            Ok(None) => Err(BlobError::backend(
                "this SFTP server does not offer hardlink@openssh.com",
            )),
            Err(fail) => {
                if fail.transport {
                    self.reset(&conn).await;
                }
                Err(fail.into_blob())
            }
        }
    }

    /// Opens an object for writing without truncating or writing: whether the
    /// server lets this user write to it. Probe objects only.
    pub(super) async fn open_for_write_for_probe(
        &self,
        key: &str,
    ) -> std::result::Result<bool, BlobError> {
        check_key(key)?;
        if !super::probe::is_probe_key(key) {
            return Err(BlobError::backend(format!("{key:?} is not a probe object")));
        }
        let path = self.path(key);
        self.run(|conn| {
            let path = path.clone();
            async move {
                match conn.sftp.open_with_flags(path, OpenFlags::WRITE).await {
                    Ok(mut file) => {
                        file.shutdown().await?;
                        Ok(true)
                    }
                    Err(SftpError::Status(s)) if s.status_code == StatusCode::PermissionDenied => {
                        Ok(false)
                    }
                    Err(e) => Err(e.into()),
                }
            }
        })
        .await
    }

    #[cfg(feature = "test-hooks")]
    pub async fn rewrite_held_handle(&self, key: &str) -> std::result::Result<bool, BlobError> {
        self.rewrite_held_handle_for_probe(key).await
    }

    #[cfg(feature = "test-hooks")]
    pub async fn open_for_write(&self, key: &str) -> std::result::Result<bool, BlobError> {
        self.open_for_write_for_probe(key).await
    }

    async fn read_raw(conn: &Conn, dir: &str) -> std::result::Result<Vec<RawEntry>, SftpFail> {
        match conn.sftp.read_dir(dir).await {
            Ok(entries) => Ok(entries
                .map(|e| raw_entry(e.file_name(), &e.metadata()))
                .collect()),
            Err(SftpError::Status(s)) if s.status_code == StatusCode::NoSuchFile => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    /// The keys under `tmp/` whose modification time is `older_than_ms` or
    /// earlier, for the server's cleanup of abandoned uploads. The times are
    /// the SFTP server's own, so the caller leaves a wide margin. An entry
    /// without a time is never reported.
    pub async fn stale_tmp(
        &self,
        older_than_ms: i64,
    ) -> std::result::Result<Vec<String>, BlobError> {
        let dir = self.path(TMP_DIR);
        let raw = self
            .run(|conn| {
                let dir = dir.clone();
                async move { Self::read_raw(&conn, &dir).await }
            })
            .await?;
        let tmp = format!("{TMP_DIR}/");
        let mut stale: Vec<String> = listing_entries(&tmp, &tmp, &raw)
            .into_iter()
            .filter(|e| e.modified_ms.is_some_and(|m| m <= older_than_ms))
            .map(|e| e.key)
            .collect();
        stale.sort();
        Ok(stale)
    }
}

fn temporary(message: impl Into<String>) -> BlobError {
    BlobError::Backend {
        message: message.into(),
        temporary: true,
    }
}

impl BlobStore for SftpStore {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> std::result::Result<(), BlobError> {
        check_key(key)?;
        match self.inner.publish {
            Publish::Hardlink => self.put_hardlink(key, &data).await,
            Publish::Direct => self.put_direct(key, &data).await,
        }
    }

    async fn get(&self, key: &str) -> std::result::Result<Bytes, BlobError> {
        check_key(key)?;
        let path = self.path(key);
        self.run(|conn| {
            let path = path.clone();
            async move {
                let mut file = conn.sftp.open(path).await?;
                let mut buf = Vec::new();
                file.read_to_end(&mut buf).await?;
                Ok(Bytes::from(buf))
            }
        })
        .await
    }

    async fn get_range(
        &self,
        key: &str,
        range: Range<u64>,
    ) -> std::result::Result<Bytes, BlobError> {
        check_key(key)?;
        let path = self.path(key);
        self.run(|conn| {
            let path = path.clone();
            let range = range.clone();
            async move {
                let mut file = conn.sftp.open(path).await?;
                let size = file.metadata().await?.size;
                let end = size.map_or(range.end, |s| range.end.min(s));
                if end <= range.start {
                    return Ok(Bytes::new());
                }
                let len = end - range.start;
                file.seek(SeekFrom::Start(range.start)).await?;
                let mut buf = Vec::with_capacity(usize::try_from(len.min(1 << 20)).unwrap_or(0));
                (&mut file).take(len).read_to_end(&mut buf).await?;
                Ok(Bytes::from(buf))
            }
        })
        .await
    }

    async fn list(&self, prefix: &str) -> std::result::Result<Vec<ListEntry>, BlobError> {
        let (dir_part, _) = prefix.rsplit_once('/').unwrap_or(("", prefix));
        if !dir_part.is_empty() {
            check_key(dir_part)?;
        }
        let top_key_prefix = if dir_part.is_empty() {
            String::new()
        } else {
            format!("{dir_part}/")
        };
        let found = self
            .run(|conn| {
                let top = top_key_prefix.clone();
                async move {
                    let mut files = Vec::new();
                    let mut pending = vec![(top.clone(), 0usize)];
                    while let Some((key_prefix, depth)) = pending.pop() {
                        let dir = self.path(key_prefix.trim_end_matches('/'));
                        let dir = if dir.is_empty() { ".".to_owned() } else { dir };
                        let raw = Self::read_raw(&conn, &dir).await?;
                        files.extend(listing_entries(prefix, &key_prefix, &raw));
                        for sub in listing_dirs(&key_prefix, &raw) {
                            let is_tmp = top.is_empty() && sub == format!("{TMP_DIR}/");
                            if depth < MAX_LIST_DEPTH && !is_tmp {
                                pending.push((sub, depth + 1));
                            }
                        }
                    }
                    Ok(files)
                }
            })
            .await?;
        let mut out = found;
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    async fn object_size(&self, key: &str) -> std::result::Result<Option<u64>, BlobError> {
        check_key(key)?;
        let path = self.path(key);
        let seen = self
            .run(|conn| {
                let path = path.clone();
                async move {
                    match conn.sftp.metadata(path).await {
                        Ok(meta) => Ok(Some(meta.size)),
                        Err(SftpError::Status(s)) if s.status_code == StatusCode::NoSuchFile => {
                            Ok(None)
                        }
                        Err(e) => Err(e.into()),
                    }
                }
            })
            .await?;
        match seen {
            None => Ok(None),
            Some(Some(size)) => Ok(Some(size)),
            Some(None) => Err(BlobError::backend(format!(
                "the SFTP server did not report the size of {key}"
            ))),
        }
    }

    async fn ensure_prefixes(&self, prefixes: &[&str]) -> std::result::Result<(), BlobError> {
        let mut dirs: Vec<String> = Vec::new();
        for p in prefixes {
            let p = p.trim_matches('/');
            check_key(p)?;
            dirs.push(p.to_owned());
        }
        for fixed in ["keys", TMP_DIR] {
            dirs.push(fixed.to_owned());
        }
        for shard in 0..=255u8 {
            dirs.push(format!("packs/{shard:02x}"));
        }
        dirs.sort();
        dirs.dedup();
        let root = self.inner.root.clone();
        let paths: Vec<String> = dirs.iter().map(|d| self.path(d)).collect();
        self.run(|conn| {
            let root = root.clone();
            let paths = paths.clone();
            async move {
                if !root.is_empty() {
                    self.ensure_dir(&conn, &root).await?;
                }
                for path in &paths {
                    self.ensure_dir(&conn, path).await?;
                }
                Ok(())
            }
        })
        .await
    }

    fn publishes_atomically(&self) -> bool {
        self.inner.publish == Publish::Hardlink
    }
}

impl BlobDelete for SftpStore {
    async fn delete(&self, key: &str) -> std::result::Result<(), BlobError> {
        check_key(key)?;
        let path = self.path(key);
        self.run(|conn| {
            let path = path.clone();
            async move { Ok(conn.sftp.remove_file(path).await?) }
        })
        .await
    }
}

impl BlobPurge for SftpStore {
    async fn purge(&self, key: &str) -> PurgeOutcome {
        let bytes = self.object_size(key).await.ok().flatten().unwrap_or(0);
        match self.delete(key).await {
            Ok(()) => PurgeOutcome::Purged { versions: 1, bytes },
            Err(BlobError::NotFound) => PurgeOutcome::NotFound,
            Err(e) => PurgeOutcome::Failed(e),
        }
    }

    async fn deleted_keys(
        &self,
        _prefix: &str,
        _older_than_ms: i64,
    ) -> std::result::Result<Vec<String>, BlobError> {
        Ok(Vec::new())
    }

    async fn rewritten_keys(&self, _prefix: &str) -> std::result::Result<Vec<String>, BlobError> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn options(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn endpoint_parsing() {
        assert_eq!(parse_endpoint("host").unwrap(), ("host".into(), 22));
        assert_eq!(parse_endpoint("host:2222").unwrap(), ("host".into(), 2222));
        assert_eq!(parse_endpoint("[::1]").unwrap(), ("::1".into(), 22));
        assert_eq!(parse_endpoint("[::1]:23").unwrap(), ("::1".into(), 23));
        assert_eq!(parse_endpoint("::1").unwrap(), ("::1".into(), 22));
        for bad in ["", ":22", "host:", "host:x", "host:70000", "[::1", "[::1]x"] {
            assert!(parse_endpoint(bad).is_err(), "{bad:?}");
        }
        for schemed in ["ssh://host:23", "sftp://host", "ssh://[::1]:23", "x://"] {
            match parse_endpoint(schemed) {
                Err(Error::InvalidInput(m)) => assert!(m.contains("without ssh://"), "{m}"),
                other => panic!("{schemed:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn storage_box_preset_address_parses_to_port_23() {
        let template = crate::store::policy::STORAGE_PRESETS
            .iter()
            .find(|p| p.id == "hetzner-storage-box")
            .and_then(|p| p.endpoint_template)
            .expect("the Storage Box preset has an address template");
        let address = template.replace("{user}", "u123");
        assert_eq!(
            parse_endpoint(&address).unwrap(),
            ("u123.your-storagebox.de".into(), 23)
        );
    }

    #[test]
    fn publish_modes() {
        assert_eq!(Publish::parse(None).unwrap(), Publish::Hardlink);
        assert_eq!(Publish::parse(Some("hardlink")).unwrap(), Publish::Hardlink);
        assert_eq!(Publish::parse(Some("direct")).unwrap(), Publish::Direct);
        assert!(matches!(
            Publish::parse(Some("rename")),
            Err(Error::InvalidInput(_))
        ));
    }

    fn attrs(mode: u32, size: u64) -> Metadata {
        Metadata {
            permissions: Some(mode),
            size: Some(size),
            mtime: Some(7),
            ..Metadata::empty()
        }
    }

    fn raw(name: &str, attrs: &Metadata) -> RawEntry {
        raw_entry(name.to_owned(), attrs)
    }

    #[test]
    fn listing_entries_are_vetted() {
        let file = attrs(0o100_444, 9);
        let dir = attrs(0o040_755, 0);
        assert_eq!(
            listing_entries("packs/", "packs/ab/", &[raw("obj", &file)]),
            [ListEntry {
                key: "packs/ab/obj".into(),
                size: 9,
                modified_ms: Some(7000),
            }]
        );
        assert_eq!(
            listing_dirs("packs/", &[raw("ab", &dir)]),
            ["packs/ab/".to_owned()]
        );
        assert!(listing_entries("", "packs/", &[raw("ab", &dir)]).is_empty());
        assert!(listing_dirs("packs/", &[raw("obj", &file)]).is_empty());
        for name in ["", ".", "..", "a/b", "../x", "a\0b", "/etc/passwd"] {
            let entries = [raw(name, &file), raw(name, &dir)];
            assert!(
                listing_entries("", "packs/", &entries).is_empty(),
                "{name:?}"
            );
            assert!(listing_dirs("packs/", &entries).is_empty(), "{name:?}");
        }
        let skipped = [
            raw("link", &attrs(0o120_777, 3)),
            raw("noperms", &Metadata::empty()),
        ];
        assert!(listing_entries("", "", &skipped).is_empty());
        let mut nosize = file.clone();
        nosize.size = None;
        assert_eq!(listing_entries("", "", &[raw("obj", &nosize)])[0].size, 0);
        assert!(listing_entries("other", "", &[raw("obj", &file)]).is_empty());
    }

    fn hostile_name() -> impl Strategy<Value = String> {
        let piece = prop_oneof![
            Just(String::new()),
            Just(".".to_owned()),
            Just("..".to_owned()),
            Just("/".to_owned()),
            Just("\\".to_owned()),
            Just("\0".to_owned()),
            Just("packs".to_owned()),
            Just("\u{e9}".to_owned()),
            "[a-f0-9]{1,8}",
            ".*",
        ];
        prop::collection::vec(piece, 0..4).prop_map(|parts| parts.concat())
    }

    fn hostile_entry() -> impl Strategy<Value = RawEntry> {
        (
            hostile_name(),
            prop::option::of(any::<u64>()),
            prop::option::of(any::<u32>()),
            prop_oneof![
                Just(RawKind::File),
                Just(RawKind::Dir),
                Just(RawKind::Other)
            ],
        )
            .prop_map(|(name, size, mtime_s, kind)| RawEntry {
                name,
                size,
                mtime_s,
                kind,
            })
    }

    proptest! {
        #[test]
        fn hostile_listings_yield_only_sound_keys(
            prefix in prop_oneof![Just(String::new()), "[a-z/.]{0,6}", ".*"],
            dir in prop_oneof![Just(String::new()), Just("packs/".to_owned()), "[a-z./]{0,8}", ".*"],
            raw in prop::collection::vec(hostile_entry(), 0..16),
        ) {
            prop_assert_eq!(check_listing(&prefix, &dir, &raw), Ok(()));
        }
    }

    #[test]
    fn unsupported_keys_are_refused_when_opened() {
        let dir = tempfile::tempdir().unwrap();
        let keygen = |args: &[&str], name: &str| {
            let path = dir.path().join(name);
            let status = std::process::Command::new("ssh-keygen")
                .args(["-q", "-C", "t"])
                .args(args)
                .arg("-f")
                .arg(&path)
                .status()
                .expect("ssh-keygen");
            assert!(status.success());
            (
                std::fs::read_to_string(&path).unwrap(),
                std::fs::read_to_string(dir.path().join(format!("{name}.pub"))).unwrap(),
            )
        };
        let (ed, ed_pub) = keygen(&["-t", "ed25519", "-N", ""], "ed");
        let (ec, _) = keygen(&["-t", "ecdsa", "-b", "384", "-N", ""], "ec");
        let (rsa, rsa_pub) = keygen(&["-t", "rsa", "-b", "2048", "-N", ""], "rsa");
        let (locked, _) = keygen(&["-t", "ed25519", "-N", "secret"], "locked");

        let open = |key: &str, host_key: &str, extra: &[(&str, &str)]| {
            let mut pairs = vec![
                ("endpoint", "127.0.0.1:2222"),
                ("user", "u"),
                ("key", key),
                ("host_key", host_key),
            ];
            pairs.extend_from_slice(extra);
            SftpStore::open(&options(&pairs))
        };
        assert!(open(&ed, &ed_pub, &[]).is_ok());
        assert!(open(&ec, &ed_pub, &[]).is_ok());
        assert!(open(&ed, &ed_pub, &[("publish", "direct")]).is_ok());
        for refused in [
            open(&rsa, &ed_pub, &[]),
            open(&locked, &ed_pub, &[]),
            open(&ed, &rsa_pub, &[]),
            open(
                &ed,
                "ssh-ed25519-cert-v01@openssh.com AAAAIHNzaC1lZDI1NTE5LWNlcnQtdjAxQG9wZW5zc2guY29t",
                &[],
            ),
            open(&ed, &ed_pub, &[("publish", "rename")]),
            open(&ed, &ed_pub, &[("root", "../up")]),
            open("not a key", &ed_pub, &[]),
        ] {
            assert!(matches!(refused, Err(Error::InvalidInput(_))));
        }
    }

    fn host_key_line(args: &[&str]) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k");
        let status = std::process::Command::new("ssh-keygen")
            .args(["-q", "-C", "t", "-N", ""])
            .args(args)
            .arg("-f")
            .arg(&path)
            .status()
            .expect("ssh-keygen");
        assert!(status.success());
        std::fs::read_to_string(dir.path().join("k.pub")).unwrap()
    }

    #[test]
    fn host_key_fingerprint_matches_the_pinned_key() {
        for args in [&["-t", "ed25519"][..], &["-t", "ecdsa", "-b", "256"][..]] {
            let line = host_key_line(args);
            let key = PublicKey::from_openssh(line.trim()).unwrap();
            let fp = SftpStore::host_key_fingerprint(&line).unwrap();
            assert!(fp.starts_with("SHA256:"));
            assert_eq!(fp, key.fingerprint(HashAlg::Sha256).to_string());
        }
        for refused in [
            host_key_line(&["-t", "rsa", "-b", "2048"]),
            "ssh-ed25519-cert-v01@openssh.com AAAAIHNzaC1lZDI1NTE5LWNlcnQtdjAxQG9wZW5zc2guY29t"
                .into(),
            "not a key".into(),
        ] {
            assert!(matches!(
                SftpStore::host_key_fingerprint(&refused),
                Err(Error::InvalidInput(_))
            ));
        }
    }

    #[tokio::test]
    async fn offered_host_key_rejects_a_bad_endpoint() {
        for endpoint in ["", "host:notaport"] {
            assert!(matches!(
                SftpStore::offered_host_key(endpoint).await,
                Err(Error::InvalidInput(_))
            ));
        }
    }

    #[tokio::test]
    async fn offered_host_key_unreachable_is_temporary() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let err = SftpStore::offered_host_key(&format!("127.0.0.1:{port}"))
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                Error::Store(BlobError::Backend {
                    temporary: true,
                    ..
                })
            ),
            "{err:?}"
        );
    }

    #[test]
    fn offered_public_key_keeps_a_plain_key() {
        let key = PublicKey::from_openssh(host_key_line(&["-t", "ed25519"]).trim()).unwrap();
        let offered = PublicKeyOrCertificate::PublicKey {
            key: key.clone(),
            hash_alg: None,
        };
        assert_eq!(offered_public_key(&offered), Ok(key));
    }

    #[test]
    fn rsa_only_server_is_a_permanent_error() {
        let no_key = russh::Error::NoCommonAlgo {
            kind: russh::AlgorithmKind::Key,
            ours: vec!["ssh-ed25519".into()],
            theirs: vec!["rsa-sha2-512".into()],
        };
        assert!(matches!(
            connect_error(&no_key),
            BlobError::Backend {
                temporary: false,
                ref message,
            } if message.contains("no ed25519 or ECDSA host key")
        ));
        for other in [
            russh::Error::NoCommonAlgo {
                kind: russh::AlgorithmKind::Kex,
                ours: vec![],
                theirs: vec![],
            },
            russh::Error::Disconnect,
        ] {
            assert!(matches!(
                connect_error(&other),
                BlobError::Backend {
                    temporary: true,
                    ..
                }
            ));
        }
    }

    #[test]
    fn debug_hides_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k");
        assert!(
            std::process::Command::new("ssh-keygen")
                .args(["-q", "-N", "", "-t", "ed25519", "-f"])
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        let key = std::fs::read_to_string(&path).unwrap();
        let host_key = std::fs::read_to_string(dir.path().join("k.pub")).unwrap();
        let store = SftpStore::open(&options(&[
            ("endpoint", "h"),
            ("user", "u"),
            ("key", &key),
            ("host_key", &host_key),
        ]))
        .unwrap();
        let shown = format!("{store:?}");
        let body = key
            .lines()
            .find(|l| l.len() > 20 && !l.starts_with('-'))
            .unwrap();
        assert!(!shown.contains(body) && !shown.contains("OPENSSH"));
    }
}
