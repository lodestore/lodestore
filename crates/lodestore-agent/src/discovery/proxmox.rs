//! Proxmox VE node and guests from the files `pmxcfs` serves under
//! `/etc/pve`. Nothing is written there and no process is started.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use lodestore_proto::agent::v1::{
    ClusterNode, ConfigLine, GuestKind, ProxmoxFacts, ProxmoxGuest, ProxmoxStorage,
};
use serde::{Deserialize, Serialize};

use super::{
    MAX_BRIDGES, MAX_CLUSTER_NODES, MAX_GUEST_CONF_BYTES, MAX_GUEST_TAGS, MAX_GUEST_VOLUMES,
    MAX_GUESTS, MAX_NAME_BYTES, MAX_PLUGIN_MARKER, MAX_PVE_FILE_BYTES, MAX_SCAN_BYTES,
    MAX_STATE_BYTES, MAX_STORAGE_CONTENT, MAX_STORAGES, MAX_VERSION_BYTES, MAX_VOLUME_LINE,
    STORAGE_CONTENT_TYPES, ScanFailed,
};
use super::{clip, open_nofollow, read_bounded};
use crate::identity::write_private_file;
use crate::pve::install::{PluginHealth, plugin_health};

const VMIDS: std::ops::RangeInclusive<u32> = 100..=999_999_999;
const MAX_DPKG_STATUS_BYTES: u64 = 64 << 20;

pub(crate) const PLUGIN_FILE: &str = "PVE/Storage/Custom/LodestorePlugin.pm";
pub(crate) const PROVIDER_FILE: &str = "PVE/BackupProvider/Plugin/Lodestore.pm";
pub(crate) const PLUGIN_MARKER_DIR: &str = "lodestore/plugin";
pub(crate) const HA_RESOURCES: &str = "ha/resources.cfg";

/// Where the files are read from; tests point them at temporary directories.
#[derive(Debug, Clone)]
pub(crate) struct ScanRoots {
    pub pve: PathBuf,
    pub sys: PathBuf,
    pub perl: PathBuf,
    pub dpkg_status: PathBuf,
}

pub(crate) struct Found {
    pub proxmox: ProxmoxFacts,
    pub truncated: bool,
}

#[derive(Deserialize)]
struct Members {
    nodename: String,
    cluster: Option<MembersCluster>,
    #[serde(default)]
    nodelist: BTreeMap<String, MembersNode>,
}

#[derive(Deserialize)]
struct MembersNode {
    #[serde(default)]
    online: i64,
}

#[derive(Deserialize)]
struct MembersCluster {
    name: String,
}

#[derive(Deserialize)]
struct VmList {
    ids: HashMap<String, VmListEntry>,
}

#[derive(Deserialize)]
struct VmListEntry {
    node: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct GuestConf {
    pub name: Option<String>,
    pub hostname: Option<String>,
    pub tags: Vec<String>,
    pub smbios_uuid: Option<String>,
    pub unprivileged: bool,
    pub pve_uid: Option<String>,
    pub pve_rtime: u64,
    pub volumes: Vec<(String, String)>,
    pub volumes_truncated: bool,
}

fn failed(what: &str) -> impl FnOnce(io::Error) -> ScanFailed + '_ {
    move |e| ScanFailed(format!("{what} cannot be read: {e}"))
}

pub(crate) fn node_name_valid(name: &str) -> bool {
    (1..=63).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

pub(crate) struct MembersFile {
    pub node: String,
    cluster: Option<String>,
    pub nodes: BTreeMap<String, bool>,
}

pub(crate) fn parse_members(bytes: &[u8]) -> Result<MembersFile, ScanFailed> {
    let members: Members = serde_json::from_slice(bytes)
        .map_err(|e| ScanFailed(format!(".members cannot be parsed: {e}")))?;
    if !node_name_valid(&members.nodename) {
        return Err(ScanFailed(format!(
            ".members names the node {:?}, which is not a valid node name",
            members.nodename
        )));
    }
    Ok(MembersFile {
        node: members.nodename,
        cluster: members.cluster.map(|c| c.name),
        nodes: members
            .nodelist
            .into_iter()
            .map(|(name, node)| (name, node.online != 0))
            .collect(),
    })
}

/// `cluster_name` of the `totem` block of `corosync.conf`.
fn parse_cluster_name(text: &str) -> Option<String> {
    let mut blocks = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('#') {
            continue;
        }
        if let Some(block) = line.strip_suffix('{') {
            blocks.push(block.trim());
        } else if line == "}" {
            blocks.pop();
        } else if blocks == ["totem"]
            && let Some(name) = line.strip_prefix("cluster_name:")
        {
            return Some(name.trim().to_string()).filter(|n| !n.is_empty());
        }
    }
    None
}

struct Listed {
    guests: Vec<(GuestKind, u32)>,
    skipped: bool,
    /// Every VMID of the cluster, whatever node and type.
    used: HashSet<u32>,
}

/// The guests of `node` as `(kind, vmid)`, and whether an entry of this node
/// had to be skipped.
fn parse_vmlist(bytes: &[u8], node: &str) -> Result<Listed, ScanFailed> {
    let list: VmList = serde_json::from_slice(bytes)
        .map_err(|e| ScanFailed(format!(".vmlist cannot be parsed: {e}")))?;
    let mut skipped = false;
    let mut guests = Vec::new();
    let mut used = HashSet::new();
    for (key, entry) in list.ids {
        used.extend(key.parse::<u32>());
        if entry.node != node {
            continue;
        }
        let vmid = key
            .parse::<u32>()
            .ok()
            .filter(|id| VMIDS.contains(id) && id.to_string() == key);
        let kind = match entry.kind.as_str() {
            "qemu" => Some(GuestKind::Vm),
            "lxc" => Some(GuestKind::Ct),
            _ => None,
        };
        match (kind, vmid) {
            (Some(kind), Some(vmid)) => guests.push((kind, vmid)),
            _ => skipped = true,
        }
    }
    guests.sort_by_key(|&(kind, vmid)| (vmid, kind as i32));
    Ok(Listed {
        guests,
        skipped,
        used,
    })
}

fn split_tags(value: &str) -> Vec<String> {
    value
        .split(|c: char| c == ';' || c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// The keys of the main section of a guest config. A key matches only at the
/// start of a line, so a `#name:` line of the description is not a name.
pub(crate) fn parse_guest_conf(text: &str) -> GuestConf {
    let mut conf = GuestConf::default();
    let mut meta = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key {
            "meta" => meta = Some(value),
            "unprivileged" if !conf.unprivileged => conf.unprivileged = value == "1",
            _ if is_volume_key(key) => {
                if conf.volumes.len() >= MAX_GUEST_VOLUMES || value.len() > MAX_VOLUME_LINE {
                    conf.volumes_truncated = true;
                } else {
                    conf.volumes.push((key.to_string(), value.to_string()));
                }
            }
            "name" if conf.name.is_none() => conf.name = Some(value.to_string()),
            "hostname" if conf.hostname.is_none() => conf.hostname = Some(value.to_string()),
            "tags" if conf.tags.is_empty() => conf.tags = split_tags(value),
            "smbios1" if conf.smbios_uuid.is_none() => {
                conf.smbios_uuid = value
                    .split(',')
                    .find_map(|field| field.trim().strip_prefix("uuid="))
                    .map(str::to_string);
            }
            _ => {}
        }
    }
    if let Some((uid, rtime)) = meta.and_then(parse_meta) {
        (conf.pve_uid, conf.pve_rtime) = (Some(uid), rtime);
    }
    conf
}

fn is_volume_key(key: &str) -> bool {
    let digits = key
        .strip_prefix("mp")
        .or_else(|| key.strip_prefix("unused"));
    key == "rootfs"
        || digits
            .is_some_and(|d| (1..=3).contains(&d.len()) && d.bytes().all(|b| b.is_ascii_digit()))
}

/// `uid` and `rtime` of a `meta:` line, as `PVE::GuestIdentity` reads them: a
/// repeated member or a uid that is not 20 base32 characters makes the whole
/// line invalid, which is the same as no uid.
fn parse_meta(value: &str) -> Option<(String, u64)> {
    let (mut uid, mut rtime) = (None, None);
    for member in value.split(',') {
        let (key, member_value) = member.trim().split_once('=')?;
        let slot = match key {
            "uid" => &mut uid,
            "rtime" => &mut rtime,
            _ => continue,
        };
        if slot.replace(member_value).is_some() {
            return None;
        }
    }
    let uid = uid?;
    let valid = uid.len() == 20 && uid.bytes().all(|b| matches!(b, b'a'..=b'z' | b'2'..=b'7'));
    valid.then(|| {
        (
            uid.to_string(),
            rtime.and_then(|t| t.parse().ok()).unwrap_or(0),
        )
    })
}

fn storage_id_valid(id: &str) -> bool {
    let mut bytes = id.bytes();
    id.len() <= MAX_NAME_BYTES
        && bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn storage_type_valid(kind: &str) -> bool {
    (1..=32).contains(&kind.len())
        && kind
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn is_true(value: &str) -> bool {
    matches!(value, "1" | "yes" | "true" | "on")
}

/// The storages `node` can use from `storage.cfg` (`{type}: {id}` sections of
/// indented `{key} {value}` lines), and whether a section had to be left out.
/// A `nodes` list without `node` and a `disable` flag leave a storage out
/// silently: they are the user's choice, not a loss.
fn parse_storages(text: &str, node: &str) -> (Vec<ProxmoxStorage>, bool) {
    struct Section {
        kind: String,
        id: String,
        content: Vec<String>,
        shared: bool,
        skip: bool,
    }
    let mut truncated = false;
    let mut storages = Vec::new();
    let mut open: Option<Section> = None;
    let mut close = |section: Option<Section>, storages: &mut Vec<ProxmoxStorage>| {
        let Some(s) = section else { return };
        if s.skip {
            return;
        }
        if !storage_id_valid(&s.id)
            || !storage_type_valid(&s.kind)
            || storages.len() >= MAX_STORAGES
        {
            truncated = true;
        } else {
            storages.push(ProxmoxStorage {
                id: s.id,
                r#type: s.kind,
                content: s.content,
                shared: s.shared,
            });
        }
    };
    for line in text.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            let Some(section) = open.as_mut() else {
                continue;
            };
            let line = line.trim();
            let (key, value) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            let value = value.trim();
            match key {
                "content" => {
                    let mut known = Vec::new();
                    for item in value.split(',').map(str::trim) {
                        if STORAGE_CONTENT_TYPES.contains(&item) && !known.iter().any(|k| k == item)
                        {
                            known.push(item.to_string());
                        }
                    }
                    known.truncate(MAX_STORAGE_CONTENT);
                    section.content = known;
                }
                "nodes" => section.skip |= !value.split(',').any(|n| n.trim() == node),
                "disable" => section.skip |= is_true(value),
                "shared" => section.shared = is_true(value),
                _ => {}
            }
        } else {
            close(open.take(), &mut storages);
            open = line.split_once(':').map(|(kind, id)| Section {
                kind: kind.trim().to_string(),
                id: id.trim().to_string(),
                content: Vec::new(),
                shared: false,
                skip: false,
            });
        }
    }
    close(open.take(), &mut storages);
    (storages, truncated)
}

/// `vmid -> pool` from the `pool:<id>:<comment>:<vmids>:<storages>:` lines of
/// `user.cfg`.
pub(crate) fn parse_pools(text: &str) -> HashMap<u32, String> {
    let mut pools = HashMap::new();
    for line in text.lines() {
        let mut fields = line.split(':');
        if fields.next() != Some("pool") {
            continue;
        }
        let (Some(pool), Some(_comment), Some(members)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        for vmid in members.split(',').filter_map(|m| m.trim().parse().ok()) {
            pools.insert(vmid, pool.to_string());
        }
    }
    pools
}

/// Reads lines up to the first `[section]` line: snapshot sections after it
/// can hold hundreds of KiB of history that discovery never needs.
pub(crate) fn read_main_section(reader: impl BufRead, limit: u64) -> io::Result<String> {
    let mut reader = reader.take(limit.saturating_add(1));
    let (mut main, mut line) = (Vec::new(), Vec::new());
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 || line.first() == Some(&b'[') {
            break;
        }
        main.extend_from_slice(&line);
        if main.len() as u64 > limit {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                format!("the main section is larger than {limit} bytes"),
            ));
        }
    }
    Ok(String::from_utf8_lossy(&main).into_owned())
}

fn read_conf(path: &Path) -> io::Result<String> {
    read_main_section(BufReader::new(open_nofollow(path)?), MAX_GUEST_CONF_BYTES)
}

fn map_key(kind: GuestKind, vmid: u32) -> String {
    match kind {
        GuestKind::Vm => format!("vm/{vmid}"),
        _ => format!("ct/{vmid}"),
    }
}

/// A saved id. A container keeps the `meta uid` it had when the id was given;
/// an entry without one is the old format, written before phase 25.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
enum Entry {
    Plain(String),
    Ct {
        uuid: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pve_uid: Option<String>,
    },
}

impl Entry {
    fn ct(uuid: String, pve_uid: &str) -> Entry {
        Entry::Ct {
            uuid,
            pve_uid: Some(pve_uid.to_string()),
        }
    }

    fn uuid(&self) -> &str {
        match self {
            Entry::Plain(uuid) | Entry::Ct { uuid, .. } => uuid,
        }
    }

    fn recorded_uid(&self) -> Option<&str> {
        match self {
            Entry::Plain(_) => None,
            Entry::Ct { pve_uid, .. } => pve_uid.as_deref(),
        }
    }
}

type Ids = BTreeMap<String, Entry>;

enum Instance {
    Keep(String),
    Adopt(String),
    New,
}

/// Any difference between the recorded and the current uid is a new
/// instance, "was empty, is set now" and "was set, is gone" included; only an
/// entry of the old format takes the current uid once (ADR 0008, Identity).
fn instance_of(entry: Option<&Entry>, uid: &str) -> Instance {
    match entry {
        None => Instance::New,
        Some(entry) => match entry.recorded_uid() {
            None => Instance::Adopt(entry.uuid().to_string()),
            Some(recorded) if recorded == uid => Instance::Keep(entry.uuid().to_string()),
            Some(_) => Instance::New,
        },
    }
}

/// Moves a damaged id file aside: its containers get new ids, which makes
/// them new instances, as after a reinstall of the node.
fn quarantine(path: &Path, why: &dyn std::fmt::Display) -> Result<Ids, ScanFailed> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".corrupt");
    std::fs::rename(path, path.with_file_name(name)).map_err(failed("the source id file"))?;
    tracing::warn!(
        file = %path.display(),
        %why,
        "the source id file is damaged; it was moved aside and containers get new ids"
    );
    Ok(Ids::new())
}

/// Any read error other than a missing, damaged, oversized or symlinked file
/// fails the run and leaves the file alone: a transient error must not renew
/// every container id.
fn load_ids(path: &Path) -> Result<Ids, ScanFailed> {
    let bytes = match read_bounded(path, MAX_STATE_BYTES) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Ids::new()),
        Err(e)
            if e.kind() == io::ErrorKind::FileTooLarge || e.raw_os_error() == Some(libc::ELOOP) =>
        {
            return quarantine(path, &e);
        }
        Err(e) => return Err(failed("the source id file")(e)),
    };
    match serde_json::from_slice::<Ids>(&bytes) {
        Ok(mut ids) => {
            ids.retain(|_, entry| uuid::Uuid::parse_str(entry.uuid()).is_ok());
            Ok(ids)
        }
        Err(e) => quarantine(path, &e),
    }
}

fn new_uuid() -> String {
    uuid::Uuid::now_v7().hyphenated().to_string()
}

/// Only a missing `corosync.conf` together with no `cluster.name` in
/// `.members` is a node outside a cluster; anything else that does not agree
/// fails the run, so a guest never changes its key because of a bad read.
fn cluster_name(root: &Path, members: Option<String>) -> Result<String, ScanFailed> {
    let conf = match read_bounded(&root.join("corosync.conf"), MAX_PVE_FILE_BYTES) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(failed("corosync.conf")(e)),
        Ok(bytes) => Some(
            parse_cluster_name(&String::from_utf8_lossy(&bytes))
                .ok_or_else(|| ScanFailed("corosync.conf names no cluster".into()))?,
        ),
    };
    if conf != members {
        return Err(ScanFailed(format!(
            "corosync.conf names the cluster {conf:?} and .members {members:?}"
        )));
    }
    Ok(conf.unwrap_or_default())
}

/// A cut tag would select guests it was never put on, so a long tag is left
/// out rather than shortened.
fn guest_tags(tags: Vec<String>, truncated: &mut bool) -> Vec<String> {
    let before = tags.len();
    let mut tags: Vec<String> = tags
        .into_iter()
        .filter(|t| t.len() <= MAX_NAME_BYTES)
        .collect();
    tags.truncate(MAX_GUEST_TAGS);
    *truncated |= tags.len() < before;
    tags
}

fn smbios_uuid(conf: &GuestConf) -> Option<uuid::Uuid> {
    conf.smbios_uuid
        .as_deref()
        .and_then(|u| uuid::Uuid::parse_str(u).ok())
}

fn read_marker(roots: &ScanRoots, name: &str, truncated: &mut bool) -> String {
    let path = roots.pve.join(PLUGIN_MARKER_DIR).join(name);
    match read_bounded(&path, MAX_PLUGIN_MARKER as u64) {
        Ok(bytes) => String::from_utf8_lossy(&bytes)
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string(),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::InvalidInput
            ) || e.raw_os_error() == Some(libc::ELOOP) =>
        {
            String::new()
        }
        Err(_) => {
            *truncated = true;
            String::new()
        }
    }
}

fn cluster_nodes(
    roots: &ScanRoots,
    members: &BTreeMap<String, bool>,
    truncated: &mut bool,
) -> Vec<ClusterNode> {
    let mut nodes = Vec::new();
    for (name, &online) in members {
        if !node_name_valid(name) {
            *truncated = true;
        } else if nodes.len() >= MAX_CLUSTER_NODES {
            *truncated = true;
            break;
        } else {
            nodes.push(ClusterNode {
                name: name.clone(),
                online,
                plugin_marker: read_marker(roots, name, truncated),
            });
        }
    }
    nodes
}

fn iface_name_valid(name: &str) -> bool {
    (1..=15).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn bridges(roots: &ScanRoots, truncated: &mut bool) -> Vec<String> {
    let dir = roots.sys.join("class/net");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(_) => {
            *truncated = true;
            return Vec::new();
        }
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| iface_name_valid(name))
        .filter(|name| std::fs::metadata(dir.join(name).join("bridge")).is_ok_and(|m| m.is_dir()))
        .collect();
    names.sort();
    if names.len() > MAX_BRIDGES {
        names.truncate(MAX_BRIDGES);
        *truncated = true;
    }
    names
}

fn ct_running(roots: &ScanRoots, vmid: u32) -> bool {
    let cgroup = roots.sys.join("fs/cgroup/lxc").join(vmid.to_string());
    std::fs::symlink_metadata(cgroup).is_ok_and(|m| m.is_dir())
}

/// The containers with a `ct: {vmid}` section in `ha/resources.cfg`. A file
/// that cannot be read fails the run: a container reported as not managed by
/// HA could be replaced under the HA manager.
fn ha_containers(roots: &ScanRoots) -> Result<HashSet<u32>, ScanFailed> {
    match read_bounded(&roots.pve.join(HA_RESOURCES), MAX_PVE_FILE_BYTES) {
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes)
            .lines()
            .filter_map(|line| line.strip_prefix("ct:")?.trim().parse().ok())
            .collect()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(HashSet::new()),
        Err(e) => Err(failed(HA_RESOURCES)(e)),
    }
}

/// `lower` and `upper` (exclusive) of `next-id:` in `datacenter.cfg`.
fn parse_next_id(text: &str) -> (u32, u32) {
    let (mut lower, mut upper) = (100, 1_000_000);
    for value in text.lines().filter_map(|l| l.strip_prefix("next-id:")) {
        for (key, number) in value.split(',').filter_map(|m| m.trim().split_once('=')) {
            match (key, number.parse::<u32>()) {
                ("lower", Ok(n)) => lower = n,
                ("upper", Ok(n)) => upper = n,
                _ => {}
            }
        }
    }
    (lower.max(*VMIDS.start()), upper.min(*VMIDS.end()))
}

/// The smallest VMID of the `next-id` range that no guest of the cluster has;
/// 0 when there is none or the range cannot be read.
fn next_vmid(roots: &ScanRoots, used: &HashSet<u32>, truncated: &mut bool) -> u32 {
    let range = match read_bounded(&roots.pve.join("datacenter.cfg"), MAX_PVE_FILE_BYTES) {
        Ok(bytes) => parse_next_id(&String::from_utf8_lossy(&bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => parse_next_id(""),
        Err(_) => {
            *truncated = true;
            return 0;
        }
    };
    (range.0..range.1)
        .find(|id| !used.contains(id))
        .unwrap_or(0)
}

/// The version of the installed `pve-manager` from the dpkg database.
pub(crate) fn pve_manager_version(path: &Path) -> String {
    let Ok(file) = open_nofollow(path) else {
        return String::new();
    };
    let mut reader = BufReader::new(file.take(MAX_DPKG_STATUS_BYTES));
    let (mut line, mut package, mut installed, mut version) = (Vec::new(), false, false, None);
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line).unwrap_or(0);
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end();
        if read == 0 || text.is_empty() {
            if package
                && installed
                && let Some(version) = version.take()
            {
                return clip_version(version);
            }
            (package, installed, version) = (false, false, None);
            if read == 0 {
                return String::new();
            }
        } else if let Some(name) = text.strip_prefix("Package: ") {
            package = name == "pve-manager";
        } else if let Some(status) = text.strip_prefix("Status: ") {
            installed = status.ends_with(" installed");
        } else if let Some(found) = text.strip_prefix("Version: ") {
            version = Some(found.to_string());
        }
    }
}

fn clip_version(version: String) -> String {
    let valid = version.len() <= MAX_VERSION_BYTES
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'~' | b':' | b'-'));
    if valid { version } else { String::new() }
}

/// What the agent knows about one container, for the facts of the node and
/// for the check right before a replace. One function builds both, so the
/// expected and the current state are compared by the same code.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
#[cfg_attr(not(test), expect(dead_code))]
pub(crate) struct CtTargetFacts {
    pub vmid: u32,
    pub name: String,
    pub pve_uid: String,
    pub pve_rtime: u64,
    pub instance_uuid: String,
    pub volumes: Vec<(String, String)>,
    pub volumes_truncated: bool,
    pub running: bool,
    pub ha_managed: bool,
    pub unprivileged: bool,
}

fn ct_facts(
    roots: &ScanRoots,
    vmid: u32,
    conf: &GuestConf,
    ha: &HashSet<u32>,
    instance_uuid: String,
) -> CtTargetFacts {
    CtTargetFacts {
        vmid,
        name: clip(conf.hostname.clone().unwrap_or_default()),
        pve_uid: conf.pve_uid.clone().unwrap_or_default(),
        pve_rtime: conf.pve_rtime,
        instance_uuid,
        volumes: conf.volumes.clone(),
        volumes_truncated: conf.volumes_truncated,
        running: ct_running(roots, vmid),
        ha_managed: ha.contains(&vmid),
        unprivileged: conf.unprivileged,
    }
}

/// The container `vmid` on this node as the scan would report it; `Ok(None)`
/// when it has no config here. `instance_uuid` is empty when the saved id
/// belongs to another `meta uid`, or there is none yet.
#[cfg_attr(not(test), expect(dead_code))]
pub(crate) fn ct_target_facts(
    roots: &ScanRoots,
    ids_path: &Path,
    vmid: u32,
) -> Result<Option<CtTargetFacts>, ScanFailed> {
    if !VMIDS.contains(&vmid) {
        return Ok(None);
    }
    let members = read_bounded(&roots.pve.join(".members"), MAX_PVE_FILE_BYTES)
        .map_err(failed(".members"))?;
    let node = parse_members(&members)?.node;
    let conf_path = roots
        .pve
        .join("nodes")
        .join(node)
        .join("lxc")
        .join(format!("{vmid}.conf"));
    let conf = match read_conf(&conf_path) {
        Ok(text) => parse_guest_conf(&text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(failed("the container config")(e)),
    };
    let ha = ha_containers(roots)?;
    let ids = load_ids(ids_path)?;
    let uid = conf.pve_uid.clone().unwrap_or_default();
    let uuid = match instance_of(ids.get(&map_key(GuestKind::Ct, vmid)), &uid) {
        Instance::Keep(uuid) | Instance::Adopt(uuid) => uuid,
        Instance::New => String::new(),
    };
    Ok(Some(ct_facts(roots, vmid, &conf, &ha, uuid)))
}

/// The saved id of a container and the `meta uid` it was given with; the uid
/// is empty for a container without one and for an entry of the old format.
#[cfg_attr(not(test), expect(dead_code))]
pub(crate) struct CtIdentity {
    pub uuid: String,
    pub pve_uid: String,
}

#[cfg_attr(not(test), expect(dead_code))]
pub(crate) fn lookup_ct(ids_path: &Path, vmid: u32) -> Result<Option<CtIdentity>, ScanFailed> {
    let ids = load_ids(ids_path)?;
    Ok(ids
        .get(&map_key(GuestKind::Ct, vmid))
        .map(|entry| CtIdentity {
            uuid: entry.uuid().to_string(),
            pve_uid: entry.recorded_uid().unwrap_or_default().to_string(),
        }))
}

/// `Ok(None)`: the Proxmox root does not exist, this is not a Proxmox node. A
/// root that exists without the cluster files is a stopped `pmxcfs`, not "no
/// Proxmox", and fails the run.
pub(crate) fn scan(
    roots: &ScanRoots,
    ids_path: &Path,
    guest_in_place_allowed: bool,
) -> Result<Option<Found>, ScanFailed> {
    scan_with(roots, ids_path, guest_in_place_allowed, MAX_SCAN_BYTES)
}

/// `budget` caps the bytes read under the Proxmox root in one run; guests whose
/// config is past it are not reported and keep their saved ids.
fn scan_with(
    roots: &ScanRoots,
    ids_path: &Path,
    guest_in_place_allowed: bool,
    budget: u64,
) -> Result<Option<Found>, ScanFailed> {
    let root = &roots.pve;
    match std::fs::metadata(root) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(failed("the Proxmox directory")(e)),
        Ok(meta) if !meta.is_dir() => {
            return Err(ScanFailed(format!("{} is not a directory", root.display())));
        }
        Ok(_) => {}
    }
    let members =
        read_bounded(&root.join(".members"), MAX_PVE_FILE_BYTES).map_err(failed(".members"))?;
    let mut spent = members.len() as u64;
    let MembersFile {
        node,
        cluster: member_cluster,
        nodes: member_nodes,
    } = parse_members(&members)?;
    let cluster = cluster_name(root, member_cluster)?;
    let (mut listed, mut truncated, used) =
        match read_bounded(&root.join(".vmlist"), MAX_PVE_FILE_BYTES) {
            Ok(bytes) => {
                spent += bytes.len() as u64;
                let list = parse_vmlist(&bytes, &node)?;
                (list.guests, list.skipped, list.used)
            }
            Err(e) if e.kind() == io::ErrorKind::FileTooLarge => {
                tracing::warn!("/etc/pve/.vmlist is too large; reporting the node without guests");
                let proxmox = ProxmoxFacts {
                    cluster,
                    node,
                    ..ProxmoxFacts::default()
                };
                return Ok(Some(Found {
                    proxmox,
                    truncated: true,
                }));
            }
            Err(e) => return Err(failed(".vmlist")(e)),
        };
    // Guests past the cap are not reported but keep their saved ids.
    let present: HashSet<String> = listed.iter().map(|&(k, v)| map_key(k, v)).collect();
    if listed.len() > MAX_GUESTS {
        listed.truncate(MAX_GUESTS);
        truncated = true;
    }
    let old_ids = load_ids(ids_path)?;
    let ha = ha_containers(roots)?;

    let pools = match read_bounded(&root.join("user.cfg"), MAX_PVE_FILE_BYTES) {
        Ok(bytes) => {
            spent += bytes.len() as u64;
            parse_pools(&String::from_utf8_lossy(&bytes))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => HashMap::new(),
        Err(_) => {
            truncated = true;
            HashMap::new()
        }
    };
    let storages = match read_bounded(&root.join("storage.cfg"), MAX_PVE_FILE_BYTES) {
        Ok(bytes) => {
            spent += bytes.len() as u64;
            let (storages, cut) = parse_storages(&String::from_utf8_lossy(&bytes), &node);
            truncated |= cut;
            storages
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(_) => {
            truncated = true;
            Vec::new()
        }
    };
    let nodes = cluster_nodes(roots, &member_nodes, &mut truncated);
    let bridges = bridges(roots, &mut truncated);
    let next_vmid = next_vmid(roots, &used, &mut truncated);

    let node_dir = root.join("nodes").join(&node);
    let mut confs: Vec<(GuestKind, u32, Option<GuestConf>)> = Vec::new();
    for &(kind, vmid) in &listed {
        if spent >= budget {
            truncated = true;
            break;
        }
        let dir = if kind == GuestKind::Vm {
            "qemu-server"
        } else {
            "lxc"
        };
        let conf = read_conf(&node_dir.join(dir).join(format!("{vmid}.conf")));
        spent += conf.as_ref().map_or(0, |text| text.len() as u64);
        confs.push((kind, vmid, conf.ok().map(|text| parse_guest_conf(&text))));
    }
    let mut smbios_seen: HashMap<uuid::Uuid, usize> = HashMap::new();
    for (_, _, conf) in confs.iter().filter(|(kind, ..)| *kind == GuestKind::Vm) {
        if let Some(uuid) = conf.as_ref().and_then(smbios_uuid) {
            *smbios_seen.entry(uuid).or_default() += 1;
        }
    }

    let mut ids: Ids = old_ids
        .iter()
        .filter(|(key, _)| present.contains(*key))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut fresh = Vec::new();
    let mut guests = Vec::new();
    for (kind, vmid, conf) in confs {
        let key = map_key(kind, vmid);
        let own = match (kind, &conf) {
            (GuestKind::Vm, None) => {
                truncated = true;
                continue;
            }
            (GuestKind::Vm, Some(conf)) if conf.smbios_uuid.is_some() => Some(smbios_uuid(conf)),
            _ => None,
        };
        let uuid = match own {
            Some(own) => {
                ids.remove(&key);
                match own.filter(|u| !u.is_nil() && smbios_seen.get(u) == Some(&1)) {
                    Some(uuid) => uuid.hyphenated().to_string(),
                    None => {
                        tracing::warn!(
                            vmid,
                            "the VM has a SMBIOS uuid that is empty, invalid or shared with another VM of this node; it is not reported"
                        );
                        truncated = true;
                        continue;
                    }
                }
            }
            None if kind == GuestKind::Ct => {
                let Some(conf) = &conf else {
                    // The uid is unknown: a new id is saved in the old format and takes the uid once the config reads.
                    truncated = true;
                    let uuid = match ids.get(&key) {
                        Some(entry) => entry.uuid().to_string(),
                        None => {
                            let uuid = new_uuid();
                            ids.insert(key.clone(), Entry::Plain(uuid.clone()));
                            fresh.push(uuid.clone());
                            uuid
                        }
                    };
                    guests.push(unreadable_ct(roots, vmid, &uuid, &ha));
                    continue;
                };
                let uid = conf.pve_uid.clone().unwrap_or_default();
                match instance_of(ids.get(&key), &uid) {
                    Instance::Keep(uuid) => uuid,
                    Instance::Adopt(uuid) => {
                        ids.insert(key.clone(), Entry::ct(uuid.clone(), &uid));
                        uuid
                    }
                    Instance::New => {
                        let uuid = new_uuid();
                        ids.insert(key.clone(), Entry::ct(uuid.clone(), &uid));
                        fresh.push(uuid.clone());
                        uuid
                    }
                }
            }
            None => match ids.get(&key) {
                Some(entry) => entry.uuid().to_string(),
                None => {
                    let uuid = new_uuid();
                    ids.insert(key.clone(), Entry::Plain(uuid.clone()));
                    fresh.push(uuid.clone());
                    uuid
                }
            },
        };
        let conf = conf.unwrap_or_default();
        let pool = match pools.get(&vmid) {
            Some(pool) if pool.len() > MAX_NAME_BYTES => {
                truncated = true;
                String::new()
            }
            pool => pool.cloned().unwrap_or_default(),
        };
        let mut guest = ProxmoxGuest {
            kind: kind as i32,
            vmid,
            uuid: uuid.clone(),
            pool,
            ..ProxmoxGuest::default()
        };
        if kind == GuestKind::Ct {
            apply_ct(&mut guest, ct_facts(roots, vmid, &conf, &ha, uuid));
        } else {
            guest.name = clip(conf.name.clone().unwrap_or_default());
        }
        guest.tags = guest_tags(conf.tags, &mut truncated);
        guests.push(guest);
    }
    if ids != old_ids {
        let written = serde_json::to_vec(&ids)
            .map_err(io::Error::other)
            .and_then(|json| {
                if json.len() as u64 > MAX_STATE_BYTES {
                    return Err(io::Error::from(io::ErrorKind::FileTooLarge));
                }
                write_private_file(ids_path, &json)
            });
        if let Err(error) = written {
            tracing::warn!(
                %error,
                "the source id file cannot be written; guests with new ids are not reported until it can"
            );
            truncated |= !fresh.is_empty();
            guests.retain(|g| !fresh.contains(&g.uuid));
        }
    }
    let plugin = plugin_health(&roots.pve, &roots.perl, &node);
    Ok(Some(Found {
        proxmox: ProxmoxFacts {
            cluster,
            node,
            guests,
            storages,
            nodes,
            plugin_installed: plugin.usable(),
            guest_in_place_allowed,
            next_vmid,
            pve_version: pve_manager_version(&roots.dpkg_status),
            plugin_current: plugin == PluginHealth::Ok,
            bridges,
        },
        truncated,
    }))
}

fn apply_ct(guest: &mut ProxmoxGuest, facts: CtTargetFacts) {
    guest.name = facts.name;
    guest.running = facts.running;
    guest.unprivileged = facts.unprivileged;
    guest.pve_uid = facts.pve_uid;
    guest.pve_rtime = facts.pve_rtime;
    guest.ha_managed = facts.ha_managed;
    guest.volumes = facts
        .volumes
        .into_iter()
        .map(|(key, value)| ConfigLine { key, value })
        .collect();
    guest.volumes_truncated = facts.volumes_truncated;
}

/// A container whose config cannot be read keeps its saved id; what the config
/// would say is not reported.
fn unreadable_ct(roots: &ScanRoots, vmid: u32, uuid: &str, ha: &HashSet<u32>) -> ProxmoxGuest {
    let mut guest = ProxmoxGuest {
        kind: GuestKind::Ct as i32,
        vmid,
        uuid: uuid.to_string(),
        ..ProxmoxGuest::default()
    };
    let conf = GuestConf::default();
    apply_ct(
        &mut guest,
        ct_facts(roots, vmid, &conf, ha, uuid.to_string()),
    );
    guest
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::pve::install::{PLUGIN_SOURCE, PROVIDER_SOURCE, embedded_plugin_hash, plugin_hash};

    pub(super) struct Pve {
        _tmp: tempfile::TempDir,
        pub root: PathBuf,
        pub sys: PathBuf,
        pub perl: PathBuf,
        pub dpkg: PathBuf,
        pub ids: PathBuf,
    }

    impl Pve {
        pub fn new() -> Pve {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("pve");
            let ids = tmp.path().join("state").join("source-ids.json");
            std::fs::create_dir_all(&root).unwrap();
            Pve {
                sys: tmp.path().join("sys"),
                perl: tmp.path().join("perl5"),
                dpkg: tmp.path().join("dpkg-status"),
                _tmp: tmp,
                root,
                ids,
            }
        }

        pub fn roots(&self) -> ScanRoots {
            ScanRoots {
                pve: self.root.clone(),
                sys: self.sys.clone(),
                perl: self.perl.clone(),
                dpkg_status: self.dpkg.clone(),
            }
        }

        pub fn touch(&self, base: &Path, rel: &str, text: &str) {
            let path = base.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        pub fn write(&self, rel: &str, text: &str) {
            let path = self.root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        pub fn members(&self, node: &str, cluster: Option<&str>) {
            let mut members = serde_json::json!({ "nodename": node, "version": 3 });
            if let Some(name) = cluster {
                members["cluster"] =
                    serde_json::json!({ "name": name, "version": 2, "nodes": 2, "quorate": 1 });
            }
            self.write(".members", &members.to_string());
        }

        pub fn vmlist(&self, guests: &[(u32, &str, &str)]) {
            let ids: serde_json::Map<_, _> = guests
                .iter()
                .map(|(vmid, kind, node)| {
                    (
                        vmid.to_string(),
                        serde_json::json!({ "node": node, "type": kind, "version": 1 }),
                    )
                })
                .collect();
            self.write(
                ".vmlist",
                &serde_json::json!({ "version": 7, "ids": ids }).to_string(),
            );
        }

        pub fn scan(&self) -> Result<Option<Found>, ScanFailed> {
            scan(&self.roots(), &self.ids, false)
        }

        pub fn found(&self) -> Found {
            self.scan().expect("scan").expect("a Proxmox node")
        }

        pub fn ids(&self) -> BTreeMap<String, String> {
            let ids: Ids = serde_json::from_slice(&std::fs::read(&self.ids).unwrap()).unwrap();
            ids.into_iter()
                .map(|(key, entry)| (key, entry.uuid().to_string()))
                .collect()
        }

        pub fn raw_ids(&self) -> serde_json::Value {
            serde_json::from_slice(&std::fs::read(&self.ids).unwrap()).unwrap()
        }

        pub fn corrupt(&self) -> PathBuf {
            self.ids.with_file_name("source-ids.json.corrupt")
        }

        pub fn save_ids(&self, ids: &[(&str, &str)]) {
            let ids: BTreeMap<String, String> = ids
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            std::fs::create_dir_all(self.ids.parent().unwrap()).unwrap();
            std::fs::write(&self.ids, serde_json::to_vec(&ids).unwrap()).unwrap();
        }
    }

    pub(super) fn guest(found: &Found, vmid: u32) -> Option<&ProxmoxGuest> {
        found.proxmox.guests.iter().find(|g| g.vmid == vmid)
    }

    const SAVED: &str = "0192f0c4-7d3a-7000-8000-000000000001";

    #[test]
    fn scan_reads_the_node_guests() {
        let pve = Pve::new();
        pve.members("pve1", Some("pve-prod"));
        pve.write(
            "corosync.conf",
            "logging {\n  debug: off\n}\ntotem {\n  cluster_name: pve-prod\n  version: 2\n  interface {\n    linknumber: 0\n  }\n}\n",
        );
        pve.vmlist(&[
            (200, "lxc", "pve1"),
            (300, "qemu", "pve2"),
            (101, "qemu", "pve1"),
        ]);
        pve.write(
            "nodes/pve1/qemu-server/101.conf",
            "#name: evil\n#smbios1: uuid=11111111-1111-1111-1111-111111111111\nboot: order=scsi0\n\
             name: web-01\ntags: db;web\nsmbios1: uuid=0B6F1A2C-3D4E-4F50-8172-93A4B5C6D7E8,manufacturer=x\n\
             \n[snap1]\nname: old-web\nsmbios1: uuid=22222222-2222-2222-2222-222222222222\n",
        );
        pve.write("nodes/pve1/lxc/200.conf", "arch: amd64\nhostname: dns\n");
        pve.write("nodes/pve2/qemu-server/300.conf", "name: other\n");
        pve.write("user.cfg", "user:root@pam:1:0:::::\npool:prod::101,200::\n");

        let found = pve.found();
        assert!(!found.truncated);
        let facts = found.proxmox;
        assert_eq!(
            (facts.cluster.as_str(), facts.node.as_str()),
            ("pve-prod", "pve1")
        );
        assert_eq!(facts.guests.len(), 2);
        let (vm, ct) = (&facts.guests[0], &facts.guests[1]);
        assert_eq!(
            (
                vm.kind,
                vm.vmid,
                vm.name.as_str(),
                vm.uuid.as_str(),
                vm.pool.as_str()
            ),
            (
                GuestKind::Vm as i32,
                101,
                "web-01",
                "0b6f1a2c-3d4e-4f50-8172-93a4b5c6d7e8",
                "prod"
            )
        );
        assert_eq!(vm.tags, ["db", "web"]);
        assert_eq!(
            (ct.kind, ct.vmid, ct.name.as_str(), ct.pool.as_str()),
            (GuestKind::Ct as i32, 200, "dns", "prod")
        );
        assert!(uuid::Uuid::parse_str(&ct.uuid).is_ok());
        let debug = format!("{facts:?}");
        for leaked in ["evil", "1111", "old-web", "2222", "other"] {
            assert!(!debug.contains(leaked), "{leaked} leaked into {debug}");
        }
    }

    #[test]
    fn pve_root_without_vmlist_is_a_failed_run_not_no_proxmox() {
        let pve = Pve::new();
        std::fs::create_dir_all(pve.ids.parent().unwrap()).unwrap();
        let saved = r#"{"ct/200":"0192f0c4-7d3a-7000-8000-000000000001"}"#;
        std::fs::write(&pve.ids, saved).unwrap();
        let unchanged = || assert_eq!(std::fs::read_to_string(&pve.ids).unwrap(), saved);

        let missing = ScanRoots {
            pve: pve.root.join("missing"),
            ..pve.roots()
        };
        assert!(scan(&missing, &pve.ids, false).unwrap().is_none());
        assert!(pve.scan().is_err(), "an empty root is a stopped pmxcfs");
        unchanged();

        pve.members("pve1", None);
        assert!(pve.scan().is_err(), ".members without .vmlist");
        unchanged();

        pve.write(".vmlist", "not json");
        assert!(pve.scan().is_err(), ".vmlist that is not JSON");
        unchanged();

        pve.vmlist(&[(200, "lxc", "pve1")]);
        pve.write(".members", r#"{"version": 3}"#);
        assert!(pve.scan().is_err(), ".members without nodename");
        unchanged();
    }

    #[test]
    fn container_uuid_is_kept_and_renewed_for_a_reused_vmid() {
        let pve = Pve::new();
        pve.members("pve1", None);
        pve.write("nodes/pve1/lxc/200.conf", "hostname: dns\n");
        pve.write(
            "nodes/pve1/qemu-server/101.conf",
            "name: web\nsmbios1: uuid=0b6f1a2c-3d4e-4f50-8172-93a4b5c6d7e8\n",
        );
        pve.vmlist(&[(200, "lxc", "pve1"), (101, "qemu", "pve1")]);
        let first = guest(&pve.found(), 200).unwrap().uuid.clone();
        assert_eq!(guest(&pve.found(), 200).unwrap().uuid, first);
        assert_eq!(pve.ids().keys().collect::<Vec<_>>(), ["ct/200"]);

        pve.vmlist(&[(101, "qemu", "pve1")]);
        assert!(guest(&pve.found(), 200).is_none());
        assert!(!pve.ids().contains_key("ct/200"));
        pve.vmlist(&[(200, "lxc", "pve1"), (101, "qemu", "pve1")]);
        let reused = guest(&pve.found(), 200).unwrap().uuid.clone();
        assert_ne!(reused, first);

        pve.write("nodes/pve1/qemu-server/200.conf", "name: now-a-vm\n");
        pve.vmlist(&[(200, "qemu", "pve1")]);
        let vm = guest(&pve.found(), 200).unwrap().clone();
        assert_eq!(vm.kind, GuestKind::Vm as i32);
        assert!(!pve.ids().contains_key("ct/200"));
        pve.vmlist(&[(200, "lxc", "pve1")]);
        let back = guest(&pve.found(), 200).unwrap().uuid.clone();
        assert!(back != reused && back != vm.uuid);
    }

    #[test]
    fn failed_listing_keeps_the_state() {
        let pve = Pve::new();
        pve.members("pve1", None);
        pve.save_ids(&[("ct/200", SAVED)]);
        let before = std::fs::read(&pve.ids).unwrap();
        assert!(pve.scan().is_err());
        assert_eq!(std::fs::read(&pve.ids).unwrap(), before);

        pve.vmlist(&[(200, "lxc", "pve1")]);
        pve.write("nodes/pve1/lxc/200.conf", "hostname: dns\n");
        std::fs::write(&pve.ids, "{ broken").unwrap();
        let found = pve.found();
        assert_ne!(guest(&found, 200).unwrap().uuid, SAVED);
        assert_eq!(std::fs::read_to_string(pve.corrupt()).unwrap(), "{ broken");

        std::fs::write(&pve.ids, "[1, 2").unwrap();
        pve.found();
        assert_eq!(std::fs::read_to_string(pve.corrupt()).unwrap(), "[1, 2");
    }

    #[test]
    fn transient_read_error_keeps_the_file() {
        let pve = Pve::new();
        pve.members("pve1", None);
        pve.vmlist(&[(200, "lxc", "pve1")]);
        std::fs::create_dir_all(&pve.ids).unwrap();
        assert!(pve.scan().is_err());
        assert!(pve.ids.is_dir());
        assert!(!pve.corrupt().exists());
    }

    #[test]
    fn unwritable_state_reports_no_new_uuid() {
        let pve = Pve::new();
        pve.members("pve1", None);
        pve.save_ids(&[("ct/200", SAVED)]);
        pve.vmlist(&[(200, "lxc", "pve1"), (201, "lxc", "pve1")]);
        pve.write("nodes/pve1/lxc/200.conf", "hostname: a\n");
        pve.write("nodes/pve1/lxc/201.conf", "hostname: b\n");
        let blocker = pve.ids.with_file_name(".source-ids.json.tmp");
        std::fs::create_dir_all(&blocker).unwrap();

        let found = pve.found();
        assert_eq!(guest(&found, 200).unwrap().uuid, SAVED);
        assert!(guest(&found, 201).is_none());
        assert!(found.truncated);

        std::fs::remove_dir(&blocker).unwrap();
        let found = pve.found();
        let new = guest(&found, 201).unwrap().uuid.clone();
        assert!(!found.truncated);
        assert_eq!(pve.ids()["ct/201"], new);
        assert_eq!(guest(&pve.found(), 201).unwrap().uuid, new);
    }

    #[test]
    fn duplicate_smbios_uuid_is_not_reported() {
        let pve = Pve::new();
        pve.members("pve1", None);
        let shared = "0b6f1a2c-3d4e-4f50-8172-93a4b5c6d7e8";
        pve.write(
            "nodes/pve1/qemu-server/101.conf",
            &format!("name: a\nsmbios1: uuid={shared}\n"),
        );
        pve.write(
            "nodes/pve1/qemu-server/102.conf",
            &format!("name: b\nsmbios1: uuid={}\n", shared.to_ascii_uppercase()),
        );
        pve.write(
            "nodes/pve1/qemu-server/103.conf",
            "name: c\nsmbios1: uuid=00000000-0000-0000-0000-000000000000\n",
        );
        pve.write("nodes/pve1/qemu-server/104.conf", "name: d\n");
        pve.write("nodes/pve1/lxc/200.conf", "hostname: e\n");
        pve.vmlist(&[
            (101, "qemu", "pve1"),
            (102, "qemu", "pve1"),
            (103, "qemu", "pve1"),
            (104, "qemu", "pve1"),
            (200, "lxc", "pve1"),
        ]);
        let found = pve.found();
        assert!(found.truncated);
        let vmids: Vec<u32> = found.proxmox.guests.iter().map(|g| g.vmid).collect();
        assert_eq!(vmids, [104, 200]);
        let ids = pve.ids();
        assert!(
            ["vm/101", "vm/102", "vm/103"]
                .iter()
                .all(|k| !ids.contains_key(*k))
        );
    }

    #[test]
    fn unreadable_corosync_conf_is_a_failed_run() {
        let pve = Pve::new();
        pve.members("pve1", Some("b"));
        pve.vmlist(&[(200, "lxc", "pve1")]);
        pve.write("nodes/pve1/lxc/200.conf", "hostname: dns\n");
        pve.save_ids(&[("ct/200", SAVED), ("ct/999", SAVED)]);
        let before = std::fs::read(&pve.ids).unwrap();
        let corosync = pve.root.join("corosync.conf");

        assert!(pve.scan().is_err(), "cluster.name without corosync.conf");
        std::fs::create_dir(&corosync).unwrap();
        assert!(pve.scan().is_err(), "corosync.conf is a directory");
        std::fs::remove_dir(&corosync).unwrap();
        std::fs::File::create(&corosync)
            .unwrap()
            .set_len(MAX_PVE_FILE_BYTES + 1)
            .unwrap();
        assert!(pve.scan().is_err(), "corosync.conf over the limit");
        pve.write("corosync.conf", "totem {\n  version: 2\n}\n");
        assert!(pve.scan().is_err(), "no cluster_name");
        pve.write("corosync.conf", "totem {\n  cluster_name: a\n}\n");
        assert!(pve.scan().is_err(), "cluster_name differs from .members");
        assert_eq!(std::fs::read(&pve.ids).unwrap(), before);

        pve.write("corosync.conf", "totem {\n  cluster_name: b\n}\n");
        assert_eq!(pve.found().proxmox.cluster, "b");
    }

    #[test]
    fn vm_with_unreadable_conf_is_not_reported_under_a_new_uuid() {
        let pve = Pve::new();
        pve.members("pve1", None);
        let conf = pve.root.join("nodes/pve1/qemu-server/101.conf");
        pve.write("nodes/pve1/qemu-server/101.conf", "name: web\n");
        pve.write("nodes/pve1/lxc/200.conf", "hostname: dns\ntags: a\n");
        pve.vmlist(&[(101, "qemu", "pve1"), (200, "lxc", "pve1")]);
        let found = pve.found();
        let (vm, ct) = (
            guest(&found, 101).unwrap().uuid.clone(),
            guest(&found, 200).unwrap().uuid.clone(),
        );
        assert_eq!(pve.ids()["vm/101"], vm);

        std::fs::remove_file(&conf).unwrap();
        std::fs::create_dir(&conf).unwrap();
        let found = pve.found();
        assert!(guest(&found, 101).is_none());
        assert!(found.truncated);
        assert_eq!(pve.ids()["vm/101"], vm);
        std::fs::remove_dir(&conf).unwrap();
        pve.write("nodes/pve1/qemu-server/101.conf", "name: web\n");
        assert_eq!(guest(&pve.found(), 101).unwrap().uuid, vm);

        let big = format!("name: big\n{}\n", "x".repeat(MAX_GUEST_CONF_BYTES as usize));
        pve.write("nodes/pve1/qemu-server/104.conf", &big);
        pve.vmlist(&[
            (101, "qemu", "pve1"),
            (104, "qemu", "pve1"),
            (200, "lxc", "pve1"),
        ]);
        let found = pve.found();
        assert!(guest(&found, 104).is_none());
        assert!(found.truncated);
        assert!(!pve.ids().contains_key("vm/104"));

        std::fs::remove_file(pve.root.join("nodes/pve1/lxc/200.conf")).unwrap();
        let found = pve.found();
        let ct_now = guest(&found, 200).unwrap();
        assert_eq!(ct_now.uuid, ct);
        assert!(ct_now.name.is_empty() && ct_now.tags.is_empty());
        assert!(found.truncated);

        let mut bytes = b"#desc \xff here\nname: lossy\n".to_vec();
        bytes.extend_from_slice(b"smbios1: uuid=0b6f1a2c-3d4e-4f50-8172-93a4b5c6d7e8\n");
        std::fs::write(&conf, bytes).unwrap();
        let found = pve.found();
        let vm = guest(&found, 101).unwrap();
        assert_eq!(vm.name, "lossy");
        assert_eq!(vm.uuid, "0b6f1a2c-3d4e-4f50-8172-93a4b5c6d7e8");
    }

    #[test]
    fn source_ids_file_is_private_and_bounded() {
        use std::os::unix::fs::PermissionsExt;

        let pve = Pve::new();
        pve.members("pve1", None);
        pve.vmlist(&[(200, "lxc", "pve1")]);
        pve.write("nodes/pve1/lxc/200.conf", "hostname: dns\n");
        pve.save_ids(&[]);
        std::fs::set_permissions(&pve.ids, std::fs::Permissions::from_mode(0o644)).unwrap();
        pve.found();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&pve.ids), 0o600);

        let target = pve.ids.with_file_name("elsewhere.json");
        let planted = serde_json::to_vec(&serde_json::json!({ "ct/200": SAVED })).unwrap();
        std::fs::write(&target, &planted).unwrap();
        std::fs::remove_file(&pve.ids).unwrap();
        std::os::unix::fs::symlink(&target, &pve.ids).unwrap();
        let found = pve.found();
        assert_ne!(guest(&found, 200).unwrap().uuid, SAVED);
        let moved = std::fs::symlink_metadata(pve.corrupt()).unwrap();
        assert!(moved.file_type().is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), planted);
        let replaced = std::fs::symlink_metadata(&pve.ids).unwrap();
        assert!(replaced.file_type().is_file());
        assert_eq!(mode(&pve.ids), 0o600);

        std::fs::File::create(&pve.ids)
            .unwrap()
            .set_len(64 << 20)
            .unwrap();
        let found = pve.found();
        assert!(guest(&found, 200).is_some());
        assert_eq!(std::fs::metadata(pve.corrupt()).unwrap().len(), 64 << 20);
        assert_eq!(pve.ids().len(), 1);
    }

    fn storages(pve: &Pve) -> Vec<(String, String, Vec<String>, bool)> {
        let found = pve.found().proxmox;
        let list = found.storages.iter();
        list.map(|s| (s.id.clone(), s.r#type.clone(), s.content.clone(), s.shared))
            .collect()
    }

    #[test]
    fn storage_cfg_reads_content_nodes_and_disable() {
        let pve = Pve::new();
        pve.members("pve1", None);
        pve.vmlist(&[]);
        pve.write(
            "storage.cfg",
            "dir: local\n\tpath /var/lib/vz\n\tcontent iso,backup,vztmpl\n\n\
             lvmthin: local-lvm\n\tthinpool data\n\tvgname pve\n\tcontent rootdir,images,bogus,rootdir\n\n\
             nfs: elsewhere\n\tcontent images\n\tshared 1\n\tnodes pve2,pve3\n\n\
             dir: off\n\tdisable 1\n\tcontent rootdir\n\n\
             lodestoreprobe: probe\n\tprune-backups keep-all=1\n\n\
             nfs: both\n\tnodes pve2, pve1\n\tcontent rootdir\n\tshared 1\n",
        );
        let strings = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            storages(&pve),
            [
                (
                    "local".into(),
                    "dir".into(),
                    strings(&["iso", "backup", "vztmpl"]),
                    false
                ),
                (
                    "local-lvm".into(),
                    "lvmthin".into(),
                    strings(&["rootdir", "images"]),
                    false
                ),
                ("probe".into(), "lodestoreprobe".into(), Vec::new(), false),
                ("both".into(), "nfs".into(), strings(&["rootdir"]), true),
            ]
        );
        assert!(!pve.found().truncated);
    }

    #[test]
    fn storage_cfg_skips_invalid_ids_and_caps_the_list() {
        let pve = Pve::new();
        pve.members("pve1", None);
        pve.vmlist(&[]);
        let long = "a".repeat(MAX_NAME_BYTES + 1);
        pve.write(
            "storage.cfg",
            &format!("dir: 9lives\n\tcontent rootdir\n\ndir: {long}\n\ndir: bad/id\n\ndir: fine\n"),
        );
        let found = pve.found();
        assert!(found.truncated);
        let ids: Vec<&str> = found
            .proxmox
            .storages
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        assert_eq!(ids, ["fine"]);

        let many: String = (0..MAX_STORAGES + 5)
            .map(|i| format!("dir: s{i}\n"))
            .collect();
        pve.write("storage.cfg", &many);
        let found = pve.found();
        assert!(found.truncated);
        assert_eq!(found.proxmox.storages.len(), MAX_STORAGES);

        std::fs::remove_file(pve.root.join("storage.cfg")).unwrap();
        let found = pve.found();
        assert!(!found.truncated && found.proxmox.storages.is_empty());
    }

    const UID_A: &str = "abcdefghijklmnopqrst";
    const UID_B: &str = "bbbbbbbbbbbbbbbbbbbb";
    const SAVED2: &str = "0192f0c4-7d3a-7000-8000-000000000002";

    fn one_ct(pve: &Pve, vmid: u32, conf: &str) {
        pve.members("pve1", None);
        pve.vmlist(&[(vmid, "lxc", "pve1")]);
        pve.write(&format!("nodes/pve1/lxc/{vmid}.conf"), conf);
    }

    #[test]
    fn ct_meta_uid_recorded() {
        let pve = Pve::new();
        one_ct(
            &pve,
            200,
            "hostname: dns\nunprivileged: 1\nmeta: ctime=5,uid=abcdefghijklmnopqrst,rtime=1\n",
        );
        let found = pve.found();
        let ct = guest(&found, 200).unwrap();
        assert_eq!((ct.pve_uid.as_str(), ct.pve_rtime), (UID_A, 1));
        assert!(ct.unprivileged);
        let uuid = ct.uuid.clone();
        let saved = serde_json::json!({ "uuid": uuid, "pve_uid": UID_A });
        assert_eq!(pve.raw_ids()["ct/200"], saved);
        assert_eq!(guest(&pve.found(), 200).unwrap().uuid, uuid);

        for invalid in [
            "meta: uid=abcdefghijklmnopqrst,uid=abcdefghijklmnopqrst\n",
            "meta: uid=ABCDEFGHIJKLMNOPQRST\n",
            "meta: uid=abcdefghijklmnopqrs1\n",
            "meta: ctime=5\n",
        ] {
            one_ct(&pve, 201, &format!("hostname: x\n{invalid}"));
            let found = pve.found();
            let ct = guest(&found, 201).unwrap();
            assert_eq!((ct.pve_uid.as_str(), ct.pve_rtime), ("", 0), "{invalid}");
        }
        one_ct(
            &pve,
            202,
            "meta: uid=bbbbbbbbbbbbbbbbbbbb\n#meta: uid=cccccccccccccccccccc\nmeta: uid=abcdefghijklmnopqrst\n[snap]\nmeta: uid=dddddddddddddddddddd\n",
        );
        let found = pve.found();
        assert_eq!(guest(&found, 202).unwrap().pve_uid, UID_A);
    }

    #[test]
    fn ct_meta_uid_changed_renews() {
        let pve = Pve::new();
        let with = |uid: &str| format!("hostname: dns\nmeta: uid={uid},rtime=2\n");
        one_ct(&pve, 200, &with(UID_A));
        let first = guest(&pve.found(), 200).unwrap().uuid.clone();
        assert_eq!(guest(&pve.found(), 200).unwrap().uuid, first);

        pve.write("nodes/pve1/lxc/200.conf", &with(UID_B));
        let second = guest(&pve.found(), 200).unwrap().uuid.clone();
        assert_ne!(second, first);
        assert_eq!(pve.raw_ids()["ct/200"]["pve_uid"], UID_B);

        pve.write("nodes/pve1/lxc/200.conf", "hostname: dns\n");
        let third = guest(&pve.found(), 200).unwrap().uuid.clone();
        assert_ne!(third, second);
        assert_eq!(pve.raw_ids()["ct/200"]["pve_uid"], "");
        assert_eq!(guest(&pve.found(), 200).unwrap().uuid, third);

        pve.write("nodes/pve1/lxc/200.conf", &with(UID_A));
        let fourth = guest(&pve.found(), 200).unwrap().uuid.clone();
        assert!(![&first, &second, &third].contains(&&fourth));

        pve.write("nodes/pve1/lxc/200.conf", "hostname: dns\n");
        assert_ne!(guest(&pve.found(), 200).unwrap().uuid, fourth);
    }

    #[test]
    fn source_ids_old_format_kept() {
        let pve = Pve::new();
        pve.members("pve1", None);
        pve.vmlist(&[(200, "lxc", "pve1"), (201, "lxc", "pve1")]);
        pve.write(
            "nodes/pve1/lxc/200.conf",
            &format!("hostname: a\nmeta: uid={UID_A}\n"),
        );
        pve.write("nodes/pve1/lxc/201.conf", "hostname: b\n");
        pve.save_ids(&[("ct/200", SAVED), ("ct/201", SAVED2)]);

        let found = pve.found();
        assert_eq!(guest(&found, 200).unwrap().uuid, SAVED);
        assert_eq!(guest(&found, 201).unwrap().uuid, SAVED2);
        let raw = pve.raw_ids();
        let (a, b) = (
            serde_json::json!({"uuid": SAVED, "pve_uid": UID_A}),
            serde_json::json!({"uuid": SAVED2, "pve_uid": ""}),
        );
        assert_eq!((&raw["ct/200"], &raw["ct/201"]), (&a, &b));

        pve.write(
            "nodes/pve1/lxc/201.conf",
            &format!("hostname: b\nmeta: uid={UID_B}\n"),
        );
        assert_ne!(guest(&pve.found(), 201).unwrap().uuid, SAVED2);
        assert_eq!(guest(&pve.found(), 200).unwrap().uuid, SAVED);
    }

    #[test]
    fn ct_running_from_cgroup() {
        let pve = Pve::new();
        pve.members("pve1", None);
        pve.vmlist(&[
            (200, "lxc", "pve1"),
            (201, "lxc", "pve1"),
            (202, "lxc", "pve1"),
        ]);
        for vmid in 200..=202 {
            pve.write(&format!("nodes/pve1/lxc/{vmid}.conf"), "hostname: x\n");
        }
        std::fs::create_dir_all(pve.sys.join("fs/cgroup/lxc/200")).unwrap();
        pve.touch(&pve.sys, "fs/cgroup/lxc/202", "a file is no cgroup");
        let found = pve.found();
        let running = |vmid| guest(&found, vmid).unwrap().running;
        assert_eq!(
            (running(200), running(201), running(202)),
            (true, false, false)
        );
    }

    #[test]
    fn ct_ha_managed() {
        let pve = Pve::new();
        pve.members("pve1", None);
        pve.vmlist(&[
            (200, "lxc", "pve1"),
            (201, "lxc", "pve1"),
            (202, "lxc", "pve1"),
        ]);
        for vmid in 200..=202 {
            pve.write(&format!("nodes/pve1/lxc/{vmid}.conf"), "hostname: x\n");
        }
        let managed = |pve: &Pve| {
            let found = pve.found();
            [200, 201, 202].map(|vmid| guest(&found, vmid).unwrap().ha_managed)
        };
        assert_eq!(managed(&pve), [false; 3]);
        pve.write(
            "ha/resources.cfg",
            "ct: 200\n\tstate started\n\nvm: 201\n\tstate started\n\n#ct: 202\n",
        );
        assert_eq!(managed(&pve), [true, false, false]);

        std::fs::remove_file(pve.root.join("ha/resources.cfg")).unwrap();
        std::fs::create_dir(pve.root.join("ha/resources.cfg")).unwrap();
        assert!(pve.scan().is_err(), "an unreadable HA config fails the run");
    }

    #[test]
    fn ct_volume_lines() {
        let pve = Pve::new();
        one_ct(
            &pve,
            200,
            "hostname: x\nrootfs: local-lvm:vm-200-disk-0,size=8G\nmp0: /srv/x,mp=/data\n#mp7: hidden\n\
             mp1: tank:vm-200-disk-1,mp=/m,backup=0\nunused0: local-lvm:vm-200-disk-2\nmpx: no\n[snap]\nmp5: old\n",
        );
        let found = pve.found();
        let ct = guest(&found, 200).unwrap();
        let lines: Vec<(&str, &str)> = ct
            .volumes
            .iter()
            .map(|l| (l.key.as_str(), l.value.as_str()))
            .collect();
        assert_eq!(
            lines,
            [
                ("rootfs", "local-lvm:vm-200-disk-0,size=8G"),
                ("mp0", "/srv/x,mp=/data"),
                ("mp1", "tank:vm-200-disk-1,mp=/m,backup=0"),
                ("unused0", "local-lvm:vm-200-disk-2"),
            ]
        );
        assert!(!ct.volumes_truncated);

        let many: String = (0..70)
            .map(|i| format!("mp{i}: /d{i},mp=/m{i}\n"))
            .collect();
        one_ct(&pve, 201, &format!("hostname: y\n{many}"));
        let found = pve.found();
        let ct = guest(&found, 201).unwrap();
        assert_eq!(ct.volumes.len(), MAX_GUEST_VOLUMES);
        assert!(ct.volumes_truncated);

        let long = "x".repeat(MAX_VOLUME_LINE + 1);
        let conf = format!("hostname: z\nmp0: {long}\nmp1: /ok,mp=/ok\n");
        one_ct(&pve, 202, &conf);
        let found = pve.found();
        let ct = guest(&found, 202).unwrap();
        assert_eq!(ct.volumes.len(), 1);
        assert!(ct.volumes_truncated);
    }

    #[test]
    fn bridges_from_sysfs() {
        let pve = Pve::new();
        pve.members("pve1", None);
        pve.vmlist(&[]);
        for name in ["vmbr0", "vnet1", "toolongname-for-an-iface"] {
            std::fs::create_dir_all(pve.sys.join("class/net").join(name).join("bridge")).unwrap();
        }
        std::fs::create_dir_all(pve.sys.join("class/net/eth0")).unwrap();
        pve.touch(&pve.sys, "class/net/tap1/bridge", "a file, not a bridge");
        let found = pve.found();
        assert_eq!(found.proxmox.bridges, ["vmbr0", "vnet1"]);
        assert!(!found.truncated);

        for i in 0..300 {
            let dir = pve.sys.join(format!("class/net/br{i:03}/bridge"));
            std::fs::create_dir_all(dir).unwrap();
        }
        let found = pve.found();
        assert_eq!(found.proxmox.bridges.len(), MAX_BRIDGES);
        assert!(found.truncated);
    }

    #[test]
    fn cluster_node_markers() {
        let pve = Pve::new();
        pve.write(
            ".members",
            r#"{"nodename":"pve1","cluster":{"name":"c"},"nodelist":{"pve1":{"id":1,"online":1},"pve2":{"id":2,"online":0},"pve3":{"id":3,"online":1},"pve4":{"id":4,"online":1},"../x":{"online":1}}}"#,
        );
        pve.write("corosync.conf", "totem {\n  cluster_name: c\n}\n");
        pve.vmlist(&[]);
        let marker = "1.0 1 abcdef 9.3.1";
        pve.write("lodestore/plugin/pve1", &format!("{marker}\nsecond line\n"));
        std::os::unix::fs::symlink(
            pve.root.join("lodestore/plugin/pve1"),
            pve.root.join("lodestore/plugin/pve3"),
        )
        .unwrap();
        pve.write("lodestore/plugin/pve4", &"x".repeat(MAX_PLUGIN_MARKER + 1));

        let found = pve.found();
        let nodes: Vec<(&str, bool, &str)> = found
            .proxmox
            .nodes
            .iter()
            .map(|n| (n.name.as_str(), n.online, n.plugin_marker.as_str()))
            .collect();
        assert_eq!(
            nodes,
            [
                ("pve1", true, marker),
                ("pve2", false, ""),
                ("pve3", true, ""),
                ("pve4", true, ""),
            ]
        );
        assert!(found.truncated, "a bad node name and an oversized marker");
    }

    #[test]
    fn ct_target_facts_matches_scan() {
        let pve = Pve::new();
        let conf = format!(
            "hostname: dns\nunprivileged: 1\nmeta: uid={UID_A},rtime=7\nrootfs: l:vm-200-disk-0\nmp0: /a,mp=/b\n"
        );
        one_ct(&pve, 200, &conf);
        std::fs::create_dir_all(pve.sys.join("fs/cgroup/lxc/200")).unwrap();
        pve.write("ha/resources.cfg", "ct: 200\n");
        let found = pve.found();
        let ct = guest(&found, 200).unwrap();

        let roots = pve.roots();
        let facts = ct_target_facts(&roots, &pve.ids, 200).unwrap().unwrap();
        assert_eq!(
            (facts.name.as_str(), facts.pve_uid.as_str(), facts.pve_rtime),
            (ct.name.as_str(), ct.pve_uid.as_str(), ct.pve_rtime)
        );
        assert_eq!(facts.instance_uuid, ct.uuid);
        let volumes: Vec<(String, String)> = ct
            .volumes
            .iter()
            .map(|l| (l.key.clone(), l.value.clone()))
            .collect();
        assert_eq!(facts.volumes, volumes);
        assert_eq!(
            (
                facts.running,
                facts.ha_managed,
                facts.unprivileged,
                facts.volumes_truncated
            ),
            (
                ct.running,
                ct.ha_managed,
                ct.unprivileged,
                ct.volumes_truncated
            )
        );
        assert!(facts.running && facts.ha_managed && facts.unprivileged);

        let identity = lookup_ct(&pve.ids, 200).unwrap().unwrap();
        assert_eq!(
            (identity.uuid.as_str(), identity.pve_uid.as_str()),
            (ct.uuid.as_str(), UID_A)
        );
        assert!(lookup_ct(&pve.ids, 999).unwrap().is_none());
        assert!(ct_target_facts(&roots, &pve.ids, 999).unwrap().is_none());
        assert!(ct_target_facts(&roots, &pve.ids, 5).unwrap().is_none());

        let changed = format!("hostname: dns\nmeta: uid={UID_B},rtime=9\n");
        pve.write("nodes/pve1/lxc/200.conf", &changed);
        let stale = ct_target_facts(&roots, &pve.ids, 200).unwrap().unwrap();
        assert_eq!(
            (stale.pve_uid.as_str(), stale.instance_uuid.as_str()),
            (UID_B, "")
        );
        let renewed = guest(&pve.found(), 200).unwrap().uuid.clone();
        let again = ct_target_facts(&roots, &pve.ids, 200).unwrap().unwrap();
        assert_eq!(again.instance_uuid, renewed);
        assert_ne!(renewed, ct.uuid);
    }

    #[test]
    fn next_vmid_skips_used() {
        let pve = Pve::new();
        pve.members("pve1", None);
        pve.vmlist(&[
            (100, "lxc", "pve1"),
            (101, "qemu", "pve2"),
            (103, "lxc", "pve1"),
        ]);
        pve.write("nodes/pve1/lxc/100.conf", "hostname: a\n");
        pve.write("nodes/pve1/lxc/103.conf", "hostname: b\n");
        assert_eq!(pve.found().proxmox.next_vmid, 102);

        pve.write("datacenter.cfg", "keyboard: de\nnext-id: lower=500\n");
        assert_eq!(pve.found().proxmox.next_vmid, 500);

        pve.vmlist(&[(500, "qemu", "pve2")]);
        assert_eq!(pve.found().proxmox.next_vmid, 501);

        pve.write("datacenter.cfg", "next-id: lower=500,upper=501\n");
        assert_eq!(pve.found().proxmox.next_vmid, 0);
    }

    #[test]
    fn plugin_installed_needs_files_and_marker() {
        let pve = Pve::new();
        pve.members("pve1", None);
        pve.vmlist(&[]);
        let facts = |pve: &Pve| {
            let p = pve.found().proxmox;
            (p.plugin_installed, p.plugin_current)
        };
        assert_eq!(facts(&pve), (false, false));
        pve.touch(&pve.perl, PLUGIN_FILE, PLUGIN_SOURCE);
        pve.touch(&pve.perl, PROVIDER_FILE, PROVIDER_SOURCE);
        assert_eq!(facts(&pve), (false, false), "no marker yet");
        let hash = embedded_plugin_hash();
        pve.write("lodestore/plugin/pve1", &format!("1.0 1 {hash} 9.3.1\n"));
        assert_eq!(facts(&pve), (true, true));

        pve.touch(&pve.perl, PLUGIN_FILE, "package x;");
        assert_eq!(facts(&pve), (false, false), "the files differ from the marker");
        pve.touch(&pve.perl, PROVIDER_FILE, "package y;");
        let old = plugin_hash(b"package x;", b"package y;");
        pve.write("lodestore/plugin/pve1", &format!("1.0 1 {old} 9.3.1\n"));
        assert_eq!(facts(&pve), (true, false), "another agent's plugin");

        let provider = pve.perl.join(PROVIDER_FILE);
        std::fs::remove_file(&provider).unwrap();
        assert!(!pve.found().proxmox.plugin_installed);
        std::os::unix::fs::symlink(pve.perl.join(PLUGIN_FILE), &provider).unwrap();
        assert!(
            !pve.found().proxmox.plugin_installed,
            "a symlink is no file"
        );
    }

    #[test]
    fn pve_version_comes_from_the_dpkg_status() {
        let pve = Pve::new();
        pve.members("pve1", None);
        pve.vmlist(&[]);
        assert_eq!(pve.found().proxmox.pve_version, "");
        let status = "Package: pve-manager-old\nStatus: install ok installed\nVersion: 1.0\n\n\
                      Package: pve-manager\nStatus: deinstall ok config-files\nVersion: 8.0.0\n\n\
                      Package: other\nStatus: install ok installed\nVersion: 2.0\n\n\
                      Package: pve-manager\nStatus: install ok installed\nVersion: 9.3.1\nDescription: x\n";
        std::fs::write(&pve.dpkg, status).unwrap();
        assert_eq!(pve.found().proxmox.pve_version, "9.3.1");
        std::fs::write(&pve.dpkg, status.replace("9.3.1", "9 3 1")).unwrap();
        assert_eq!(pve.found().proxmox.pve_version, "");
    }

    #[test]
    fn guest_in_place_flag_is_passed_through() {
        let pve = Pve::new();
        pve.members("pve1", None);
        pve.vmlist(&[]);
        let facts = |allowed| {
            let found = scan(&pve.roots(), &pve.ids, allowed).unwrap().unwrap();
            found.proxmox
        };
        assert!(!facts(false).guest_in_place_allowed);
        assert!(facts(true).guest_in_place_allowed);
    }

    #[test]
    fn node_outside_a_cluster() {
        let pve = Pve::new();
        pve.members("pve1", None);
        pve.vmlist(&[]);
        let facts = pve.found().proxmox;
        assert_eq!((facts.cluster.as_str(), facts.node.as_str()), ("", "pve1"));
    }

    #[test]
    fn guest_order_is_stable_across_runs() {
        let pve = Pve::new();
        pve.members("pve1", None);
        let mut vmids: Vec<u32> = (100..1600).collect();
        vmids.reverse();
        vmids.swap(3, 1200);
        let entries: Vec<String> = vmids
            .iter()
            .map(|v| format!(r#""{v}":{{"node":"pve1","type":"lxc","version":1}}"#))
            .collect();
        pve.write(
            ".vmlist",
            &format!(r#"{{"ids":{{{}}}}}"#, entries.join(",")),
        );
        let first = pve.found();
        let second = pve.found();
        assert!(first.truncated && second.truncated);
        let listed = |f: &Found| -> Vec<(u32, String)> {
            let guests = f.proxmox.guests.iter();
            guests.map(|g| (g.vmid, g.uuid.clone())).collect()
        };
        assert_eq!(listed(&first), listed(&second));
        let kept: Vec<u32> = listed(&first).iter().map(|(v, _)| *v).collect();
        assert_eq!(kept, (100..1100).collect::<Vec<_>>());
        assert_eq!(pve.ids().len(), MAX_GUESTS);
    }

    #[test]
    fn guests_past_the_cap_keep_their_ids() {
        let pve = Pve::new();
        pve.members("pve1", None);
        let list = |vmids: std::ops::Range<u32>| {
            let entries: Vec<String> = vmids
                .map(|v| format!(r#""{v}":{{"node":"pve1","type":"lxc","version":1}}"#))
                .collect();
            pve.write(
                ".vmlist",
                &format!(r#"{{"ids":{{{}}}}}"#, entries.join(",")),
            );
        };
        let last = 200 + u32::try_from(MAX_GUESTS).unwrap() - 1;
        let uuid_of = |f: &Found| {
            f.proxmox
                .guests
                .iter()
                .find(|g| g.vmid == last)
                .map(|g| g.uuid.clone())
        };
        list(200..last + 1);
        let before = uuid_of(&pve.found()).unwrap();
        list(150..last + 1);
        assert_eq!(uuid_of(&pve.found()), None);
        list(200..last + 1);
        assert_eq!(uuid_of(&pve.found()), Some(before));

        let pve = Pve::new();
        pve.members("pve1", None);
        pve.vmlist(&[
            (100, "lxc", "pve1"),
            (101, "lxc", "pve1"),
            (102, "lxc", "pve1"),
        ]);
        let many: Vec<String> = (0..25).map(|i| format!("t{i}")).collect();
        pve.write(
            "nodes/pve1/lxc/100.conf",
            &format!("hostname: a\ntags: {}\n", many.join(";")),
        );
        let long = "x".repeat(MAX_NAME_BYTES + 1);
        pve.write(
            "nodes/pve1/lxc/101.conf",
            &format!("hostname: b\ntags: ok;{long}\n"),
        );
        pve.write("nodes/pve1/lxc/102.conf", "hostname: c\n");
        pve.write("user.cfg", &format!("pool:{long}::102::\npool:p::101::\n"));
        let found = pve.found();
        assert!(found.truncated);
        assert_eq!(guest(&found, 100).unwrap().tags, many[..MAX_GUEST_TAGS]);
        assert_eq!(guest(&found, 101).unwrap().tags, ["ok"]);
        assert_eq!(guest(&found, 101).unwrap().pool, "p");
        assert_eq!(guest(&found, 102).unwrap().pool, "");
    }

    #[test]
    fn oversized_pve_files_mark_the_list_truncated() {
        let pve = Pve::new();
        pve.members("pve1", Some("prod"));
        pve.write("corosync.conf", "totem {\n  cluster_name: prod\n}\n");
        std::fs::File::create(pve.root.join(".vmlist"))
            .unwrap()
            .set_len(MAX_PVE_FILE_BYTES + 1)
            .unwrap();
        let found = pve.found();
        assert!(found.truncated);
        assert_eq!(
            (found.proxmox.node.as_str(), found.proxmox.cluster.as_str()),
            ("pve1", "prod")
        );
        assert!(found.proxmox.guests.is_empty());

        pve.vmlist(&[(100, "lxc", "pve1"), (101, "qemu", "pve1")]);
        let snapshots: String = (0..3000)
            .map(|i| format!("[snap{i}]\nname: old{i}\n{}\n", "#".repeat(1000)))
            .collect();
        pve.write(
            "nodes/pve1/qemu-server/101.conf",
            &format!("name: web\ntags: a\n{snapshots}"),
        );
        pve.write("nodes/pve1/lxc/100.conf", "hostname: dns\n");
        std::fs::File::create(pve.root.join("user.cfg"))
            .unwrap()
            .set_len(MAX_PVE_FILE_BYTES + 1)
            .unwrap();
        let found = pve.found();
        assert!(found.truncated);
        assert!(found.proxmox.guests.iter().all(|g| g.pool.is_empty()));
        let vm = guest(&found, 101).unwrap();
        assert_eq!(
            (vm.name.as_str(), vm.tags.as_slice()),
            ("web", &["a".to_string()][..])
        );

        let pve = Pve::new();
        pve.members("pve1", None);
        let guests: Vec<(u32, &str, &str)> = (100..110).map(|v| (v, "lxc", "pve1")).collect();
        pve.vmlist(&guests);
        for vmid in 100..110 {
            let pad = "#".repeat(10_000);
            pve.write(
                &format!("nodes/pve1/lxc/{vmid}.conf"),
                &format!("hostname: h{vmid}\n{pad}\n"),
            );
        }
        let all = pve.found();
        assert!(!all.truncated);
        let saved = pve.ids();
        assert_eq!(saved.len(), 10);
        let found = scan_with(&pve.roots(), &pve.ids, false, 35_000)
            .unwrap()
            .unwrap();
        assert!(found.truncated);
        let reported = found.proxmox.guests.len();
        assert!(reported > 0 && reported < 10, "{reported}");
        assert_eq!(pve.ids(), saved);
    }

    struct Counting<R> {
        inner: R,
        read: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl<R: io::Read> io::Read for Counting<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read.set(self.read.get() + n);
            Ok(n)
        }
    }

    #[test]
    fn main_section_stops_at_the_first_bracket() {
        let main = "name: web\n".repeat(20);
        let mut data = main.clone().into_bytes();
        data.extend_from_slice(b"[snap1]\nname: old\n");
        data.resize(3 << 20, b'x');
        let read = std::rc::Rc::default();
        let reader = BufReader::new(Counting {
            inner: io::Cursor::new(data),
            read: std::rc::Rc::clone(&read),
        });
        let capacity = reader.capacity();
        assert_eq!(
            read_main_section(reader, MAX_GUEST_CONF_BYTES).unwrap(),
            main
        );
        assert!(read.get() <= main.len() + capacity, "read {}", read.get());

        let long = "a: b\n".repeat(MAX_GUEST_CONF_BYTES as usize);
        let error = read_main_section(io::Cursor::new(long), MAX_GUEST_CONF_BYTES).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);
    }

    #[test]
    fn path_segments_come_only_from_numbers_and_a_checked_node_name() {
        let pve = Pve::new();
        pve.members("../../root", None);
        pve.vmlist(&[(100, "lxc", "../../root")]);
        assert!(pve.scan().is_err());

        pve.members("pve1", None);
        let entry = r#"{"node":"pve1","type":"qemu","version":1}"#;
        let keys = ["101/../../x", "abc", "99", "1000000000", "0101", "101"];
        let ids: Vec<String> = keys.iter().map(|k| format!(r#""{k}":{entry}"#)).collect();
        pve.write(".vmlist", &format!(r#"{{"ids":{{{}}}}}"#, ids.join(",")));
        pve.write("nodes/pve1/qemu-server/101.conf", "name: web\n");
        pve.write("x.conf", "name: leak\n");
        pve.write("nodes/pve1/qemu-server/99.conf", "name: leak\n");
        let found = pve.found();
        assert!(found.truncated);
        let vmids: Vec<u32> = found.proxmox.guests.iter().map(|g| g.vmid).collect();
        assert_eq!(vmids, [101]);
        assert!(!format!("{:?}", found.proxmox).contains("leak"));
    }
}
