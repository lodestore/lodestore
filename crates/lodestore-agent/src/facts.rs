use std::collections::HashSet;
use std::time::Duration;

use lodestore_proto::agent::v1::{Heartbeat, HookFacts, HookScript, Mount, RestoreFacts};
use tokio::task::JoinHandle;

use crate::Paths;
use crate::config::AgentSettings;
use crate::hooks::list_scripts;

const MAX_MOUNTS: usize = 64;
const MAX_FIELD_BYTES: usize = 255;
const COLLECT_TIMEOUT: Duration = Duration::from_secs(5);

const PSEUDO_FS: &[&str] = &[
    "proc",
    "sysfs",
    "devtmpfs",
    "devpts",
    "tmpfs",
    "cgroup",
    "cgroup2",
    "overlay",
    "squashfs",
    "nsfs",
    "tracefs",
    "debugfs",
    "securityfs",
    "pstore",
    "bpf",
    "autofs",
    "mqueue",
    "hugetlbfs",
    "configfs",
    "fusectl",
    "binfmt_misc",
];

fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = (bytes[i] == b'\\' && i + 4 <= bytes.len())
            .then(|| std::str::from_utf8(&bytes[i + 1..i + 4]).ok())
            .flatten()
            .and_then(|d| u8::from_str_radix(d, 8).ok());
        match octal {
            Some(byte) => {
                out.push(byte);
                i += 4;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn mount_points(table: &str) -> Vec<(String, String)> {
    let mut seen = HashSet::new();
    table
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let (_device, path, fs_type) = (fields.next()?, fields.next()?, fields.next()?);
            (!PSEUDO_FS.contains(&fs_type)).then(|| (unescape(path), fs_type.to_string()))
        })
        .filter(|(path, _)| seen.insert(path.clone()))
        .collect()
}

fn clip(mut text: String) -> String {
    if text.len() > MAX_FIELD_BYTES {
        let mut end = MAX_FIELD_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

fn mounts() -> Vec<Mount> {
    let Ok(table) = std::fs::read_to_string("/proc/self/mounts") else {
        return Vec::new();
    };
    mount_points(&table)
        .into_iter()
        .take(MAX_MOUNTS)
        .filter_map(|(path, fs_type)| {
            let stat = rustix::fs::statvfs(path.as_str()).ok()?;
            Some(Mount {
                path: clip(path),
                fs_type: clip(fs_type),
                total_bytes: stat.f_blocks.saturating_mul(stat.f_frsize),
                free_bytes: stat.f_bavail.saturating_mul(stat.f_frsize),
            })
        })
        .collect()
}

fn distro() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                .map(|v| v.trim_matches('"').to_string())
        })
        .unwrap_or_default()
}

pub fn hook_facts(paths: &Paths) -> HookFacts {
    HookFacts {
        allow_inline: AgentSettings::load(paths).hooks.allow_inline,
        scripts: list_scripts(&paths.hooks_dir())
            .into_iter()
            .map(|(name, ok)| HookScript { name, ok })
            .collect(),
    }
}

pub fn restore_facts(paths: &Paths) -> RestoreFacts {
    let settings = AgentSettings::load(paths).restore;
    RestoreFacts {
        roots: settings
            .roots(paths)
            .iter()
            .map(|root| root.to_string_lossy().into_owned())
            .collect(),
        allow_in_place: settings.allow_in_place,
    }
}

pub fn collect() -> Heartbeat {
    let uptime_s = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|t| t.split_whitespace().next()?.parse::<f64>().ok())
        .map(|s| s as i64)
        .unwrap_or(0);
    Heartbeat {
        uptime_s,
        kernel: clip(
            rustix::system::uname()
                .release()
                .to_string_lossy()
                .into_owned(),
        ),
        distro: clip(distro()),
        mounts: mounts(),
        hook_facts: None,
        restore_facts: None,
    }
}

/// Runs `collect` off the async runtime: `statvfs` on a hung network mount can
/// block forever. While a run is stuck the last good result is reused and no
/// second run is started, so a dead mount costs one blocked thread, not one
/// per heartbeat.
pub struct Collector {
    collect: fn() -> Heartbeat,
    timeout: Duration,
    running: Option<JoinHandle<Heartbeat>>,
    last: Heartbeat,
}

impl Default for Collector {
    fn default() -> Self {
        Collector::new(collect, COLLECT_TIMEOUT)
    }
}

impl Collector {
    fn new(collect: fn() -> Heartbeat, timeout: Duration) -> Self {
        Collector {
            collect,
            timeout,
            running: None,
            last: Heartbeat::default(),
        }
    }

    pub async fn next(&mut self) -> Heartbeat {
        let collect = self.collect;
        let running = self
            .running
            .get_or_insert_with(|| tokio::task::spawn_blocking(collect));
        match tokio::time::timeout(self.timeout, running).await {
            Ok(Ok(heartbeat)) => {
                self.running = None;
                self.last = heartbeat;
            }
            Ok(Err(_)) => self.running = None,
            Err(_) => tracing::warn!(
                "collecting host facts is taking too long, probably a hung network mount; sending the previous values"
            ),
        }
        self.last.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use lodestore_proto::agent::v1::Heartbeat;

    use super::{Collector, clip, mount_points, restore_facts};

    #[test]
    fn restore_facts_reports_settings() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::Paths {
            config_dir: tmp.path().join("etc"),
            state_dir: tmp.path().join("var"),
        };
        let facts = restore_facts(&paths);
        assert_eq!(
            facts.roots,
            vec![paths.state_dir.join("restore").to_string_lossy().into_owned()]
        );
        assert!(!facts.allow_in_place);

        std::fs::create_dir_all(&paths.config_dir).unwrap();
        let file = paths.agent_yaml();
        std::fs::write(
            &file,
            "restore:\n  allowed_restore_roots: [/srv/r]\n  allow_in_place: true\n",
        )
        .unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let facts = restore_facts(&paths);
        assert_eq!(facts.roots, vec!["/srv/r".to_string()]);
        assert!(facts.allow_in_place);
    }

    static RUNS: AtomicUsize = AtomicUsize::new(0);

    fn slow() -> Heartbeat {
        RUNS.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(400));
        Heartbeat {
            uptime_s: 42,
            ..Heartbeat::default()
        }
    }

    #[tokio::test]
    async fn a_stuck_collection_does_not_block_or_pile_up() {
        let mut collector = Collector::new(slow, Duration::from_millis(50));
        let started = std::time::Instant::now();
        assert_eq!(collector.next().await.uptime_s, 0);
        assert_eq!(collector.next().await.uptime_s, 0);
        assert!(started.elapsed() < Duration::from_millis(300));
        assert_eq!(RUNS.load(Ordering::SeqCst), 1);

        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(collector.next().await.uptime_s, 42);
    }

    #[test]
    fn long_fields_are_clipped_on_a_char_boundary() {
        assert_eq!(clip("a".repeat(400)).len(), 255);
        let clipped = clip("é".repeat(200));
        assert!(clipped.len() <= 255 && clipped.chars().all(|c| c == 'é'));
    }

    #[test]
    fn pseudo_filesystems_and_duplicates_are_dropped() {
        let table = "/dev/sda1 / ext4 rw 0 0\nproc /proc proc rw 0 0\n\
                     /dev/sdb1 /mnt/with\\040space xfs rw 0 0\n/dev/sda1 / ext4 rw 0 0\n";
        assert_eq!(
            mount_points(table),
            vec![
                ("/".to_string(), "ext4".to_string()),
                ("/mnt/with space".to_string(), "xfs".to_string()),
            ]
        );
    }
}
