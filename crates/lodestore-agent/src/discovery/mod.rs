//! Finds the backup sources of this host and reports them to the server with
//! every heartbeat. Discovery only reads files: everything under the Proxmox
//! root is opened with `O_NOFOLLOW` and read up to a fixed size.

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
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::Paths;

pub const DISCOVERY_EVERY: Duration = Duration::from_secs(5 * 60);
pub const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(60);
pub const MAX_GUESTS: usize = 1000;
pub const MAX_GUEST_TAGS: usize = 20;
pub const MAX_NAME_BYTES: usize = 64;
pub const MAX_PVE_FILE_BYTES: u64 = 4 << 20;
pub const MAX_GUEST_CONF_BYTES: u64 = 64 << 10;
pub const MAX_STATE_BYTES: u64 = 1 << 20;

#[derive(Debug, Clone)]
pub struct Config {
    pub pve_root: PathBuf,
}

impl Config {
    pub fn system() -> Config {
        Config {
            pve_root: PathBuf::from("/etc/pve"),
        }
    }

    /// Reads Proxmox from `pve_root` and probes no database servers.
    pub fn proxmox_only(pve_root: PathBuf) -> Config {
        Config { pve_root }
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
/// `pmxcfs` blocks that thread and nothing else.
pub async fn collect(config: &Config, paths: &Paths) -> Result<SourceFacts, ScanFailed> {
    let (root, ids) = (config.pve_root.clone(), paths.source_ids());
    let found = tokio::task::spawn_blocking(move || proxmox::scan(&root, &ids))
        .await
        .map_err(|e| ScanFailed(format!("the discovery thread failed: {e}")))??;
    Ok(SourceFacts {
        age_ms: 0,
        truncated: found.as_ref().is_some_and(|f| f.truncated),
        proxmox: found.map(|f| f.proxmox),
        databases: Vec::new(),
    })
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
        let run = move || -> Run {
            let (config, paths) = (config.clone(), paths.clone());
            Box::pin(async move { collect(&config, &paths).await })
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

    use lodestore_proto::agent::v1::ProxmoxFacts;

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
        let (discovery, _task) =
            Discovery::start(run, Duration::from_secs(10), Duration::from_secs(5), shutdown);
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
}
