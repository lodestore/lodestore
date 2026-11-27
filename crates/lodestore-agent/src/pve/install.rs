//! `lodestore-agent pve install`: puts the storage plugin and the backup
//! provider on this node, records that in pmxcfs and adds the `lodestore`
//! storage once every node of the cluster has the same plugin.

use std::cmp::Ordering;
use std::fmt;
use std::io::{self, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::discovery::proxmox::{
    PLUGIN_FILE, PLUGIN_MARKER_DIR, PROVIDER_FILE, node_name_valid, parse_members,
    pve_manager_version,
};
use crate::discovery::{MAX_PLUGIN_MARKER, MAX_PVE_FILE_BYTES, read_bounded};
use crate::identity::write_atomic;
use crate::pve::{PROVIDER_PROTOCOL, STORAGE_ID};

pub const MIN_PVE_MANAGER: &str = "9.2.33";

const PLUGIN_SOURCE: &str =
    include_str!("../../../../deploy/proxmox/PVE/Storage/Custom/LodestorePlugin.pm");
const PROVIDER_SOURCE: &str =
    include_str!("../../../../deploy/proxmox/PVE/BackupProvider/Plugin/Lodestore.pm");

const SERVICES: [&str; 4] = ["pvedaemon", "pveproxy", "pvestatd", "pvescheduler"];
const POLL: Duration = Duration::from_secs(2);
const PRINT_EVERY: Duration = Duration::from_secs(30);

/// Where the installer reads and writes; tests point them at temporary
/// directories.
#[derive(Debug, Clone)]
pub struct InstallRoots {
    pub pve: PathBuf,
    pub perl: PathBuf,
    pub dpkg_status: PathBuf,
    pub agent_yaml: PathBuf,
}

impl Default for InstallRoots {
    fn default() -> Self {
        InstallRoots {
            pve: PathBuf::from("/etc/pve"),
            perl: PathBuf::from("/usr/share/perl5"),
            dpkg_status: PathBuf::from("/var/lib/dpkg/status"),
            agent_yaml: PathBuf::from("/etc/lodestore/agent.yaml"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct InstallOptions {
    pub allow_guest_in_place: bool,
    pub wait: Duration,
}

/// External programs, always started with an argv vector.
pub trait Commands {
    fn run(&self, argv: &[&str]) -> anyhow::Result<()>;
}

pub struct System;

impl Commands for System {
    fn run(&self, argv: &[&str]) -> anyhow::Result<()> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| anyhow::anyhow!("empty command"))?;
        let output = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| anyhow::anyhow!("{program}: {e}"))?;
        if !output.status.success() {
            anyhow::bail!(
                "{} failed ({}): {}",
                argv.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    NotReady(String),
    #[error("{0}")]
    Failed(String),
}

impl InstallError {
    pub fn exit_code(&self) -> u8 {
        match self {
            InstallError::Failed(_) => 1,
            InstallError::Refused(_) => 2,
            InstallError::NotReady(_) => 3,
        }
    }
}

fn failed(what: impl fmt::Display) -> impl FnOnce(io::Error) -> InstallError {
    move |e| InstallError::Failed(format!("{what}: {e}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeStatus {
    Ready,
    Waiting,
    Outdated(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeState {
    pub name: String,
    pub status: NodeStatus,
}

impl fmt::Display for NodeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.status {
            NodeStatus::Ready => write!(f, "{}: ready", self.name),
            NodeStatus::Waiting => write!(f, "{}: waiting", self.name),
            NodeStatus::Outdated(why) => write!(f, "{}: outdated ({why})", self.name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    pub nodes: Vec<NodeState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    pub agent_version: String,
    pub protocol: u32,
    pub hash: String,
    pub pve_manager: String,
}

impl fmt::Display for Marker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} {} {}",
            self.agent_version, self.protocol, self.hash, self.pve_manager
        )
    }
}

/// `{agent_version} {provider_protocol} {blake3 of both files} {pve_manager}`.
pub fn parse_marker(line: &str) -> Option<Marker> {
    let mut parts = line.split_whitespace();
    let marker = Marker {
        agent_version: parts.next()?.to_string(),
        protocol: parts.next()?.parse().ok()?,
        hash: parts.next()?.to_string(),
        pve_manager: parts.next()?.to_string(),
    };
    parts.next().is_none().then_some(marker)
}

fn plugin_hash(plugin: &[u8], provider: &[u8]) -> String {
    let mut hasher = blake3::Hasher::new();
    for file in [plugin, provider] {
        hasher.update(&(file.len() as u64).to_le_bytes());
        hasher.update(file);
    }
    hasher.finalize().to_hex().to_string()
}

/// The hash of the two Perl files built into this agent.
pub fn embedded_plugin_hash() -> String {
    plugin_hash(PLUGIN_SOURCE.as_bytes(), PROVIDER_SOURCE.as_bytes())
}

fn read_marker(pve: &Path, node: &str) -> io::Result<Option<String>> {
    let path = pve.join(PLUGIN_MARKER_DIR).join(node);
    match read_bounded(&path, MAX_PLUGIN_MARKER as u64) {
        Ok(bytes) => Ok(Some(
            String::from_utf8_lossy(&bytes)
                .lines()
                .next()
                .unwrap_or_default()
                .to_string(),
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The comparison of Debian versions (`dpkg --compare-versions`).
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    fn split(v: &str) -> (u64, &str, &str) {
        let (epoch, rest) = match v.split_once(':') {
            Some((e, rest)) if !e.is_empty() && e.bytes().all(|c| c.is_ascii_digit()) => {
                (e.parse().unwrap_or(u64::MAX), rest)
            }
            _ => (0, v),
        };
        let (upstream, revision) = rest.rsplit_once('-').unwrap_or((rest, ""));
        (epoch, upstream, revision)
    }
    let ((ea, ua, ra), (eb, ub, rb)) = (split(a), split(b));
    ea.cmp(&eb)
        .then_with(|| compare_part(ua.as_bytes(), ub.as_bytes()))
        .then_with(|| compare_part(ra.as_bytes(), rb.as_bytes()))
}

fn compare_part(mut a: &[u8], mut b: &[u8]) -> Ordering {
    let order = |s: &[u8]| match s.first() {
        None => 0,
        Some(c) if c.is_ascii_digit() => 0,
        Some(b'~') => -1,
        Some(c) if c.is_ascii_alphabetic() => i32::from(*c),
        Some(c) => i32::from(*c) + 256,
    };
    let digit = |s: &[u8]| s.first().is_some_and(u8::is_ascii_digit);
    while !a.is_empty() || !b.is_empty() {
        while (!a.is_empty() && !digit(a)) || (!b.is_empty() && !digit(b)) {
            let (oa, ob) = (order(a), order(b));
            if oa != ob {
                return oa.cmp(&ob);
            }
            a = &a[1..];
            b = &b[1..];
        }
        while a.first() == Some(&b'0') {
            a = &a[1..];
        }
        while b.first() == Some(&b'0') {
            b = &b[1..];
        }
        let mut first = Ordering::Equal;
        while digit(a) && digit(b) {
            first = first.then(a[0].cmp(&b[0]));
            a = &a[1..];
            b = &b[1..];
        }
        if digit(a) {
            return Ordering::Greater;
        }
        if digit(b) {
            return Ordering::Less;
        }
        if first != Ordering::Equal {
            return first;
        }
    }
    Ordering::Equal
}

fn new_enough(version: &str) -> bool {
    !version.is_empty() && compare_versions(version, MIN_PVE_MANAGER) != Ordering::Less
}

/// Writes `bytes` unless the file already holds exactly them.
fn put_file(path: &Path, bytes: &[u8], mode: u32) -> Result<bool, InstallError> {
    match read_bounded(path, bytes.len() as u64) {
        Ok(current) if current == bytes => return Ok(false),
        _ => {}
    }
    let dir = path.parent().expect("a file under the root");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(dir)
        .map_err(failed(dir.display()))?;
    write_atomic(path, bytes, mode).map_err(failed(path.display()))?;
    Ok(true)
}

/// pmxcfs takes `mkdir`, writes and `rename`, but sets the modes itself and
/// has no symlinks, so the marker is written without `chmod`.
fn put_marker(pve: &Path, node: &str, line: &str) -> Result<(), InstallError> {
    if read_marker(pve, node).ok().flatten().as_deref() == Some(line) {
        return Ok(());
    }
    let dir = pve.join(PLUGIN_MARKER_DIR);
    std::fs::create_dir_all(&dir).map_err(failed(dir.display()))?;
    let (path, tmp) = (dir.join(node), dir.join(format!(".{node}.tmp")));
    std::fs::write(&tmp, format!("{line}\n")).map_err(failed(tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(failed(path.display()))
}

fn node_state(pve: &Path, node: &str, hash: &str) -> NodeState {
    let status = match read_marker(pve, node) {
        Ok(None) => NodeStatus::Waiting,
        Err(e) => NodeStatus::Outdated(format!("marker cannot be read: {e}")),
        Ok(Some(line)) => match parse_marker(&line) {
            None => NodeStatus::Outdated("marker cannot be parsed".into()),
            Some(m) if m.hash != hash => {
                NodeStatus::Outdated(format!("plugin of agent {}", m.agent_version))
            }
            Some(m) if !new_enough(&m.pve_manager) => {
                NodeStatus::Outdated(format!("pve-manager {}", m.pve_manager))
            }
            Some(_) => NodeStatus::Ready,
        },
    };
    NodeState {
        name: node.to_string(),
        status,
    }
}

/// The id and type of the first section of `storage.cfg` whose id is `id`.
fn storage_section(text: &str, id: &str) -> Option<String> {
    text.lines()
        .filter(|line| !line.starts_with([' ', '\t', '#']))
        .filter_map(|line| line.split_once(':'))
        .find(|(_, name)| name.trim() == id)
        .map(|(kind, _)| kind.trim().to_string())
}

/// Whether `storage.cfg` already has the `lodestore` storage; a section of
/// that name with another type is an error.
fn lodestore_section(pve: &Path) -> Result<bool, InstallError> {
    let text = match read_bounded(&pve.join("storage.cfg"), MAX_PVE_FILE_BYTES) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(failed("storage.cfg")(e)),
    };
    match storage_section(&text, STORAGE_ID) {
        None => Ok(false),
        Some(kind) if kind == STORAGE_ID => Ok(true),
        Some(kind) => Err(InstallError::Failed(format!(
            "storage.cfg already has a storage named {STORAGE_ID} of type {kind}"
        ))),
    }
}

pub fn install(
    roots: &InstallRoots,
    options: &InstallOptions,
    commands: &dyn Commands,
    out: &mut dyn Write,
) -> Result<InstallReport, InstallError> {
    let members = match read_bounded(&roots.pve.join(".members"), MAX_PVE_FILE_BYTES) {
        Ok(bytes) => parse_members(&bytes).map_err(|e| InstallError::Failed(e.0))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(InstallError::Refused(format!(
                "this is not a Proxmox VE node: {} does not exist",
                roots.pve.join(".members").display()
            )));
        }
        Err(e) => return Err(failed(".members")(e)),
    };
    let version = pve_manager_version(&roots.dpkg_status);
    if !new_enough(&version) {
        let found = if version.is_empty() {
            "none installed"
        } else {
            &version
        };
        return Err(InstallError::Refused(format!(
            "Lodestore needs Proxmox VE with pve-manager {MIN_PVE_MANAGER} or newer; this node has {found}"
        )));
    }
    let mut nodes: Vec<String> = members.nodes.into_keys().collect();
    if !nodes.contains(&members.node) {
        nodes.push(members.node.clone());
    }
    nodes.sort();
    if let Some(bad) = nodes.iter().find(|n| !node_name_valid(n)) {
        return Err(InstallError::Failed(format!(
            ".members lists {bad:?}, which is not a valid node name"
        )));
    }

    for (rel, source) in [
        (PLUGIN_FILE, PLUGIN_SOURCE),
        (PROVIDER_FILE, PROVIDER_SOURCE),
    ] {
        put_file(&roots.perl.join(rel), source.as_bytes(), 0o644)?;
    }
    let mut reload = vec!["systemctl", "reload-or-restart"];
    reload.extend(SERVICES);
    commands
        .run(&reload)
        .map_err(|e| InstallError::Failed(e.to_string()))?;
    let hash = embedded_plugin_hash();
    let marker = Marker {
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol: PROVIDER_PROTOCOL,
        hash: hash.clone(),
        pve_manager: version,
    };
    put_marker(&roots.pve, &members.node, &marker.to_string())?;

    let started = Instant::now();
    let mut printed: Option<Instant> = None;
    let states = loop {
        let states: Vec<NodeState> = nodes
            .iter()
            .map(|n| node_state(&roots.pve, n, &hash))
            .collect();
        let ready = states.iter().all(|s| s.status == NodeStatus::Ready);
        let timed_out = started.elapsed() >= options.wait;
        if ready || timed_out || printed.is_none_or(|t| t.elapsed() >= PRINT_EVERY) {
            for state in &states {
                let _ = writeln!(out, "{state}");
            }
            printed = Some(Instant::now());
        }
        if ready {
            break states;
        }
        if timed_out {
            let missing: Vec<&str> = states
                .iter()
                .filter(|s| s.status != NodeStatus::Ready)
                .map(|s| s.name.as_str())
                .collect();
            return Err(InstallError::NotReady(format!(
                "the plugin is not ready on {}: run `lodestore-agent pve install` there; the storage is added once every node has it",
                missing.join(", ")
            )));
        }
        std::thread::sleep(POLL.min(options.wait.saturating_sub(started.elapsed())));
    };

    if lodestore_section(&roots.pve)? {
        return Ok(InstallReport { nodes: states });
    }
    let added = commands.run(&[
        "pvesm",
        "add",
        STORAGE_ID,
        STORAGE_ID,
        "--content",
        "backup",
        "--prune-backups",
        "keep-all=1",
    ]);
    // Another node that waited at the same time may have added it first.
    if let Err(e) = added
        && !lodestore_section(&roots.pve)?
    {
        return Err(InstallError::Failed(e.to_string()));
    }
    Ok(InstallReport { nodes: states })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    pub(super) struct Recorder(Mutex<Vec<Vec<String>>>);

    impl Commands for Recorder {
        fn run(&self, argv: &[&str]) -> anyhow::Result<()> {
            self.0
                .lock()
                .unwrap()
                .push(argv.iter().map(|a| a.to_string()).collect());
            Ok(())
        }
    }

    impl Recorder {
        pub fn pvesm_calls(&self) -> usize {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|argv| argv[0] == "pvesm")
                .count()
        }
    }

    pub(super) struct Cluster {
        _tmp: tempfile::TempDir,
        pub roots: InstallRoots,
    }

    pub(super) fn dpkg_status(version: &str) -> String {
        format!(
            "Package: pve-manager\nStatus: install ok installed\nVersion: {version}\n\nPackage: other\nStatus: install ok installed\nVersion: 1.0\n"
        )
    }

    impl Cluster {
        pub fn new(version: &str) -> Cluster {
            let tmp = tempfile::tempdir().unwrap();
            let roots = InstallRoots {
                pve: tmp.path().join("pve"),
                perl: tmp.path().join("perl5"),
                dpkg_status: tmp.path().join("dpkg-status"),
                agent_yaml: tmp.path().join("etc/agent.yaml"),
            };
            std::fs::create_dir_all(&roots.pve).unwrap();
            let members = serde_json::json!({
                "nodename": "pve-01",
                "version": 5,
                "cluster": { "name": "lab", "version": 2, "nodes": 2, "quorate": 1 },
                "nodelist": {
                    "pve-01": { "id": 1, "online": 1, "ip": "10.0.0.1" },
                    "pve-02": { "id": 2, "online": 1, "ip": "10.0.0.2" },
                },
            });
            std::fs::write(roots.pve.join(".members"), members.to_string()).unwrap();
            std::fs::write(&roots.dpkg_status, dpkg_status(version)).unwrap();
            Cluster { _tmp: tmp, roots }
        }

        pub fn marker(&self, node: &str, line: &str) {
            let dir = self.roots.pve.join(PLUGIN_MARKER_DIR);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(node), format!("{line}\n")).unwrap();
        }

        pub fn run(
            &self,
            wait: Duration,
            commands: &Recorder,
        ) -> (Result<InstallReport, InstallError>, String) {
            let mut out = Vec::new();
            let options = InstallOptions {
                allow_guest_in_place: false,
                wait,
            };
            let result = install(&self.roots, &options, commands, &mut out);
            (result, String::from_utf8(out).unwrap())
        }
    }

    pub(super) fn good_marker(pve_manager: &str) -> String {
        format!(
            "{} {PROVIDER_PROTOCOL} {} {pve_manager}",
            env!("CARGO_PKG_VERSION"),
            embedded_plugin_hash()
        )
    }

    #[test]
    fn pve_install_waits_for_all_nodes() {
        let cluster = Cluster::new("9.2.33");
        let pve = cluster.roots.pve.clone();
        let other = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(1));
            let dir = pve.join(PLUGIN_MARKER_DIR);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("pve-02"), good_marker("9.3.1")).unwrap();
        });
        let commands = Recorder::default();
        let (result, out) = cluster.run(Duration::from_secs(20), &commands);
        other.join().unwrap();
        let report = result.unwrap();

        assert_eq!(
            report
                .nodes
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>(),
            ["pve-01: ready", "pve-02: ready"]
        );
        assert!(out.contains("pve-02: waiting"), "{out}");
        let own =
            std::fs::read_to_string(cluster.roots.pve.join("lodestore/plugin/pve-01")).unwrap();
        assert_eq!(own, format!("{}\n", good_marker("9.2.33")));
        for (rel, source) in [
            (PLUGIN_FILE, PLUGIN_SOURCE),
            (PROVIDER_FILE, PROVIDER_SOURCE),
        ] {
            assert_eq!(
                std::fs::read_to_string(cluster.roots.perl.join(rel)).unwrap(),
                source
            );
        }
        let calls = commands.0.lock().unwrap();
        assert_eq!(
            calls.as_slice(),
            [
                vec![
                    "systemctl",
                    "reload-or-restart",
                    "pvedaemon",
                    "pveproxy",
                    "pvestatd",
                    "pvescheduler"
                ],
                vec![
                    "pvesm",
                    "add",
                    "lodestore",
                    "lodestore",
                    "--content",
                    "backup",
                    "--prune-backups",
                    "keep-all=1"
                ],
            ]
        );
    }

    #[test]
    fn pve_install_times_out() {
        let cluster = Cluster::new("9.2.33");
        let commands = Recorder::default();
        let (result, out) = cluster.run(Duration::from_secs(2), &commands);
        let error = result.unwrap_err();
        assert_eq!(error.exit_code(), 3);
        let text = error.to_string();
        assert!(text.contains("pve-02"), "{text}");
        assert!(!text.contains("pve-01"), "{text}");
        assert!(
            text.contains("run `lodestore-agent pve install` there"),
            "{text}"
        );
        assert!(
            out.contains("pve-01: ready") && out.contains("pve-02: waiting"),
            "{out}"
        );
        assert_eq!(commands.pvesm_calls(), 0);
    }

    #[test]
    fn pve_install_node_old_pve() {
        let cluster = Cluster::new("9.2.33");
        cluster.marker("pve-02", &good_marker("9.2.10"));
        let commands = Recorder::default();
        let (result, out) = cluster.run(Duration::from_secs(2), &commands);
        assert_eq!(result.unwrap_err().exit_code(), 3);
        assert!(
            out.contains("pve-02: outdated (pve-manager 9.2.10)"),
            "{out}"
        );
        assert_eq!(commands.pvesm_calls(), 0);
    }

    #[test]
    fn pve_install_compares_debian_versions() {
        use Ordering::*;
        for (a, b, want) in [
            ("9.2.33", "9.2.33", Equal),
            ("9.2.32", "9.2.33", Less),
            ("9.2.10", "9.2.33", Less),
            ("9.3.1", "9.2.33", Greater),
            ("10.0.0", "9.2.33", Greater),
            ("9.2.33~rc1", "9.2.33", Less),
            ("9.2.33+pve1", "9.2.33", Greater),
            ("1:1.0", "9.2.33", Greater),
            ("9.2.033", "9.2.33", Equal),
        ] {
            assert_eq!(compare_versions(a, b), want, "{a} vs {b}");
        }
    }
}
