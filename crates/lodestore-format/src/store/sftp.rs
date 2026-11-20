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
        let seen = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => {
                if key.key_data() == self.pinned.key_data() {
                    return Ok(true);
                }
                fingerprint(key)
            }
            PublicKeyOrCertificate::Certificate(cert) => format!(
                "a host certificate for {}",
                fingerprint(&PublicKey::from(cert.public_key().clone()))
            ),
        };
        if let Ok(mut slot) = self.offered.lock() {
            *slot = Some(seen);
        }
        Ok(false)
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
    /// Hand the server this many bytes, then break the connection.
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

/// An entry of a directory listing the server sent, once its name has been
/// vetted: names with a separator, a dot name, a NUL or a bad key are dropped.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Listed {
    File(ListEntry),
    Dir(String),
}

pub(crate) fn vet_entry(parent: &str, name: &str, attrs: &Metadata) -> Option<Listed> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return None;
    }
    let key = format!("{parent}{name}");
    check_key(&key).ok()?;
    match attrs.file_type() {
        FileType::Dir => Some(Listed::Dir(format!("{key}/"))),
        FileType::File => Some(Listed::File(ListEntry {
            key,
            size: attrs.size?,
            modified_ms: mtime_ms(attrs),
        })),
        FileType::Symlink | FileType::Other => None,
    }
}

impl SftpStore {
    pub fn open(options: &BTreeMap<String, String>) -> Result<Self> {
        let opt = |k: &str| options.get(k).map(String::as_str).filter(|v| !v.is_empty());
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
        let config = Arc::new(client::Config {
            keepalive_interval: Some(KEEPALIVE),
            preferred: russh::Preferred {
                key: std::borrow::Cow::Borrowed(HOST_KEY_ALGORITHMS),
                host_key_certificates: std::borrow::Cow::Borrowed(&[]),
                ..russh::Preferred::DEFAULT
            },
            ..Default::default()
        });
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
                    None => temporary(format!("cannot connect to the SFTP server: {e}")),
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
            Tear::Short(len) => file.write_all(&data[..len.min(data.len())]).await?,
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

    async fn list_dir(
        conn: &Conn,
        dir: &str,
        key_prefix: &str,
    ) -> std::result::Result<Vec<Listed>, SftpFail> {
        let entries = match conn.sftp.read_dir(dir).await {
            Ok(entries) => entries,
            Err(SftpError::Status(s)) if s.status_code == StatusCode::NoSuchFile => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(e.into()),
        };
        Ok(entries
            .filter_map(|e| vet_entry(key_prefix, &e.file_name(), &e.metadata()))
            .collect())
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
        let prefix = prefix.to_owned();
        let found = self
            .run(|conn| {
                let top = top_key_prefix.clone();
                async move {
                    let mut files = Vec::new();
                    let mut pending = vec![(top.clone(), 0usize)];
                    while let Some((key_prefix, depth)) = pending.pop() {
                        let dir = self.path(key_prefix.trim_end_matches('/'));
                        let dir = if dir.is_empty() { ".".to_owned() } else { dir };
                        for entry in Self::list_dir(&conn, &dir, &key_prefix).await? {
                            match entry {
                                Listed::File(file) => files.push(file),
                                Listed::Dir(sub) => {
                                    let is_tmp = top.is_empty() && sub == format!("{TMP_DIR}/");
                                    if depth < MAX_LIST_DEPTH && !is_tmp {
                                        pending.push((sub, depth + 1));
                                    }
                                }
                            }
                        }
                    }
                    Ok(files)
                }
            })
            .await?;
        let mut out: Vec<ListEntry> = found
            .into_iter()
            .filter(|e| e.key.starts_with(&prefix))
            .collect();
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
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn listing_entries_are_vetted() {
        let file = attrs(0o100_444, 9);
        let dir = attrs(0o040_755, 0);
        assert_eq!(
            vet_entry("packs/ab/", "obj", &file),
            Some(Listed::File(ListEntry {
                key: "packs/ab/obj".into(),
                size: 9,
                modified_ms: Some(7000),
            }))
        );
        assert_eq!(
            vet_entry("packs/", "ab", &dir),
            Some(Listed::Dir("packs/ab/".into()))
        );
        for name in ["", ".", "..", "a/b", "../x", "a\0b", "/etc/passwd"] {
            assert_eq!(vet_entry("packs/", name, &file), None, "{name:?}");
        }
        assert_eq!(vet_entry("", "link", &attrs(0o120_777, 3)), None);
        assert_eq!(vet_entry("", "noperms", &Metadata::empty()), None);
        let mut nosize = file.clone();
        nosize.size = None;
        assert_eq!(vet_entry("", "obj", &nosize), None);
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
