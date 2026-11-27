//! The `pve_*` operations of the agent's local socket. The Perl provider of
//! the Proxmox VE storage plugin is only a transport: it forwards what vzdump
//! and `pct restore` tell it, and the agent does the work. Every operation
//! needs a peer with the required uid (root in production), the token of a job
//! the agent started and, for the processes the agent spawned, a peer that
//! descends from that process.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::sync::{mpsc, oneshot};

use super::{MIN_PROVIDER_PROTOCOL, PROVIDER_PROTOCOL};
use crate::socket::FIRST_LINE_TIMEOUT;

pub const MAX_PVE_LINE: usize = 256 << 10;
pub const MAX_PVE_SESSIONS: usize = 8;
const TOKEN_BYTES: usize = 32;
const TOKEN_HEX: usize = TOKEN_BYTES * 2;
const MAX_ANCESTORS: usize = 16;
const MAX_PATH: usize = 4096;
const REPLY_WAIT: Duration = Duration::from_secs(10);
const PROVIDER_UPDATE_HINT: &str = "run lodestore-agent pve install on this node";

const OPS: [&str; 7] = [
    "pve_job_check",
    "pve_ct_open",
    "pve_ct_data",
    "pve_ct_finish",
    "pve_restore_config",
    "pve_restore_open",
    "pve_restore_close",
];

/// The secret that ties a provider call to a job the agent started. It is
/// never printed.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    fn generate() -> Token {
        let mut bytes = [0u8; TOKEN_BYTES];
        getrandom::fill(&mut bytes).expect("the operating system has no random bytes");
        Token(data_encoding::HEXLOWER.encode(&bytes))
    }

    /// The value for the job's environment. Callers must not log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

/// What vzdump passes to the provider as `$info`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Info {
    pub directory: String,
    pub sources: Vec<String>,
    pub backup_user_id: Option<u32>,
    pub bandwidth_limit: Option<u64>,
    pub firewall_config: Option<String>,
    pub guest_uid: Option<String>,
    pub guest_rtime: Option<i64>,
    pub cluster_id: Option<String>,
}

/// An event of one container backup, in the order vzdump produces them. The
/// agent answers the provider when the receiver sends on `reply`.
#[derive(Debug)]
pub enum BackupEvent {
    Open {
        info: Info,
        peer_pid: u32,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Data {
        config: Vec<u8>,
        excludes: Vec<String>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Finish {
        success: bool,
        error: Option<String>,
        reply: oneshot::Sender<Result<u64, String>>,
    },
    /// The provider's connection ended before `Finish`.
    Aborted,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RestoreEvent {
    Opened,
    Closed,
}

pub struct BackupSession {
    pub events: mpsc::Receiver<BackupEvent>,
    _registration: Registration,
}

pub struct RestoreSession {
    pub events: mpsc::Receiver<RestoreEvent>,
    _registration: Registration,
}

struct Registration {
    sessions: Arc<PveSessions>,
    id: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.sessions.state().entries.remove(&self.id);
    }
}

#[derive(Clone)]
enum Kind {
    Backup {
        vmid: u32,
        opened: bool,
        tx: mpsc::Sender<BackupEvent>,
    },
    Restore {
        restore_id: String,
        config: Arc<Vec<u8>>,
        directory: PathBuf,
        tx: mpsc::Sender<RestoreEvent>,
    },
}

#[derive(Clone)]
struct Entry {
    token: [u8; TOKEN_HEX],
    spawned: Option<u32>,
    kind: Kind,
}

#[derive(Default)]
struct State {
    next: u64,
    entries: HashMap<u64, Entry>,
}

pub struct PveSessions {
    required_uid: u32,
    first_line: Duration,
    live: AtomicUsize,
    state: Mutex<State>,
}

pub(crate) struct Peer {
    pub uid: u32,
    pub pid: Option<u32>,
}

struct Slot(Arc<PveSessions>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
struct Rejection {
    code: &'static str,
    message: String,
}

fn reject(code: &'static str, message: impl Into<String>) -> Rejection {
    Rejection {
        code,
        message: message.into(),
    }
}

enum Flow {
    Continue,
    Close,
}

impl PveSessions {
    /// Operations are served only to peers whose uid is `required_uid`.
    pub fn new(required_uid: u32) -> PveSessions {
        PveSessions {
            required_uid,
            first_line: FIRST_LINE_TIMEOUT,
            live: AtomicUsize::new(0),
            state: Mutex::new(State::default()),
        }
    }

    #[must_use]
    pub fn with_first_line_timeout(mut self, wait: Duration) -> PveSessions {
        self.first_line = wait;
        self
    }

    pub(crate) fn first_line_timeout(&self) -> Duration {
        self.first_line
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn insert(self: &Arc<Self>, kind: Kind) -> (Token, Registration) {
        let token = Token::generate();
        let mut bytes = [0u8; TOKEN_HEX];
        bytes.copy_from_slice(token.0.as_bytes());
        let mut state = self.state();
        state.next += 1;
        let id = state.next;
        state.entries.insert(
            id,
            Entry {
                token: bytes,
                spawned: None,
                kind,
            },
        );
        drop(state);
        (
            token,
            Registration {
                sessions: Arc::clone(self),
                id,
            },
        )
    }

    /// A backup of guest `vmid`. The token lives as long as the session.
    pub fn register_backup(self: &Arc<Self>, vmid: u32) -> (Token, BackupSession) {
        let (tx, events) = mpsc::channel(4);
        let (token, registration) = self.insert(Kind::Backup {
            vmid,
            opened: false,
            tx,
        });
        (
            token,
            BackupSession {
                events,
                _registration: registration,
            },
        )
    }

    /// A restore: the provider hands PVE exactly `config` and `directory`.
    pub fn register_restore(
        self: &Arc<Self>,
        restore_id: &str,
        config: Vec<u8>,
        directory: PathBuf,
    ) -> (Token, RestoreSession) {
        let (tx, events) = mpsc::channel(8);
        let (token, registration) = self.insert(Kind::Restore {
            restore_id: restore_id.to_string(),
            config: Arc::new(config),
            directory,
            tx,
        });
        (
            token,
            RestoreSession {
                events,
                _registration: registration,
            },
        )
    }

    /// Records the process the agent started for this job; the provider's
    /// peers must descend from it.
    pub fn set_spawned_pid(&self, token: &Token, pid: u32) {
        if let Some(id) = self.find(token.expose()).map(|(id, _)| id)
            && let Some(entry) = self.state().entries.get_mut(&id)
        {
            entry.spawned = Some(pid);
        }
    }

    /// Looks the token up without stopping at the first match.
    fn find(&self, token: &str) -> Option<(u64, Entry)> {
        if !is_token(token) {
            return None;
        }
        let state = self.state();
        let mut found = None;
        for (id, entry) in &state.entries {
            if token_eq(token, &entry.token) {
                found = Some((*id, entry.clone()));
            }
        }
        found
    }

    fn mark_opened(&self, id: u64) -> bool {
        let mut state = self.state();
        match state.entries.get_mut(&id).map(|e| &mut e.kind) {
            Some(Kind::Backup { opened, .. }) if !*opened => {
                *opened = true;
                true
            }
            _ => false,
        }
    }

    fn acquire(self: &Arc<Self>) -> Option<Slot> {
        if self.live.fetch_add(1, Ordering::AcqRel) >= MAX_PVE_SESSIONS {
            self.live.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(Slot(Arc::clone(self)))
    }

    /// Serves one provider connection whose first line is `first`. Blocks
    /// until the connection ends.
    pub(crate) fn serve_connection(
        self: &Arc<Self>,
        stream: UnixStream,
        first: Vec<u8>,
        peer: Peer,
    ) {
        let _ = stream.set_write_timeout(Some(REPLY_WAIT));
        if peer.uid != self.required_uid {
            tracing::warn!(
                code = "forbidden",
                uid = peer.uid,
                pid = peer.pid,
                "refused a Proxmox provider request"
            );
            send(
                &stream,
                &failure("forbidden", "only root may use this operation"),
            );
            return;
        }
        let Some(_slot) = self.acquire() else {
            send(
                &stream,
                &failure("busy", "too many provider sessions; try again later"),
            );
            return;
        };
        let Ok(read) = stream.try_clone() else {
            return;
        };
        let _ = stream.set_read_timeout(None);
        let mut conn = Conn {
            sessions: self,
            stream,
            reader: BufReader::new(read),
            peer,
            backup: None,
        };
        conn.run(first);
    }
}

pub(crate) fn is_token(text: &str) -> bool {
    text.len() == TOKEN_HEX && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Compares in constant time for a candidate of the right length.
pub(crate) fn token_eq(candidate: &str, token: &[u8; TOKEN_HEX]) -> bool {
    candidate.len() == TOKEN_HEX && bool::from(candidate.as_bytes().ct_eq(token))
}

/// Whether `pid` is `ancestor` or one of its descendants, by the parent
/// chain in `/proc`.
fn descends(mut pid: u32, ancestor: u32) -> bool {
    for _ in 0..MAX_ANCESTORS {
        if pid == ancestor {
            return true;
        }
        if pid <= 1 {
            return false;
        }
        let Some(parent) = parent_of(pid) else {
            return false;
        };
        pid = parent;
    }
    false
}

fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = &stat[stat.rfind(')')? + 1..];
    after_name.split_whitespace().nth(1)?.parse().ok()
}

fn failure(code: &str, message: &str) -> Value {
    json!({"ok": false, "code": code, "message": message})
}

fn send(mut stream: &UnixStream, value: &Value) {
    let mut line = value.to_string();
    line.push('\n');
    let _ = stream.write_all(line.as_bytes());
}

struct Link {
    id: u64,
    tx: mpsc::Sender<BackupEvent>,
    data_seen: bool,
    finished: bool,
}

struct Conn<'a> {
    sessions: &'a Arc<PveSessions>,
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    peer: Peer,
    backup: Option<Link>,
}

impl Conn<'_> {
    fn run(&mut self, first: Vec<u8>) {
        let mut line = first;
        while let Flow::Continue = self.message(&line) {
            line = match self.next_line() {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(r) => {
                    send(&self.stream, &failure(r.code, &r.message));
                    break;
                }
            };
        }
        if let Some(link) = &self.backup
            && !link.finished
        {
            let _ = link.tx.try_send(BackupEvent::Aborted);
        }
    }

    fn next_line(&mut self) -> Result<Option<Vec<u8>>, Rejection> {
        let mut line = Vec::new();
        let mut limited = (&mut self.reader).take(MAX_PVE_LINE as u64 + 1);
        let read = limited.read_until(b'\n', &mut line);
        match read {
            Ok(0) | Err(_) => return Ok(None),
            Ok(_) => {}
        }
        if line.last() == Some(&b'\n') {
            line.pop();
            return Ok(Some(line));
        }
        if line.len() > MAX_PVE_LINE {
            return Err(reject(
                "bad_request",
                format!("a line is longer than {MAX_PVE_LINE} bytes"),
            ));
        }
        Ok(None)
    }

    fn message(&mut self, line: &[u8]) -> Flow {
        match self.dispatch(line) {
            Ok(flow) => flow,
            Err(r) => {
                if matches!(
                    r.code,
                    "forbidden" | "bad_token" | "wrong_guest" | "wrong_process"
                ) {
                    tracing::warn!(
                        code = r.code,
                        uid = self.peer.uid,
                        pid = self.peer.pid,
                        "refused a Proxmox provider request"
                    );
                }
                send(&self.stream, &failure(r.code, &r.message));
                Flow::Close
            }
        }
    }

    fn dispatch(&mut self, line: &[u8]) -> Result<Flow, Rejection> {
        if self.peer.uid != self.sessions.required_uid {
            return Err(reject("forbidden", "only root may use this operation"));
        }
        let value: Value = serde_json::from_slice(line)
            .map_err(|e| reject("bad_request", format!("the request is not valid: {e}")))?;
        let version = value
            .get("v")
            .and_then(Value::as_u64)
            .ok_or_else(|| reject("bad_request", "the request has no protocol version v"))?;
        if !(u64::from(MIN_PROVIDER_PROTOCOL)..=u64::from(PROVIDER_PROTOCOL)).contains(&version) {
            return Err(reject(
                "protocol_mismatch",
                format!(
                    "the provider speaks protocol {version}, this agent supports {MIN_PROVIDER_PROTOCOL} to {PROVIDER_PROTOCOL}: {PROVIDER_UPDATE_HINT}"
                ),
            ));
        }
        let op = value.get("op").and_then(Value::as_str).unwrap_or_default();
        if !OPS.contains(&op) {
            return Err(reject("unsupported", "unknown operation"));
        }
        let request: Request = serde_json::from_value(value)
            .map_err(|e| reject("bad_request", format!("the request is not valid: {e}")))?;
        self.handle(request)
    }

    fn authorize(&self, token: &str) -> Result<(u64, Entry), Rejection> {
        let (id, entry) = self
            .sessions
            .find(token)
            .ok_or_else(|| reject("bad_token", "the job token is not valid"))?;
        let process_ok = match entry.spawned {
            Some(ancestor) => self.peer.pid.is_some_and(|pid| descends(pid, ancestor)),
            None => self.sessions.required_uid != super::ROOT_UID,
        };
        if !process_ok {
            return Err(reject("wrong_process", "the caller is not part of the job"));
        }
        Ok((id, entry))
    }

    fn handle(&mut self, request: Request) -> Result<Flow, Rejection> {
        match request {
            Request::PveJobCheck { token } => {
                self.authorize(&token)?;
                send(&self.stream, &json!({"ok": true}));
                Ok(Flow::Close)
            }
            Request::PveCtOpen { token, vmid, info } => self.open(&token, vmid, info),
            Request::PveCtData {
                token,
                config,
                excludes,
            } => self.data(&token, config, excludes),
            Request::PveCtFinish {
                token,
                success,
                error,
            } => self.finish(&token, success, error),
            Request::PveRestoreConfig { token, restore_id } => {
                let (_, entry) = self.authorize(&token)?;
                let (config, _, _) = restore_parts(&entry, &restore_id)?;
                let text = std::str::from_utf8(&config)
                    .map_err(|_| reject("bad_request", "the configuration is not UTF-8 text"))?;
                send(&self.stream, &json!({"ok": true, "config": text}));
                Ok(Flow::Close)
            }
            Request::PveRestoreOpen { token, restore_id } => {
                let (_, entry) = self.authorize(&token)?;
                let (_, directory, tx) = restore_parts(&entry, &restore_id)?;
                let _ = tx.try_send(RestoreEvent::Opened);
                send(
                    &self.stream,
                    &json!({"ok": true, "directory": directory.to_string_lossy()}),
                );
                Ok(Flow::Close)
            }
            Request::PveRestoreClose { token, restore_id } => {
                let (_, entry) = self.authorize(&token)?;
                let (_, _, tx) = restore_parts(&entry, &restore_id)?;
                let _ = tx.try_send(RestoreEvent::Closed);
                send(&self.stream, &json!({"ok": true}));
                Ok(Flow::Close)
            }
        }
    }

    fn open(&mut self, token: &str, vmid: u32, info: Info) -> Result<Flow, Rejection> {
        if self.backup.is_some() {
            return Err(reject("bad_request", "this connection is already open"));
        }
        let (id, entry) = self.authorize(token)?;
        let Kind::Backup {
            vmid: owned, tx, ..
        } = entry.kind
        else {
            return Err(reject("wrong_guest", "the token is not for a backup"));
        };
        if owned != vmid {
            return Err(reject("wrong_guest", "the token is for another guest"));
        }
        check_info(&info)?;
        let peer_pid = self
            .peer
            .pid
            .ok_or_else(|| reject("wrong_process", "the caller's process is unknown"))?;
        if !self.sessions.mark_opened(id) {
            return Err(reject("bad_token", "the job token was already used"));
        }
        let (reply, answer) = oneshot::channel();
        tx.blocking_send(BackupEvent::Open {
            info,
            peer_pid,
            reply,
        })
        .map_err(|_| reject("aborted", "the backup is no longer running"))?;
        wait(answer)?.map_err(|m| reject("refused", m))?;
        self.backup = Some(Link {
            id,
            tx,
            data_seen: false,
            finished: false,
        });
        send(&self.stream, &json!({"ok": true}));
        Ok(Flow::Continue)
    }

    fn data(
        &mut self,
        token: &str,
        config: String,
        excludes: Vec<String>,
    ) -> Result<Flow, Rejection> {
        let tx = self.link_for(token, true)?;
        let (reply, answer) = oneshot::channel();
        tx.blocking_send(BackupEvent::Data {
            config: config.into_bytes(),
            excludes,
            reply,
        })
        .map_err(|_| reject("aborted", "the backup is no longer running"))?;
        wait(answer)?.map_err(|m| reject("refused", m))?;
        if let Some(link) = &mut self.backup {
            link.data_seen = true;
        }
        send(&self.stream, &json!({"ok": true}));
        Ok(Flow::Continue)
    }

    fn finish(
        &mut self,
        token: &str,
        success: bool,
        error: Option<String>,
    ) -> Result<Flow, Rejection> {
        let tx = self.link_for(token, false)?;
        if let Some(link) = &mut self.backup {
            link.finished = true;
        }
        let (reply, answer) = oneshot::channel();
        tx.blocking_send(BackupEvent::Finish {
            success,
            error,
            reply,
        })
        .map_err(|_| reject("aborted", "the backup is no longer running"))?;
        let size = wait(answer)?.map_err(|m| reject("refused", m))?;
        send(&self.stream, &json!({"ok": true, "archive_size": size}));
        Ok(Flow::Close)
    }

    /// The channel of the open backup, after checking that this message
    /// belongs to it and arrives in order.
    fn link_for(&self, token: &str, data: bool) -> Result<mpsc::Sender<BackupEvent>, Rejection> {
        let Some(link) = &self.backup else {
            return Err(reject("bad_request", "pve_ct_open must come first"));
        };
        let (id, _) = self.authorize(token)?;
        if id != link.id {
            return Err(reject("bad_token", "the token is not for this connection"));
        }
        if data && link.data_seen {
            return Err(reject("bad_request", "pve_ct_data was already sent"));
        }
        Ok(link.tx.clone())
    }
}

fn wait<T>(answer: oneshot::Receiver<T>) -> Result<T, Rejection> {
    answer
        .blocking_recv()
        .map_err(|_| reject("aborted", "the backup is no longer running"))
}

type RestoreParts = (Arc<Vec<u8>>, PathBuf, mpsc::Sender<RestoreEvent>);

fn restore_parts(entry: &Entry, restore_id: &str) -> Result<RestoreParts, Rejection> {
    match &entry.kind {
        Kind::Restore {
            restore_id: owned,
            config,
            directory,
            tx,
        } if owned == restore_id => Ok((Arc::clone(config), directory.clone(), tx.clone())),
        _ => Err(reject("wrong_guest", "the token is not for this restore")),
    }
}

fn check_info(info: &Info) -> Result<(), Rejection> {
    let relative = |p: &str| {
        p.len() <= MAX_PATH
            && !p.contains('\0')
            && !p.starts_with('/')
            && !p.split('/').any(|c| c == "..")
    };
    let absolute = |p: &str| {
        p.len() <= MAX_PATH
            && p.starts_with('/')
            && !p.contains('\0')
            && !p.split('/').any(|c| c == "..")
    };
    if !absolute(&info.directory)
        || info.sources.is_empty()
        || !info.sources.iter().all(|s| relative(s))
    {
        return Err(reject(
            "bad_request",
            "directory must be an absolute path and sources relative paths, without ..",
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)]
enum Request {
    PveJobCheck {
        token: String,
    },
    PveCtOpen {
        token: String,
        vmid: u32,
        info: Info,
    },
    PveCtData {
        token: String,
        config: String,
        #[serde(default)]
        excludes: Vec<String>,
    },
    PveCtFinish {
        token: String,
        success: bool,
        #[serde(default)]
        error: Option<String>,
    },
    PveRestoreConfig {
        token: String,
        restore_id: String,
    },
    PveRestoreOpen {
        token: String,
        restore_id: String,
    },
    PveRestoreClose {
        token: String,
        restore_id: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token_bytes(text: &str) -> [u8; TOKEN_HEX] {
        let mut bytes = [0u8; TOKEN_HEX];
        bytes.copy_from_slice(text.as_bytes());
        bytes
    }

    #[test]
    fn token_eq_compares_whole_tokens() {
        let token = "ab".repeat(32);
        assert!(token_eq(&token, &token_bytes(&token)));
        let near = format!("{}c", &token[..63]);
        assert!(!token_eq(&near, &token_bytes(&token)));
        assert!(!token_eq(&token[..63], &token_bytes(&token)));
        assert!(!token_eq("", &token_bytes(&token)));
    }

    #[test]
    fn is_token_wants_64_lowercase_hex() {
        assert!(is_token(&"0f".repeat(32)));
        assert!(!is_token(&"0f".repeat(31)));
        assert!(!is_token(&"0F".repeat(32)));
        assert!(!is_token(&"zz".repeat(32)));
    }

    #[test]
    fn debug_hides_the_token() {
        let token = Token::generate();
        assert!(!format!("{token:?}").contains(token.expose()));
        assert!(is_token(token.expose()));
    }

    #[test]
    fn non_root_is_refused_before_taking_a_slot() {
        let sessions = Arc::new(PveSessions::new(0));
        sessions.live.store(MAX_PVE_SESSIONS, Ordering::Release);
        let (ours, theirs) = UnixStream::pair().unwrap();
        let first = br#"{"v":1,"op":"pve_job_check","token":"x"}"#.to_vec();
        sessions.serve_connection(ours, first, Peer { uid: 1, pid: None });
        let mut reply = String::new();
        BufReader::new(theirs).read_line(&mut reply).unwrap();
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["code"], "forbidden", "{reply}");
        assert_eq!(sessions.live.load(Ordering::Acquire), MAX_PVE_SESSIONS);
    }

    #[test]
    fn info_rejects_paths_that_leave_the_snapshot() {
        let info = |dir: &str, src: &str| Info {
            directory: dir.into(),
            sources: vec![src.into()],
            backup_user_id: None,
            bandwidth_limit: None,
            firewall_config: None,
            guest_uid: None,
            guest_rtime: None,
            cluster_id: None,
        };
        assert!(check_info(&info("/mnt/vzsnap0", "./")).is_ok());
        assert!(check_info(&info("mnt/vzsnap0", "./")).is_err());
        assert!(check_info(&info("/mnt/../etc", "./")).is_err());
        assert!(check_info(&info("/mnt/vzsnap0", "/etc")).is_err());
        assert!(check_info(&info("/mnt/vzsnap0", "../x")).is_err());
    }
}
