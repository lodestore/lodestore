//! Proxmox VE node and guests from the files `pmxcfs` serves under
//! `/etc/pve`. Nothing is written there and no process is started.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, BufRead, BufReader};
use std::path::Path;

use lodestore_proto::agent::v1::{GuestKind, ProxmoxFacts, ProxmoxGuest};
use serde::Deserialize;

use super::{
    MAX_GUEST_CONF_BYTES, MAX_GUEST_TAGS, MAX_GUESTS, MAX_NAME_BYTES, MAX_PVE_FILE_BYTES,
    MAX_SCAN_BYTES, MAX_STATE_BYTES, ScanFailed,
};
use super::{clip, open_nofollow, read_bounded};
use crate::identity::write_private_file;

const VMIDS: std::ops::RangeInclusive<u32> = 100..=999_999_999;

pub(crate) struct Found {
    pub proxmox: ProxmoxFacts,
    pub truncated: bool,
}

#[derive(Deserialize)]
struct Members {
    nodename: String,
    cluster: Option<MembersCluster>,
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
}

fn failed(what: &str) -> impl FnOnce(io::Error) -> ScanFailed + '_ {
    move |e| ScanFailed(format!("{what} cannot be read: {e}"))
}

fn parse_members(bytes: &[u8]) -> Result<(String, Option<String>), ScanFailed> {
    let members: Members = serde_json::from_slice(bytes)
        .map_err(|e| ScanFailed(format!(".members cannot be parsed: {e}")))?;
    let valid = (1..=63).contains(&members.nodename.len())
        && members
            .nodename
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if !valid {
        return Err(ScanFailed(format!(
            ".members names the node {:?}, which is not a valid node name",
            members.nodename
        )));
    }
    Ok((members.nodename, members.cluster.map(|c| c.name)))
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

/// The guests of `node` as `(kind, vmid)`, and whether an entry of this node
/// had to be skipped.
fn parse_vmlist(bytes: &[u8], node: &str) -> Result<(Vec<(GuestKind, u32)>, bool), ScanFailed> {
    let list: VmList = serde_json::from_slice(bytes)
        .map_err(|e| ScanFailed(format!(".vmlist cannot be parsed: {e}")))?;
    let mut skipped = false;
    let mut guests = Vec::new();
    for (key, entry) in list.ids {
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
    Ok((guests, skipped))
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
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key {
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
    conf
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

type Ids = BTreeMap<String, String>;

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
            ids.retain(|_, uuid| uuid::Uuid::parse_str(uuid).is_ok());
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

/// `Ok(None)`: `root` does not exist, this is not a Proxmox node. A `root`
/// that exists without the cluster files is a stopped `pmxcfs`, not "no
/// Proxmox", and fails the run.
pub(crate) fn scan(root: &Path, ids_path: &Path) -> Result<Option<Found>, ScanFailed> {
    scan_with(root, ids_path, MAX_SCAN_BYTES)
}

/// `budget` caps the bytes read under `root` in one run; guests whose config
/// is past it are not reported and keep their saved ids.
fn scan_with(root: &Path, ids_path: &Path, budget: u64) -> Result<Option<Found>, ScanFailed> {
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
    let (node, member_cluster) = parse_members(&members)?;
    let cluster = cluster_name(root, member_cluster)?;
    let (mut listed, mut truncated) = match read_bounded(&root.join(".vmlist"), MAX_PVE_FILE_BYTES)
    {
        Ok(bytes) => {
            spent += bytes.len() as u64;
            parse_vmlist(&bytes, &node)?
        }
        Err(e) if e.kind() == io::ErrorKind::FileTooLarge => {
            tracing::warn!("/etc/pve/.vmlist is too large; reporting the node without guests");
            let proxmox = ProxmoxFacts {
                cluster,
                node,
                guests: Vec::new(),
            };
            return Ok(Some(Found {
                proxmox,
                truncated: true,
            }));
        }
        Err(e) => return Err(failed(".vmlist")(e)),
    };
    if listed.len() > MAX_GUESTS {
        listed.truncate(MAX_GUESTS);
        truncated = true;
    }
    let old_ids = load_ids(ids_path)?;

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
        .filter(|(key, _)| listed.iter().any(|&(k, v)| map_key(k, v) == **key))
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
            None => match ids.get(&key) {
                Some(uuid) => uuid.clone(),
                None => {
                    let uuid = new_uuid();
                    ids.insert(key.clone(), uuid.clone());
                    fresh.push(uuid.clone());
                    uuid
                }
            },
        };
        let conf = conf.unwrap_or_else(|| {
            truncated = true;
            GuestConf::default()
        });
        let name = if kind == GuestKind::Vm {
            conf.name
        } else {
            conf.hostname
        };
        let pool = match pools.get(&vmid) {
            Some(pool) if pool.len() > MAX_NAME_BYTES => {
                truncated = true;
                String::new()
            }
            pool => pool.cloned().unwrap_or_default(),
        };
        guests.push(ProxmoxGuest {
            kind: kind as i32,
            vmid,
            name: clip(name.unwrap_or_default()),
            uuid,
            pool,
            tags: guest_tags(conf.tags, &mut truncated),
        });
    }
    if ids != old_ids {
        let written = serde_json::to_vec(&ids)
            .map_err(io::Error::other)
            .and_then(|json| write_private_file(ids_path, &json));
        if let Err(error) = written {
            tracing::warn!(
                %error,
                "the source id file cannot be written; guests with new ids are not reported until it can"
            );
            truncated |= !fresh.is_empty();
            guests.retain(|g| !fresh.contains(&g.uuid));
        }
    }
    Ok(Some(Found {
        proxmox: ProxmoxFacts {
            cluster,
            node,
            guests,
        },
        truncated,
    }))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    pub(super) struct Pve {
        _tmp: tempfile::TempDir,
        pub root: PathBuf,
        pub ids: PathBuf,
    }

    impl Pve {
        pub fn new() -> Pve {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("pve");
            let ids = tmp.path().join("state").join("source-ids.json");
            std::fs::create_dir_all(&root).unwrap();
            Pve {
                _tmp: tmp,
                root,
                ids,
            }
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
            scan(&self.root, &self.ids)
        }

        pub fn found(&self) -> Found {
            self.scan().expect("scan").expect("a Proxmox node")
        }

        pub fn ids(&self) -> Ids {
            serde_json::from_slice(&std::fs::read(&self.ids).unwrap()).unwrap()
        }

        pub fn corrupt(&self) -> PathBuf {
            self.ids.with_file_name("source-ids.json.corrupt")
        }

        pub fn save_ids(&self, ids: &[(&str, &str)]) {
            let ids: Ids = ids
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

        assert!(scan(&pve.root.join("missing"), &pve.ids).unwrap().is_none());
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
        let found = scan_with(&pve.root, &pve.ids, 35_000).unwrap().unwrap();
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
