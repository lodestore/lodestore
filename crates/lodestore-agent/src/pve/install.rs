//! `lodestore-agent pve install`: puts the storage plugin and the backup
//! provider on this node, records that in pmxcfs and adds the `lodestore`
//! storage once every node of the cluster has the same plugin.

use std::cmp::Ordering;
use std::fmt;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::discovery::proxmox::{
    PLUGIN_FILE, PLUGIN_MARKER_DIR, PROVIDER_FILE, node_name_valid, parse_members,
    pve_manager_version,
};
use crate::discovery::{MAX_PLUGIN_MARKER, MAX_PVE_FILE_BYTES, read_bounded};
use crate::identity::{write_atomic, write_private_file};
use crate::pve::{MIN_PROVIDER_PROTOCOL, PROVIDER_PROTOCOL, STORAGE_ID};
use crate::update::switch::SERVICE;

pub const MIN_PVE_MANAGER: &str = "9.2.33";

pub(crate) const PLUGIN_SOURCE: &str =
    include_str!("../../../../deploy/proxmox/PVE/Storage/Custom/LodestorePlugin.pm");
pub(crate) const PROVIDER_SOURCE: &str =
    include_str!("../../../../deploy/proxmox/PVE/BackupProvider/Plugin/Lodestore.pm");

const SERVICES: [&str; 4] = ["pvedaemon", "pveproxy", "pvestatd", "pvescheduler"];
const MAX_PERL_FILE: u64 = 1 << 20;
const MAX_AGENT_YAML: u64 = 1 << 20;
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

pub(crate) fn plugin_hash(plugin: &[u8], provider: &[u8]) -> String {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginHealth {
    /// The files of this agent, recorded in the marker.
    Ok,
    /// Recorded in the marker and working, but not the files of this agent.
    Outdated,
    /// A file or the marker of this node is not there.
    Missing,
    /// The files are not the ones the marker records.
    Changed,
    /// The marker names a provider protocol this agent does not speak.
    Unsupported,
}

impl PluginHealth {
    /// Whether vzdump can use the provider on this node.
    pub fn usable(self) -> bool {
        matches!(self, PluginHealth::Ok | PluginHealth::Outdated)
    }
}

/// The state of the plugin on `node`, from the Perl files under `perl` and
/// the marker under `pve`. Reads files only, so discovery and every container
/// job can call it.
pub fn plugin_health(pve: &Path, perl: &Path, node: &str) -> PluginHealth {
    let (Ok(plugin), Ok(provider)) = (
        read_bounded(&perl.join(PLUGIN_FILE), MAX_PERL_FILE),
        read_bounded(&perl.join(PROVIDER_FILE), MAX_PERL_FILE),
    ) else {
        return PluginHealth::Missing;
    };
    let Ok(Some(line)) = read_marker(pve, node) else {
        return PluginHealth::Missing;
    };
    let Some(marker) = parse_marker(&line) else {
        return PluginHealth::Changed;
    };
    let hash = plugin_hash(&plugin, &provider);
    if marker.hash != hash {
        PluginHealth::Changed
    } else if !(MIN_PROVIDER_PROTOCOL..=PROVIDER_PROTOCOL).contains(&marker.protocol) {
        PluginHealth::Unsupported
    } else if hash != embedded_plugin_hash() {
        PluginHealth::Outdated
    } else {
        PluginHealth::Ok
    }
}

/// Sets `restore.allow_guest_in_place: true` in this node's `agent.yaml` and
/// restarts the agent. Only a root shell on the node gets here; the server
/// has no path to this file.
fn allow_guest_in_place(
    path: &Path,
    commands: &dyn Commands,
    out: &mut dyn Write,
) -> Result<(), InstallError> {
    use yaml_serde::Value;
    let invalid = |e: &dyn fmt::Display| InstallError::Failed(format!("{}: {e}", path.display()));
    let (mut doc, mode) = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => {
            let bytes = read_bounded(path, MAX_AGENT_YAML).map_err(failed(path.display()))?;
            let text = String::from_utf8(bytes).map_err(|e| invalid(&e))?;
            let doc: Value = yaml_serde::from_str(&text).map_err(|e| invalid(&e))?;
            (doc, Some(meta.permissions().mode() & 0o7777))
        }
        Ok(_) => return Err(invalid(&"not a regular file")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => (Value::Null, None),
        Err(e) => return Err(failed(path.display())(e)),
    };
    if doc["restore"]["allow_guest_in_place"] == Value::Bool(true) {
        return Ok(());
    }
    let mapping = |v: &Value| matches!(v, Value::Null | Value::Mapping(_));
    if !mapping(&doc) || !mapping(&doc["restore"]) {
        return Err(invalid(&"the file and its `restore` key must be mappings"));
    }
    doc["restore"]["allow_guest_in_place"] = Value::Bool(true);
    let text = yaml_serde::to_string(&doc).map_err(|e| invalid(&e))?;
    match mode {
        Some(mode) => write_atomic(path, text.as_bytes(), mode),
        None => write_private_file(path, text.as_bytes()),
    }
    .map_err(failed(path.display()))?;
    let _ = writeln!(
        out,
        "set restore.allow_guest_in_place: true in {} (comments in that file are not kept)",
        path.display()
    );
    commands
        .run(&["systemctl", "try-restart", SERVICE])
        .map_err(|e| InstallError::Failed(e.to_string()))
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
    if options.allow_guest_in_place {
        allow_guest_in_place(&roots.agent_yaml, commands, out)?;
    }

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
            self.run_with(wait, false, commands)
        }

        pub fn run_with(
            &self,
            wait: Duration,
            allow_guest_in_place: bool,
            commands: &Recorder,
        ) -> (Result<InstallReport, InstallError>, String) {
            let mut out = Vec::new();
            let options = InstallOptions {
                allow_guest_in_place,
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

    fn ready_cluster() -> Cluster {
        let cluster = Cluster::new("9.2.33");
        cluster.marker("pve-02", &good_marker("9.3.1"));
        cluster
    }

    fn file_identity(path: &Path) -> (u64, std::time::SystemTime) {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path).unwrap();
        (meta.ino(), meta.modified().unwrap())
    }

    fn restarts(commands: &Recorder) -> Vec<Vec<String>> {
        commands
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|argv| argv[1] == "try-restart")
            .cloned()
            .collect()
    }

    #[test]
    fn pve_install_idempotent() {
        let cluster = ready_cluster();
        let commands = Recorder::default();
        cluster.run(Duration::from_secs(5), &commands).0.unwrap();
        assert_eq!(commands.pvesm_calls(), 1);
        std::fs::write(
            cluster.roots.pve.join("storage.cfg"),
            "dir: local\n\tpath /var/lib/vz\n\nlodestore: lodestore\n\tcontent backup\n\tprune-backups keep-all=1\n",
        )
        .unwrap();
        let files = [
            cluster.roots.perl.join(PLUGIN_FILE),
            cluster.roots.perl.join(PROVIDER_FILE),
            cluster.roots.pve.join("lodestore/plugin/pve-01"),
        ];
        let before: Vec<_> = files.iter().map(|f| file_identity(f)).collect();
        std::thread::sleep(Duration::from_millis(20));

        let again = Recorder::default();
        let (result, _) = cluster.run(Duration::from_secs(5), &again);
        assert_eq!(result.unwrap().nodes.len(), 2);
        assert_eq!(again.pvesm_calls(), 0);
        let after: Vec<_> = files.iter().map(|f| file_identity(f)).collect();
        assert_eq!(before, after, "nothing is rewritten");
    }

    #[test]
    fn pve_install_refuses_old_pve() {
        let cluster = Cluster::new("9.2.32");
        cluster.marker("pve-02", &good_marker("9.3.1"));
        let commands = Recorder::default();
        let error = cluster
            .run(Duration::from_secs(5), &commands)
            .0
            .unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(
            error
                .to_string()
                .contains("pve-manager 9.2.33 or newer; this node has 9.2.32"),
            "{error}"
        );
        assert!(!cluster.roots.perl.join(PLUGIN_FILE).exists());
        assert!(!cluster.roots.pve.join("lodestore/plugin/pve-01").exists());
        assert!(commands.0.lock().unwrap().is_empty());

        std::fs::remove_file(cluster.roots.pve.join(".members")).unwrap();
        let error = cluster
            .run(Duration::from_secs(5), &commands)
            .0
            .unwrap_err();
        assert_eq!(error.exit_code(), 2, "not a Proxmox VE node: {error}");
    }

    #[test]
    fn pve_install_outdated_marker() {
        let cluster = Cluster::new("9.2.33");
        cluster.marker("pve-02", "0.1.0 1 0123abcd 9.3.1");
        let commands = Recorder::default();
        let (result, out) = cluster.run(Duration::from_secs(2), &commands);
        assert_eq!(result.unwrap_err().exit_code(), 3);
        assert!(
            out.contains("pve-02: outdated (plugin of agent 0.1.0)"),
            "{out}"
        );
        assert_eq!(commands.pvesm_calls(), 0);
    }

    #[test]
    fn pve_install_adds_the_storage_another_node_added_first() {
        struct Racer<'a>(&'a Path);
        impl Commands for Racer<'_> {
            fn run(&self, argv: &[&str]) -> anyhow::Result<()> {
                if argv[0] == "pvesm" {
                    std::fs::write(self.0, "lodestore: lodestore\n\tcontent backup\n")?;
                    anyhow::bail!("storage ID 'lodestore' already defined");
                }
                Ok(())
            }
        }
        let cluster = ready_cluster();
        let cfg = cluster.roots.pve.join("storage.cfg");
        let options = InstallOptions {
            allow_guest_in_place: false,
            wait: Duration::from_secs(5),
        };
        install(&cluster.roots, &options, &Racer(&cfg), &mut io::sink()).unwrap();

        std::fs::write(&cfg, "dir: lodestore\n\tpath /srv\n").unwrap();
        let error = install(
            &cluster.roots,
            &options,
            &Recorder::default(),
            &mut io::sink(),
        );
        assert_eq!(error.unwrap_err().exit_code(), 1);
    }

    #[test]
    fn pve_install_sets_in_place_key_locally() {
        let cluster = ready_cluster();
        let yaml = &cluster.roots.agent_yaml;
        std::fs::create_dir_all(yaml.parent().unwrap()).unwrap();
        std::fs::write(yaml, "hooks: {allow_inline: true}\n").unwrap();
        std::fs::set_permissions(yaml, std::fs::Permissions::from_mode(0o600)).unwrap();
        let paths = crate::Paths {
            config_dir: yaml.parent().unwrap().to_path_buf(),
            state_dir: yaml.parent().unwrap().join("state"),
        };

        let commands = Recorder::default();
        cluster.run(Duration::from_secs(5), &commands).0.unwrap();
        assert_eq!(
            std::fs::read_to_string(yaml).unwrap(),
            "hooks: {allow_inline: true}\n"
        );
        assert!(restarts(&commands).is_empty());

        let commands = Recorder::default();
        let (result, out) = cluster.run_with(Duration::from_secs(5), true, &commands);
        result.unwrap();
        assert!(out.contains("comments in that file are not kept"), "{out}");
        let settings = crate::config::AgentSettings::load(&paths);
        assert!(settings.restore.allow_guest_in_place);
        assert!(settings.hooks.allow_inline);
        let mode = std::fs::metadata(yaml).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            restarts(&commands),
            [vec!["systemctl", "try-restart", "lodestore-agent.service"]]
        );

        let commands = Recorder::default();
        cluster
            .run_with(Duration::from_secs(5), true, &commands)
            .0
            .unwrap();
        assert!(
            restarts(&commands).is_empty(),
            "a key already set is left alone"
        );

        std::fs::write(yaml, "restore: [1]\n").unwrap();
        let (result, _) = cluster.run_with(Duration::from_secs(5), true, &Recorder::default());
        assert_eq!(result.unwrap_err().exit_code(), 1);
        assert_eq!(std::fs::read_to_string(yaml).unwrap(), "restore: [1]\n");
    }

    #[test]
    fn pve_plugin_health() {
        let cluster = ready_cluster();
        let (pve, perl) = (&cluster.roots.pve, &cluster.roots.perl);
        assert_eq!(plugin_health(pve, perl, "pve-01"), PluginHealth::Missing);
        cluster
            .run(Duration::from_secs(5), &Recorder::default())
            .0
            .unwrap();
        assert_eq!(plugin_health(pve, perl, "pve-01"), PluginHealth::Ok);
        assert!(PluginHealth::Ok.usable() && PluginHealth::Outdated.usable());

        let plugin = perl.join(PLUGIN_FILE);
        std::fs::write(&plugin, format!("{PLUGIN_SOURCE}# edited\n")).unwrap();
        assert_eq!(plugin_health(pve, perl, "pve-01"), PluginHealth::Changed);
        assert!(!PluginHealth::Changed.usable());
        std::fs::write(&plugin, PLUGIN_SOURCE).unwrap();

        let hash = embedded_plugin_hash();
        cluster.marker("pve-01", &format!("0.2.0 0 {hash} 9.2.33"));
        assert_eq!(
            plugin_health(pve, perl, "pve-01"),
            PluginHealth::Unsupported
        );

        let (old_plugin, old_provider) = ("package Old::Plugin;\n", "package Old::Provider;\n");
        std::fs::write(&plugin, old_plugin).unwrap();
        std::fs::write(perl.join(PROVIDER_FILE), old_provider).unwrap();
        let old = plugin_hash(old_plugin.as_bytes(), old_provider.as_bytes());
        cluster.marker("pve-01", &format!("0.1.0 1 {old} 9.2.33"));
        assert_eq!(plugin_health(pve, perl, "pve-01"), PluginHealth::Outdated);

        std::fs::remove_file(pve.join("lodestore/plugin/pve-01")).unwrap();
        assert_eq!(plugin_health(pve, perl, "pve-01"), PluginHealth::Missing);
    }

    #[test]
    fn pve_install_parses_markers() {
        let m = parse_marker("0.2.0 1 abc 9.3.1").unwrap();
        assert_eq!(
            (m.protocol, m.hash.as_str(), m.pve_manager.as_str()),
            (1, "abc", "9.3.1")
        );
        assert_eq!(m.to_string(), "0.2.0 1 abc 9.3.1");
        for bad in [
            "",
            "0.2.0 1 abc",
            "0.2.0 x abc 9.3.1",
            "0.2.0 1 abc 9.3.1 extra",
        ] {
            assert_eq!(parse_marker(bad), None, "{bad:?}");
        }
    }
}
