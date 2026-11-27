//! The agent's local unix socket. Its one user operation is `backup_stdin`: a
//! user on the host streams data in and the agent writes it to the repository
//! with its own keys. The `pve_*` operations belong to the Proxmox VE provider
//! (see `crate::pve::socket`). See `docs/src/content/docs/docs/reference/local-socket.md`
//! for the protocol.

use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::net::UnixListener;
use tokio_util::sync::CancellationToken;

use crate::jobs::{JobHub, stream};
use crate::pve::ROOT_UID;
use crate::pve::socket::{MAX_PVE_LINE, Peer, PveSessions};

pub const PROTOCOL_VERSION: u32 = 1;
pub const OP_BACKUP_STDIN: &str = "backup_stdin";
pub const MAX_HEADER: usize = 4096;
const SOCKET_GROUP: &str = "lodestore";
pub const FIRST_LINE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_POLICY: usize = 128;
pub const MAX_HANDLERS: usize = 16;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StdinHeader {
    pub v: u32,
    pub op: String,
    pub name: String,
    #[serde(default)]
    pub policy: Option<String>,
}

struct Rejection {
    code: &'static str,
    message: String,
}

fn rejection(code: &'static str, message: impl Into<String>) -> Rejection {
    Rejection {
        code,
        message: message.into(),
    }
}

fn read_line(stream: &mut UnixStream, limit: usize, wait: Duration) -> Result<Vec<u8>, Rejection> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    let deadline = Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(rejection(
                "bad_request",
                "the header was not complete in time",
            ));
        }
        let _ = stream.set_read_timeout(Some(left));
        match stream.read(&mut byte) {
            Ok(0) => {
                return Err(rejection(
                    "bad_request",
                    "the connection closed before the header ended",
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                return Err(rejection(
                    "bad_request",
                    format!("cannot read the header: {e}"),
                ));
            }
        }
        if byte[0] == b'\n' {
            return Ok(line);
        }
        if line.len() + 1 >= limit {
            return Err(rejection(
                "bad_request",
                format!("the header is longer than {limit} bytes"),
            ));
        }
        line.push(byte[0]);
    }
}

fn parse_header(line: &[u8]) -> Result<StdinHeader, Rejection> {
    if line.len() + 1 >= MAX_HEADER {
        return Err(rejection(
            "bad_request",
            format!("the header is longer than {MAX_HEADER} bytes"),
        ));
    }
    let header: StdinHeader = serde_json::from_slice(line)
        .map_err(|e| rejection("bad_request", format!("the header is not valid: {e}")))?;
    if header.op != OP_BACKUP_STDIN {
        return Err(rejection(
            "unsupported",
            format!("the only operation is {OP_BACKUP_STDIN}"),
        ));
    }
    if header.v != PROTOCOL_VERSION {
        return Err(rejection(
            "unsupported",
            format!("protocol version {} is not supported", header.v),
        ));
    }
    if lodestore_format::tree::validate_name(header.name.as_bytes()).is_err() {
        return Err(rejection(
            "bad_request",
            "name must be 1 to 1024 bytes, without '/' or NUL, and not . or ..",
        ));
    }
    if let Some(policy) = &header.policy
        && (policy.is_empty() || policy.len() > MAX_POLICY || policy.chars().any(char::is_control))
    {
        return Err(rejection(
            "bad_request",
            "policy is not a valid policy name",
        ));
    }
    Ok(header)
}

fn is_pve_op(line: &[u8]) -> bool {
    #[derive(Deserialize)]
    struct Op {
        op: String,
    }
    serde_json::from_slice::<Op>(line).is_ok_and(|o| o.op.starts_with("pve_"))
}

fn handle(
    hub: &Arc<JobHub>,
    pve: &Arc<PveSessions>,
    mut stream: UnixStream,
    peer: Peer,
    gid: u32,
    guard: HandlerGuard,
) {
    let line = match read_line(&mut stream, MAX_PVE_LINE, pve.first_line_timeout()) {
        Ok(line) => line,
        Err(r) => {
            drop(guard);
            stream::reply_error(&stream, r.code, &r.message);
            return;
        }
    };
    if is_pve_op(&line) {
        // The long provider session must not hold a slot of the stdin streams.
        drop(guard);
        pve.serve_connection(stream, line, peer);
        return;
    }
    match parse_header(&line) {
        Ok(header) => {
            hub.submit_stdin(header, peer.uid, gid, stream);
            drop(guard);
        }
        Err(r) => {
            // Free the slot before the caller can see the connection close.
            drop(guard);
            stream::reply_error(&stream, r.code, &r.message);
        }
    }
}

fn socket_group_id() -> Option<u32> {
    let groups = std::fs::read_to_string("/etc/group").ok()?;
    groups.lines().find_map(|line| {
        let mut fields = line.split(':');
        (fields.next()? == SOCKET_GROUP)
            .then(|| fields.nth(1)?.parse().ok())
            .flatten()
    })
}

fn bind(path: &Path) -> std::io::Result<UnixListener> {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket()) {
        std::fs::remove_file(path)?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let staging = parent.join(format!(".bind-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::DirBuilder::new().mode(0o700).create(&staging)?;
    let result = bind_staged(&staging, path);
    let _ = std::fs::remove_dir_all(&staging);
    result
}

fn bind_staged(staging: &Path, path: &Path) -> std::io::Result<UnixListener> {
    let staged = staging.join("s");
    let listener = UnixListener::bind(&staged)?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o660))?;
    if rustix::process::geteuid().is_root()
        && let Some(gid) = socket_group_id()
    {
        rustix::fs::chown(
            &staged,
            Some(rustix::fs::Uid::ROOT),
            Some(rustix::fs::Gid::from_raw(gid)),
        )?;
    }
    std::fs::rename(&staged, path)?;
    Ok(listener)
}

struct HandlerGuard(Arc<AtomicUsize>);

impl Drop for HandlerGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Listens on `path` until `shutdown`, serving the Proxmox provider to root
/// only.
pub async fn serve(hub: Arc<JobHub>, path: std::path::PathBuf, shutdown: CancellationToken) {
    serve_with(hub, Arc::new(PveSessions::new(ROOT_UID)), path, shutdown).await;
}

/// Listens on `path` until `shutdown`. A socket that cannot be bound is
/// logged and left alone: backups through the server do not depend on it.
pub async fn serve_with(
    hub: Arc<JobHub>,
    pve: Arc<PveSessions>,
    path: std::path::PathBuf,
    shutdown: CancellationToken,
) {
    let listener = match bind(&path) {
        Ok(listener) => listener,
        Err(error) => {
            tracing::warn!(socket = %path.display(), %error, "cannot open the local socket; lode backup --stdin will not reach this agent");
            return;
        }
    };
    tracing::info!(socket = %path.display(), "local socket ready");
    let live = Arc::new(AtomicUsize::new(0));
    loop {
        let accepted = tokio::select! {
            () = shutdown.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let stream = match accepted {
            Ok((stream, _)) => stream,
            Err(error) => {
                tracing::warn!(%error, "cannot accept a local connection");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let cred = match stream.peer_cred() {
            Ok(cred) => cred,
            Err(error) => {
                tracing::warn!(%error, "cannot read the caller of a local connection");
                continue;
            }
        };
        let Ok(stream) = stream.into_std() else {
            continue;
        };
        if stream.set_nonblocking(false).is_err() {
            continue;
        }
        if live.fetch_add(1, Ordering::AcqRel) >= MAX_HANDLERS {
            live.fetch_sub(1, Ordering::AcqRel);
            let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
            stream::reply_error(&stream, "busy", "too many connections; try again later");
            continue;
        }
        let hub = Arc::clone(&hub);
        let pve = Arc::clone(&pve);
        let guard = HandlerGuard(Arc::clone(&live));
        let peer = Peer {
            uid: cred.uid(),
            pid: cred.pid().and_then(|p| u32::try_from(p).ok()),
        };
        let gid = cred.gid();
        let spawned = std::thread::Builder::new()
            .name("socket-conn".into())
            .spawn(move || handle(&hub, &pve, stream, peer, gid, guard));
        if let Err(error) = spawned {
            tracing::warn!(%error, "cannot serve a local connection");
        }
    }
    let _ = std::fs::remove_file(&path);
}
