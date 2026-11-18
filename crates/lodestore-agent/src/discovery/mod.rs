//! Finds the backup sources of this host and reports them to the server with
//! every heartbeat. Proxmox is read from files: everything under the Proxmox
//! root is opened with `O_NOFOLLOW` and read up to a fixed size. Database
//! servers are asked over their local sockets (see `database`).

mod database;
mod proxmox;

use std::fs::File;
use std::future::Future;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use lodestore_proto::agent::v1::SourceFacts;
use prost::Message;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::Paths;
pub(crate) use database::ProbeMemo;

pub const DISCOVERY_EVERY: Duration = Duration::from_secs(5 * 60);
pub const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(60);
pub const MAX_GUESTS: usize = 1000;
pub const MAX_GUEST_TAGS: usize = 20;
pub const MAX_SERVERS: usize = 16;
pub const MAX_DATABASES: usize = 1000;
pub const MAX_NAME_BYTES: usize = 64;
pub const MAX_VERSION_BYTES: usize = 32;
pub const MAX_FACTS_BYTES: usize = 256 << 10;
pub const MAX_PVE_FILE_BYTES: u64 = 4 << 20;
pub const MAX_GUEST_CONF_BYTES: u64 = 64 << 10;
pub const MAX_SCAN_BYTES: u64 = 32 << 20;
pub const MAX_STATE_BYTES: u64 = 1 << 20;

#[derive(Debug, Clone)]
pub struct Config {
    pub pve_root: PathBuf,
    /// Searched in this order; a port found in an earlier directory wins.
    pub pg_socket_dirs: Vec<PathBuf>,
    pub mysql_sockets: Vec<PathBuf>,
    pub pg_user: String,
    pub mysql_user: String,
    pub psql: PathBuf,
    pub mysql: PathBuf,
    pub command_timeout: Duration,
    pub db_deadline: Duration,
}

impl Config {
    pub fn system() -> Config {
        Config {
            pg_socket_dirs: ["/run/postgresql", "/var/run/postgresql", "/tmp"]
                .map(PathBuf::from)
                .into(),
            mysql_sockets: [
                "/run/mysqld/mysqld.sock",
                "/var/run/mysqld/mysqld.sock",
                "/var/lib/mysql/mysql.sock",
            ]
            .map(PathBuf::from)
            .into(),
            ..Config::proxmox_only(PathBuf::from("/etc/pve"))
        }
    }

    /// Reads Proxmox from `pve_root` and probes no database servers.
    pub fn proxmox_only(pve_root: PathBuf) -> Config {
        Config {
            pve_root,
            pg_socket_dirs: Vec::new(),
            mysql_sockets: Vec::new(),
            pg_user: "postgres".into(),
            mysql_user: "mysql".into(),
            psql: PathBuf::from("/usr/bin/psql"),
            mysql: PathBuf::from("/usr/bin/mysql"),
            command_timeout: Duration::from_secs(10),
            db_deadline: Duration::from_secs(30),
        }
    }
}

/// A run that could not tell what is on the host. The last good result stays.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ScanFailed(pub String);

pub(crate) fn open_nofollow(path: &Path) -> io::Result<File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    Ok(file)
}

pub(crate) fn clip(mut name: String) -> String {
    let mut end = name.len().min(MAX_NAME_BYTES);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name.truncate(end);
    name
}

pub(crate) fn read_bounded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    open_nofollow(path)?
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!("{} is larger than {limit} bytes", path.display()),
        ));
    }
    Ok(bytes)
}

/// One discovery run. Proxmox files are read on a blocking thread: a hung
/// `pmxcfs` blocks that thread and nothing else. Database servers are probed
/// at the same time; they never fail the run, an unknown server is reported
/// unreachable instead.
pub(crate) async fn collect(
    config: &Config,
    paths: &Paths,
    memo: &ProbeMemo,
) -> Result<SourceFacts, ScanFailed> {
    let (root, ids) = (config.pve_root.clone(), paths.source_ids());
    let proxmox = async {
        tokio::task::spawn_blocking(move || proxmox::scan(&root, &ids))
            .await
            .map_err(|e| ScanFailed(format!("the discovery thread failed: {e}")))?
    };
    let (found, (databases, db_truncated)) =
        tokio::join!(proxmox, database::scan_databases(config, memo));
    let found = found?;
    Ok(bound(SourceFacts {
        age_ms: 0,
        truncated: db_truncated || found.as_ref().is_some_and(|f| f.truncated),
        proxmox: found.map(|f| f.proxmox),
        databases,
    }))
}

/// Cuts the list to the limits the server applies and to `MAX_FACTS_BYTES`
/// encoded, always from the tail of the already ordered lists.
fn bound(mut facts: SourceFacts) -> SourceFacts {
    let mut truncated = facts.truncated;
    if let Some(proxmox) = &mut facts.proxmox
        && proxmox.guests.len() > MAX_GUESTS
    {
        proxmox.guests.truncate(MAX_GUESTS);
        truncated = true;
    }
    if facts.databases.len() > MAX_SERVERS {
        facts.databases.truncate(MAX_SERVERS);
        truncated = true;
    }
    let mut left = MAX_DATABASES;
    for server in &mut facts.databases {
        if server.databases.len() > left {
            server.databases.truncate(left);
            truncated = true;
        }
        left -= server.databases.len();
        if server.version.len() > MAX_VERSION_BYTES {
            let mut end = MAX_VERSION_BYTES;
            while !server.version.is_char_boundary(end) {
                end -= 1;
            }
            server.version.truncate(end);
        }
    }
    let mut len = facts.encoded_len();
    while len > MAX_FACTS_BYTES {
        let cut = if let Some(server) = facts.databases.last_mut() {
            match server.databases.pop() {
                Some(db) => prost::encoding::message::encoded_len(6, &db),
                None => facts
                    .databases
                    .pop()
                    .map_or(0, |s| prost::encoding::message::encoded_len(4, &s)),
            }
        } else {
            match facts.proxmox.as_mut().and_then(|p| p.guests.pop()) {
                Some(guest) => prost::encoding::message::encoded_len(3, &guest),
                None => break,
            }
        };
        truncated = true;
        len = len.saturating_sub(cut);
        if len <= MAX_FACTS_BYTES {
            len = facts.encoded_len();
        }
    }
    facts.truncated = truncated;
    facts
}

type Run = Pin<Box<dyn Future<Output = Result<SourceFacts, ScanFailed>> + Send>>;
type Last = Arc<Mutex<Option<(SourceFacts, Instant)>>>;

/// The latest successful discovery result, refreshed in the background. A run
/// that hangs is waited for `timeout` and then left running; no second run
/// starts until it returns, so a stuck `pmxcfs` costs one thread.
#[derive(Clone)]
pub struct Discovery {
    last: Last,
}

impl Discovery {
    pub fn spawn(config: Config, paths: Paths, shutdown: CancellationToken) -> Discovery {
        let memo = ProbeMemo::new();
        let run = move || -> Run {
            let (config, paths, memo) = (config.clone(), paths.clone(), memo.clone());
            Box::pin(async move { collect(&config, &paths, &memo).await })
        };
        Discovery::start(run, DISCOVERY_EVERY, DISCOVERY_TIMEOUT, shutdown).0
    }

    fn start(
        run: impl Fn() -> Run + Send + 'static,
        every: Duration,
        timeout: Duration,
        shutdown: CancellationToken,
    ) -> (Discovery, JoinHandle<()>) {
        let last = Last::default();
        let task = tokio::spawn(schedule(run, every, timeout, shutdown, Arc::clone(&last)));
        (Discovery { last }, task)
    }

    /// `None` until the first run succeeds.
    pub fn current(&self) -> Option<SourceFacts> {
        let last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        last.as_ref().map(|(facts, at)| SourceFacts {
            age_ms: u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX),
            ..facts.clone()
        })
    }
}

async fn schedule(
    run: impl Fn() -> Run,
    every: Duration,
    timeout: Duration,
    shutdown: CancellationToken,
    last: Last,
) {
    let mut running: Option<JoinHandle<Result<SourceFacts, ScanFailed>>> = None;
    loop {
        let handle = running.get_or_insert_with(|| tokio::spawn(run()));
        let done = tokio::select! {
            _ = shutdown.cancelled() => None,
            done = tokio::time::timeout(timeout, &mut *handle) => Some(done),
        };
        match done {
            None => {
                handle.abort();
                return;
            }
            Some(Err(_)) => tracing::warn!(
                "source discovery is taking too long, probably a hung pmxcfs; reporting the previous result"
            ),
            Some(Ok(result)) => {
                running = None;
                match result {
                    Ok(Ok(facts)) => {
                        *last.lock().unwrap_or_else(PoisonError::into_inner) =
                            Some((facts, Instant::now()));
                    }
                    Ok(Err(error)) => tracing::warn!(
                        %error,
                        "source discovery failed; reporting the previous result"
                    ),
                    Err(error) => tracing::warn!(%error, "source discovery failed"),
                }
            }
        }
        tokio::select! {
            _ = shutdown.cancelled() => {
                if let Some(handle) = running {
                    handle.abort();
                }
                return;
            }
            _ = tokio::time::sleep(every) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use lodestore_proto::agent::v1::{
        Database, DatabaseServer, GuestKind, ProxmoxFacts, ProxmoxGuest,
    };

    use super::*;

    fn facts(node: &str) -> SourceFacts {
        SourceFacts {
            proxmox: Some(ProxmoxFacts {
                node: node.into(),
                ..ProxmoxFacts::default()
            }),
            ..SourceFacts::default()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn failed_run_keeps_the_last_result() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let run = move || -> Run {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if n == 0 {
                    Ok(facts("pve1"))
                } else {
                    Err(ScanFailed("no .vmlist".into()))
                }
            })
        };
        let shutdown = CancellationToken::new();
        let (discovery, _task) = Discovery::start(
            run,
            Duration::from_secs(10),
            Duration::from_secs(5),
            shutdown,
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
        let first = discovery.current().expect("the first run succeeded");
        assert_eq!(first.proxmox.as_ref().unwrap().node, "pve1");

        tokio::time::sleep(Duration::from_secs(12)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let after = discovery.current().expect("a failed run keeps the result");
        assert_eq!(after.proxmox, first.proxmox);
        assert!(after.age_ms >= 13_000, "age {}", after.age_ms);

        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(discovery.current().unwrap().age_ms >= after.age_ms + 3_000);
    }

    fn hangs() -> Run {
        Box::pin(std::future::pending())
    }

    #[tokio::test(start_paused = true)]
    async fn current_is_none_before_the_first_run() {
        let (discovery, _task) = Discovery::start(
            hangs,
            Duration::from_secs(10),
            Duration::from_secs(5),
            CancellationToken::new(),
        );
        assert!(discovery.current().is_none());
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert!(discovery.current().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_run_does_not_block_or_pile_up() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let run = move || -> Run {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                Box::pin(async { Ok(facts("pve1")) })
            } else {
                hangs()
            }
        };
        let (discovery, _task) = Discovery::start(
            run,
            Duration::from_secs(10),
            Duration::from_secs(5),
            CancellationToken::new(),
        );
        let clone = discovery.clone();
        tokio::time::sleep(Duration::from_secs(300)).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a hung run is not restarted"
        );
        let started = std::time::Instant::now();
        let current = discovery.current().expect("the first run succeeded");
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(current.proxmox.unwrap().node, "pve1");
        assert_eq!(clone.current(), discovery.current());
    }

    #[tokio::test]
    async fn discovery_stops_on_shutdown() {
        // waiting for the hung run, then sleeping after its timeout
        for timeout in [Duration::from_secs(60), Duration::from_millis(50)] {
            let shutdown = CancellationToken::new();
            let (_discovery, task) =
                Discovery::start(hangs, Duration::from_secs(60), timeout, shutdown.clone());
            tokio::time::sleep(Duration::from_millis(100)).await;
            shutdown.cancel();
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .expect("the discovery task ends on shutdown")
                .unwrap();
        }
    }

    fn guest(vmid: u32, name: &str, tags: Vec<String>) -> ProxmoxGuest {
        ProxmoxGuest {
            kind: GuestKind::Ct as i32,
            vmid,
            name: name.into(),
            uuid: "0192f0c4-7d3a-7000-8000-000000000001".into(),
            pool: String::new(),
            tags,
        }
    }

    fn with_guests(guests: Vec<ProxmoxGuest>) -> SourceFacts {
        SourceFacts {
            proxmox: Some(ProxmoxFacts {
                cluster: String::new(),
                node: "pve1".into(),
                guests,
            }),
            ..SourceFacts::default()
        }
    }

    fn vmids(facts: &SourceFacts) -> Vec<u32> {
        let guests = &facts.proxmox.as_ref().unwrap().guests;
        guests.iter().map(|g| g.vmid).collect()
    }

    #[test]
    fn bound_cuts_and_marks_truncated() {
        let many = (100..1600).map(|v| guest(v, "g", Vec::new())).collect();
        let cut = bound(with_guests(many));
        assert!(cut.truncated);
        assert_eq!(vmids(&cut), (100..1100).collect::<Vec<_>>());

        let tags: Vec<String> = (0..20).map(|i| format!("{i:0>64}")).collect();
        let big = (100..1100)
            .map(|v| guest(v, &"n".repeat(64), tags.clone()))
            .collect();
        let cut = bound(with_guests(big));
        assert!(cut.truncated);
        assert!(cut.encoded_len() <= MAX_FACTS_BYTES);
        let kept = vmids(&cut);
        assert!(!kept.is_empty() && kept.len() < 1000);
        assert_eq!(kept, (100..100 + kept.len() as u32).collect::<Vec<_>>());

        let servers = (0..20)
            .map(|port| DatabaseServer {
                port,
                version: "16.4".into(),
                databases: (1..=1500)
                    .map(|oid| Database {
                        name: format!("db{oid}"),
                        oid,
                    })
                    .collect(),
                ..DatabaseServer::default()
            })
            .collect();
        let cut = bound(SourceFacts {
            databases: servers,
            ..SourceFacts::default()
        });
        assert!(cut.truncated);
        assert_eq!(cut.databases.len(), 16);
        let total: usize = cut.databases.iter().map(|s| s.databases.len()).sum();
        assert!(total <= MAX_DATABASES, "{total}");

        let small = with_guests(vec![guest(101, "web", vec!["a".into()])]);
        assert_eq!(bound(small.clone()), small);
        assert!(!bound(small).truncated);
    }

    #[test]
    fn read_bounded_never_reads_past_the_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let huge = tmp.path().join("huge");
        File::create(&huge).unwrap().set_len(1 << 40).unwrap();
        let error = read_bounded(&huge, MAX_PVE_FILE_BYTES).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);

        let exact = tmp.path().join("exact");
        std::fs::write(&exact, vec![b'x'; 4096]).unwrap();
        assert_eq!(read_bounded(&exact, 4096).unwrap().len(), 4096);
        assert_eq!(
            read_bounded(&exact, 4095).unwrap_err().kind(),
            io::ErrorKind::FileTooLarge
        );

        let error = read_bounded(Path::new("/dev/zero"), 16).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&exact, &link).unwrap();
        let error = read_bounded(&link, 4096).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ELOOP));
    }
}
