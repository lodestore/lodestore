//! Database servers of this host, found by their local sockets.
//!
//! This is the one place where the agent, running as root, starts programs of
//! the host: `psql` as the PostgreSQL account over its socket, without a
//! password. Each gets a constant argv with a constant read-only catalog
//! query, a clean environment, `/` as working directory, no stdin, its own
//! process group, a deadline and an output limit. Nothing read from a file or
//! a reply ever reaches an argv.

use std::collections::HashSet;
use std::ffi::{CString, OsStr};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use lodestore_format::source::SourceKey;
use lodestore_proto::agent::v1::{Database, DatabaseKind, DatabaseServer};
use rustix::process::{Pid, Signal, kill_process_group};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use super::{Config, MAX_DATABASES, MAX_SERVERS, clip};
use crate::hooks::terminate;

/// One statement: before PostgreSQL 15 `-c` with several prints only the last
/// result. Every object is schema-qualified and the session `search_path` is
/// `pg_catalog`, so the owner of a database cannot shadow what this reads.
pub(crate) const PG_SQL: &str = "SELECT 's', s.system_identifier::pg_catalog.text, \
    pg_catalog.current_setting('server_version') FROM pg_catalog.pg_control_system() s \
    UNION ALL (SELECT 'd', d.oid::pg_catalog.text, d.datname::pg_catalog.text \
    FROM pg_catalog.pg_database d WHERE d.datallowconn AND NOT d.datistemplate \
    ORDER BY d.oid LIMIT 1001)";
pub(crate) const MAX_COMMAND_OUTPUT: usize = 1 << 20;
pub(crate) const MAX_COMMAND_STDERR: usize = 64 << 10;
pub(crate) const COMMAND_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";
pub(crate) const FS_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_LOCK_BYTES: u64 = 4 << 10;
const MAX_PG_VERSION_BYTES: u64 = 32;
const PG_SOCKET: &str = ".s.PGSQL.";
const PG_ENV: [(&str, &str); 3] = [
    ("PGPASSFILE", "/dev/null"),
    ("PGCLIENTENCODING", "UTF8"),
    ("PGOPTIONS", "-c search_path=pg_catalog"),
];

type Inode = (u64, u64);

/// What the agent keeps between discovery runs.
#[derive(Clone, Default)]
pub(crate) struct ProbeMemo {
    fs_busy: Arc<AtomicBool>,
}

impl ProbeMemo {
    pub(crate) fn new() -> ProbeMemo {
        ProbeMemo::default()
    }
}

struct Postgres {
    dir: PathBuf,
    port: u16,
    version: String,
    #[allow(dead_code)]
    inode: Inode,
}

struct Found {
    pg_account: Option<(u32, u32)>,
    postgres: Vec<Postgres>,
    truncated: bool,
}

/// The database servers of this host and whether the list is incomplete.
pub(crate) async fn scan_databases(
    config: &Config,
    memo: &ProbeMemo,
) -> (Vec<DatabaseServer>, bool) {
    if config.pg_socket_dirs.is_empty() && config.mysql_sockets.is_empty() {
        return (Vec::new(), false);
    }
    let Some(found) = find_candidates(config, memo).await else {
        return (Vec::new(), true);
    };
    let mut truncated = found.truncated;
    let mut servers = Vec::new();
    if let Some(account) = found.pg_account {
        for pg in &found.postgres {
            let (server, cut) = scan_postgres(config, pg, account, config.command_timeout).await;
            truncated |= cut;
            servers.push(server);
        }
    }
    (servers, truncated)
}

struct Busy(Arc<AtomicBool>);

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// All file system work of a run on one blocking thread. A thread stuck on a
/// hung file system is left behind, and no second one starts until it returns.
async fn find_candidates(config: &Config, memo: &ProbeMemo) -> Option<Found> {
    if memo.fs_busy.swap(true, Ordering::AcqRel) {
        tracing::warn!(
            "the previous search for database sockets has not returned; databases are not reported this run"
        );
        return None;
    }
    let busy = Busy(Arc::clone(&memo.fs_busy));
    let config = config.clone();
    let task = tokio::task::spawn_blocking(move || {
        let _busy = busy;
        find(&config)
    });
    match tokio::time::timeout(FS_TIMEOUT, task).await {
        Ok(Ok(found)) => Some(found),
        Ok(Err(error)) => {
            tracing::warn!(%error, "the search for database sockets failed");
            None
        }
        Err(_) => {
            tracing::warn!(
                "the search for database sockets takes too long; databases are not reported this run"
            );
            None
        }
    }
}

fn find(config: &Config) -> Found {
    let mut found = Found {
        pg_account: None,
        postgres: Vec::new(),
        truncated: false,
    };
    if !config.pg_socket_dirs.is_empty() {
        found.pg_account = account(&config.pg_user);
        match found.pg_account {
            Some((uid, _)) => {
                found.truncated |= find_postgres(&config.pg_socket_dirs, uid, &mut found.postgres);
            }
            None => {
                tracing::debug!(user = %config.pg_user, "no such account; PostgreSQL is not probed")
            }
        }
    }
    found
}

/// Sockets of the system directories come first: a port already found there
/// is never taken from a later directory such as `/tmp`.
fn find_postgres(dirs: &[PathBuf], owner: u32, out: &mut Vec<Postgres>) -> bool {
    let mut seen = HashSet::new();
    for dir in dirs {
        for port in socket_ports(dir) {
            if seen.contains(&port) {
                continue;
            }
            let Some(pg) = postgres_candidate(dir, port, owner) else {
                continue;
            };
            if out.len() == MAX_SERVERS {
                return true;
            }
            seen.insert(port);
            out.push(pg);
        }
    }
    false
}

fn socket_ports(dir: &Path) -> Vec<u16> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut ports: Vec<u16> = entries
        .filter_map(Result::ok)
        .filter_map(|e| parse_socket_port(e.file_name().to_str()?))
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// `.s.PGSQL.<port>` with the port spelled exactly as PostgreSQL spells it.
fn parse_socket_port(name: &str) -> Option<u16> {
    let digits = name.strip_prefix(PG_SOCKET)?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let port: u16 = digits.parse().ok()?;
    (port != 0 && port.to_string() == digits).then_some(port)
}

/// Only a socket and a lock file of the PostgreSQL account, neither followed
/// through a symlink: anyone can create files in `/tmp`, and the sticky bit
/// keeps a file of that account from being replaced by another user.
fn postgres_candidate(dir: &Path, port: u16, owner: u32) -> Option<Postgres> {
    let socket = dir.join(format!("{PG_SOCKET}{port}"));
    let lock = dir.join(format!("{PG_SOCKET}{port}.lock"));
    let meta = std::fs::symlink_metadata(&socket).ok()?;
    let lock_ok = std::fs::symlink_metadata(&lock).is_ok_and(|m| m.is_file() && m.uid() == owner);
    if !(meta.file_type().is_socket() && meta.uid() == owner && lock_ok) {
        tracing::debug!(
            socket = %socket.display(),
            "not a socket of the PostgreSQL account with its lock file; skipped"
        );
        return None;
    }
    Some(Postgres {
        dir: dir.to_path_buf(),
        port,
        version: pg_version_from_lock(&lock, owner),
        inode: (meta.dev(), meta.ino()),
    })
}

/// The version in `PG_VERSION` of the data directory named on the second line
/// of the lock file, both owned by the account; empty if unknown.
fn pg_version_from_lock(lock: &Path, owner: u32) -> String {
    read_untrusted(lock, MAX_LOCK_BYTES, owner)
        .and_then(|lock| {
            let data_dir = Path::new(OsStr::from_bytes(lock.split(|&b| b == b'\n').nth(1)?));
            data_dir.is_absolute().then(|| data_dir.join("PG_VERSION"))
        })
        .and_then(|path| read_untrusted(&path, MAX_PG_VERSION_BYTES, owner))
        .and_then(|bytes| {
            let text = String::from_utf8(bytes).ok()?;
            let text = text.strip_suffix('\n').unwrap_or(&text);
            is_version(text).then(|| text.to_owned())
        })
        .unwrap_or_default()
}

fn is_version(text: &str) -> bool {
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    let mut parts = text.split('.');
    parts.next().is_some_and(digits) && parts.next().is_none_or(digits) && parts.next().is_none()
}

/// A file other users may have planted: never through a symlink, never
/// waiting on a FIFO, only a regular file of `owner`, at most `limit` bytes.
fn read_untrusted(path: &Path, limit: u64, owner: u32) -> Option<Vec<u8>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.uid() != owner {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= limit).then_some(bytes)
}

/// The uid and primary gid of a local account.
fn account(name: &str) -> Option<(u32, u32)> {
    let name = CString::new(name).ok()?;
    let mut buf = vec![0u8; 4096];
    loop {
        // SAFETY: an all-zero `passwd` is a valid value (integers and null
        // pointers); it is only read after `getpwnam_r` filled it.
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the call and `buf.len()` is the
        // real size of `buf`; the strings it points into are not used.
        let rc = unsafe {
            libc::getpwnam_r(
                name.as_ptr(),
                &mut pwd,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut out,
            )
        };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        return (rc == 0 && !out.is_null()).then_some((pwd.pw_uid, pwd.pw_gid));
    }
}

async fn scan_postgres(
    config: &Config,
    pg: &Postgres,
    account: (u32, u32),
    timeout: Duration,
) -> (DatabaseServer, bool) {
    let port = pg.port.to_string();
    let mut args: Vec<&OsStr> = [
        "-X",
        "-w",
        "-A",
        "-t",
        "-q",
        "-z",
        "-0",
        "-v",
        "ON_ERROR_STOP=1",
        "-h",
    ]
    .map(OsStr::new)
    .to_vec();
    args.push(pg.dir.as_os_str());
    args.extend(["-p", port.as_str(), "-d", "postgres", "-c", PG_SQL].map(OsStr::new));
    let mut server = DatabaseServer {
        kind: DatabaseKind::Postgres as i32,
        port: u32::from(pg.port),
        version: pg.version.clone(),
        unreachable: true,
        ..DatabaseServer::default()
    };
    let ran = match run(&config.psql, &args, timeout, &PG_ENV, Some(account)).await {
        Ok(ran) if ran.code == Some(0) => ran,
        Ok(ran) => {
            tracing::warn!(port = pg.port, code = ?ran.code, "psql failed; the cluster is reported unreachable");
            return (server, false);
        }
        Err(error) => {
            tracing::warn!(port = pg.port, %error, "psql did not answer; the cluster is reported unreachable");
            return (server, false);
        }
    };
    let Some(reply) = parse_psql(&ran.stdout) else {
        tracing::warn!(
            port = pg.port,
            "psql printed something else than the catalog query result"
        );
        return (server, false);
    };
    server.unreachable = false;
    server.server_id = reply.system_id.to_string();
    server.version = reply.version;
    server.databases = reply.databases;
    (server, reply.truncated)
}

struct PsqlReply {
    system_id: u64,
    version: String,
    databases: Vec<Database>,
    truncated: bool,
}

/// `-A -t -z -0` output: NUL after every field, in triples of a tag (`s` for
/// the one server row, `d` for a database) and two values.
fn parse_psql(out: &[u8]) -> Option<PsqlReply> {
    let out = out.strip_suffix(b"\0").unwrap_or(out);
    let fields: Vec<&[u8]> = out.split(|&b| b == 0).collect();
    let (triples, rest) = fields.as_chunks::<3>();
    if !rest.is_empty() {
        return None;
    }
    let mut server = None;
    let mut rows = Vec::new();
    for &[tag, a, b] in triples {
        match tag {
            b"s" if server.is_none() => server = Some((a, b)),
            b"d" => rows.push((a, b)),
            _ => return None,
        }
    }
    let (id, version) = server?;
    let system_id = pg_system_id(std::str::from_utf8(id).ok()?)?;
    let mut truncated = rows.len() > MAX_DATABASES;
    rows.truncate(MAX_DATABASES);
    let mut databases = Vec::with_capacity(rows.len());
    for (oid, name) in rows {
        let oid: u32 = std::str::from_utf8(oid).ok()?.parse().ok()?;
        if SourceKey::postgres(system_id, oid).is_err() {
            truncated = true;
            continue;
        }
        databases.push(Database {
            name: clip(String::from_utf8_lossy(name).into_owned()),
            oid,
        });
    }
    Some(PsqlReply {
        system_id,
        version: String::from_utf8_lossy(version).into_owned(),
        databases,
        truncated,
    })
}

/// The `system_identifier` of a cluster as `pg_controldata` prints it.
/// `pg_control_system()` returns it as a signed `bigint`; the same 64 bits
/// read as unsigned are the identifier. Every key of a PostgreSQL source is
/// built from this value, and a PostgreSQL backup must name the same instance
/// as discovery does, so the text is turned into the identifier here only.
pub(crate) fn pg_system_id(text: &str) -> Option<u64> {
    text.parse::<i64>().ok().map(i64::cast_unsigned)
}

#[derive(Debug, thiserror::Error)]
enum RunError {
    #[error("cannot be started: {0}")]
    Spawn(io::Error),
    #[error("{0}")]
    Io(io::Error),
    #[error("timed out")]
    Timeout,
    #[error("wrote more than the output limit")]
    TooLarge,
}

struct Ran {
    code: Option<i32>,
    stdout: Vec<u8>,
    #[allow(dead_code)]
    stderr: Vec<u8>,
}

/// Kills the whole process group if the run is dropped before its child was
/// reaped, so a client and anything it started never outlive the agent's
/// interest in them.
struct KillGroupOnDrop(Option<Pid>);

impl Drop for KillGroupOnDrop {
    fn drop(&mut self) {
        if let Some(pgid) = self.0 {
            let _ = kill_process_group(pgid, Signal::KILL);
        }
    }
}

async fn run(
    program: &Path,
    args: &[&OsStr],
    timeout: Duration,
    extra_env: &[(&str, &str)],
    uid_gid: Option<(u32, u32)>,
) -> Result<Ran, RunError> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .env_clear()
        .env("PATH", COMMAND_PATH)
        .env("LC_ALL", "C")
        .envs(extra_env.iter().copied())
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    if let Some((uid, gid)) = uid_gid {
        cmd.uid(uid).gid(gid);
    }
    let mut child = cmd.spawn().map_err(RunError::Spawn)?;
    let mut guard = KillGroupOnDrop(
        child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(Pid::from_raw),
    );
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let done = tokio::time::timeout(timeout, async {
        tokio::try_join!(
            read_limited(stdout, MAX_COMMAND_OUTPUT),
            read_limited(stderr, MAX_COMMAND_STDERR),
            async { child.wait().await.map_err(RunError::Io) },
        )
    })
    .await;
    let result = match done {
        Ok(Ok((stdout, stderr, status))) => Ok(Ran {
            code: status.code(),
            stdout,
            stderr,
        }),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(RunError::Timeout),
    };
    if result.is_err() {
        match guard.0 {
            Some(pgid) => terminate(&mut child, pgid).await,
            None => {
                let _ = child.kill().await;
            }
        }
    }
    guard.0 = None;
    result
}

/// Reads at most `limit` bytes; more is an error and ends the run at once.
async fn read_limited(
    stream: Option<impl AsyncRead + Unpin>,
    limit: usize,
) -> Result<Vec<u8>, RunError> {
    let mut bytes = Vec::new();
    if let Some(stream) = stream {
        stream
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(RunError::Io)?;
    }
    if bytes.len() > limit {
        return Err(RunError::TooLarge);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    use super::*;
    use crate::Paths;
    use crate::discovery::collect;

    const FAKE: &str = r#"#!/bin/sh
f=__DIR__
name=__NAME__
key=
prev=
for a in "$@"; do
  case "$a" in --socket=*) key=$(basename "${a#--socket=}") ;; esac
  [ "$prev" = -p ] && key=$a
  prev=$a
done
n=$$
c="$f/calls/$name.$n"
printf '%s\0' "$@" > "$c.argv"
id -u > "$c.uid"
env > "$c.env"
pwd > "$c.pwd"
cat > "$c.stdin"
for s in "$f/$name.$key.sh" "$f/$name.sh"; do [ -f "$s" ] && . "$s"; done
for o in "$f/$name.$key.out" "$f/$name.out"; do [ -f "$o" ] && { cat "$o"; break; }; done
for e in "$f/$name.$key.err" "$f/$name.err"; do [ -f "$e" ] && { cat "$e" >&2; break; }; done
for x in "$f/$name.$key.code" "$f/$name.code"; do [ -f "$x" ] && exit "$(cat "$x")"; done
exit 0
"#;

    #[allow(dead_code)]
    pub(super) struct Call {
        pub argv: Vec<String>,
        pub uid: u32,
        pub env: Vec<String>,
        pub pwd: String,
        pub stdin: Vec<u8>,
    }

    pub(super) struct Host {
        tmp: tempfile::TempDir,
        pub fake: PathBuf,
        pub config: Config,
        pub memo: ProbeMemo,
        listeners: Vec<UnixListener>,
    }

    fn mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    pub(super) fn user() -> String {
        let out = std::process::Command::new("id")
            .arg("-un")
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    impl Host {
        pub fn new() -> Host {
            let tmp = tempfile::Builder::new()
                .prefix("ls")
                .tempdir_in("/tmp")
                .unwrap();
            let root = tmp.path();
            mode(root, 0o755);
            let fake = root.join("f");
            std::fs::create_dir_all(fake.join("calls")).unwrap();
            mode(&fake, 0o755);
            mode(&fake.join("calls"), 0o777);
            let bin = root.join("bin");
            std::fs::create_dir(&bin).unwrap();
            mode(&bin, 0o755);
            for name in ["psql", "mysql"] {
                let path = bin.join(name);
                let script = FAKE
                    .replace("__DIR__", fake.to_str().unwrap())
                    .replace("__NAME__", name);
                std::fs::write(&path, script).unwrap();
                mode(&path, 0o755);
            }
            let user = user();
            let config = Config {
                pve_root: root.join("no-pve"),
                pg_socket_dirs: Vec::new(),
                mysql_sockets: Vec::new(),
                pg_user: user.clone(),
                mysql_user: user,
                psql: bin.join("psql"),
                mysql: bin.join("mysql"),
                command_timeout: Duration::from_secs(10),
                db_deadline: Duration::from_secs(30),
            };
            Host {
                tmp,
                fake,
                config,
                memo: ProbeMemo::new(),
                listeners: Vec::new(),
            }
        }

        pub fn root(&self) -> &Path {
            self.tmp.path()
        }

        /// A socket directory searched after the ones added before.
        pub fn pg_dir(&mut self, name: &str) -> PathBuf {
            let dir = self.root().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            mode(&dir, 0o755);
            self.config.pg_socket_dirs.push(dir.clone());
            dir
        }

        pub fn bind(&mut self, path: &Path) {
            self.listeners.push(UnixListener::bind(path).unwrap());
        }

        /// A running cluster: its socket and a lock file naming `data_dir`.
        pub fn cluster(&mut self, dir: &Path, port: u16, data_dir: Option<&Path>) {
            self.bind(&dir.join(format!(".s.PGSQL.{port}")));
            let data = data_dir.map_or(String::new(), |d| d.display().to_string());
            let lock = format!("4242\n{data}\n1700000000\n{port}\n{}\n", dir.display());
            std::fs::write(dir.join(format!(".s.PGSQL.{port}.lock")), lock).unwrap();
        }

        /// What the fake `name` prints, or does, for `key` (a port or a
        /// socket file name), or for every call when `key` is empty.
        pub fn set(&self, name: &str, key: &str, what: &str, bytes: &[u8]) {
            let file = if key.is_empty() {
                format!("{name}.{what}")
            } else {
                format!("{name}.{key}.{what}")
            };
            std::fs::write(self.fake.join(file), bytes).unwrap();
        }

        pub fn calls(&self, name: &str) -> Vec<Call> {
            let dir = self.fake.join("calls");
            let mut calls = Vec::new();
            for entry in std::fs::read_dir(&dir).unwrap() {
                let file = entry.unwrap().file_name().into_string().unwrap();
                let Some(stem) = file.strip_suffix(".argv") else {
                    continue;
                };
                if !stem.starts_with(&format!("{name}.")) {
                    continue;
                }
                let read = |ext: &str| {
                    std::fs::read(dir.join(format!("{stem}.{ext}"))).unwrap_or_default()
                };
                let text = |ext: &str| String::from_utf8(read(ext)).unwrap();
                let argv = read("argv");
                let argv = argv.strip_suffix(b"\0").unwrap_or(&argv);
                calls.push(Call {
                    argv: if argv.is_empty() {
                        Vec::new()
                    } else {
                        argv.split(|&b| b == 0)
                            .map(|a| String::from_utf8_lossy(a).into_owned())
                            .collect()
                    },
                    uid: text("uid").trim().parse().unwrap_or(u32::MAX),
                    env: text("env").lines().map(str::to_owned).collect(),
                    pwd: text("pwd").trim().to_owned(),
                    stdin: read("stdin"),
                });
            }
            calls
        }

        pub async fn scan(&self) -> (Vec<DatabaseServer>, bool) {
            scan_databases(&self.config, &self.memo).await
        }

        pub fn paths(&self) -> Paths {
            Paths {
                config_dir: self.root().join("etc"),
                state_dir: self.root().join("state"),
            }
        }
    }

    pub(super) fn psql_out(rows: &[[&[u8]; 3]]) -> Vec<u8> {
        let mut out = Vec::new();
        for row in rows {
            for field in row {
                out.extend_from_slice(field);
                out.push(0);
            }
        }
        out
    }

    pub(super) const SYSID: &str = "7301234567890123456";

    pub(super) fn pg_argv(dir: &Path, port: u16) -> Vec<String> {
        let mut argv: Vec<String> = [
            "-X",
            "-w",
            "-A",
            "-t",
            "-q",
            "-z",
            "-0",
            "-v",
            "ON_ERROR_STOP=1",
            "-h",
        ]
        .map(String::from)
        .to_vec();
        argv.push(dir.display().to_string());
        argv.extend(["-p", &port.to_string(), "-d", "postgres", "-c", PG_SQL].map(String::from));
        argv
    }

    fn db(name: &str, oid: u32) -> Database {
        Database {
            name: name.into(),
            oid,
        }
    }

    pub(super) fn one_cluster() -> (Host, PathBuf) {
        let mut host = Host::new();
        let dir = host.pg_dir("pg");
        host.cluster(&dir, 5432, None);
        host.set(
            "psql",
            "",
            "out",
            &psql_out(&[
                [b"s", SYSID.as_bytes(), b"16.4"],
                [b"d", b"16384", b"app"],
                [b"d", b"16385", b"bil\tling"],
            ]),
        );
        (host, dir)
    }

    #[tokio::test]
    async fn postgres_through_peer_auth() {
        let (host, dir) = one_cluster();
        let (servers, truncated) = host.scan().await;
        assert!(!truncated);
        assert_eq!(
            servers,
            vec![DatabaseServer {
                kind: DatabaseKind::Postgres as i32,
                port: 5432,
                version: "16.4".into(),
                unreachable: false,
                server_id: SYSID.into(),
                databases: vec![db("app", 16384), db("bil\tling", 16385)],
            }]
        );
        let calls = host.calls("psql");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].argv, pg_argv(&dir, 5432));
        assert_eq!(calls[0].uid, account(&host.config.pg_user).unwrap().0);
    }

    #[tokio::test]
    async fn collect_adds_database_servers() {
        let (host, _dir) = one_cluster();
        let facts = collect(&host.config, &host.paths(), &host.memo)
            .await
            .expect("a run without Proxmox succeeds");
        assert_eq!(facts.proxmox, None);
        assert_eq!(facts.databases.len(), 1);
        assert_eq!(facts.databases[0].port, 5432);
        assert_eq!(facts.databases[0].databases.len(), 2);
        assert!(!facts.truncated);
    }

    #[tokio::test]
    async fn proxmox_only_probes_no_databases() {
        let (host, _dir) = one_cluster();
        let mut config = Config::proxmox_only(host.root().join("no-pve"));
        assert!(config.pg_socket_dirs.is_empty() && config.mysql_sockets.is_empty());
        config.psql.clone_from(&host.config.psql);
        config.mysql.clone_from(&host.config.mysql);
        let facts = collect(&config, &host.paths(), &host.memo).await.unwrap();
        assert!(facts.databases.is_empty());
        assert!(host.calls("psql").is_empty() && host.calls("mysql").is_empty());
    }
}
