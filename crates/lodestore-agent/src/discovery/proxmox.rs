//! Proxmox VE node and guests from the files `pmxcfs` serves under
//! `/etc/pve`. Nothing is written there and no process is started.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, BufRead, BufReader};
use std::path::Path;

use lodestore_proto::agent::v1::{GuestKind, ProxmoxFacts, ProxmoxGuest};
use serde::Deserialize;

use super::{MAX_GUEST_CONF_BYTES, MAX_PVE_FILE_BYTES, MAX_STATE_BYTES, ScanFailed};
use super::{open_nofollow, read_bounded};
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

fn load_ids(path: &Path) -> Result<Ids, ScanFailed> {
    match read_bounded(path, MAX_STATE_BYTES) {
        Ok(bytes) => {
            let mut ids: Ids = serde_json::from_slice(&bytes)
                .map_err(|e| ScanFailed(format!("{} cannot be parsed: {e}", path.display())))?;
            ids.retain(|_, uuid| uuid::Uuid::parse_str(uuid).is_ok());
            Ok(ids)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Ids::new()),
        Err(e) => Err(failed("the source id file")(e)),
    }
}

fn new_uuid() -> String {
    uuid::Uuid::now_v7().hyphenated().to_string()
}

/// `Ok(None)`: `root` does not exist, this is not a Proxmox node. A `root`
/// that exists without the cluster files is a stopped `pmxcfs`, not "no
/// Proxmox", and fails the run.
pub(crate) fn scan(root: &Path, ids_path: &Path) -> Result<Option<Found>, ScanFailed> {
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
    let (node, _member_cluster) = parse_members(&members)?;
    let cluster = match read_bounded(&root.join("corosync.conf"), MAX_PVE_FILE_BYTES) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(failed("corosync.conf")(e)),
        Ok(bytes) => parse_cluster_name(&String::from_utf8_lossy(&bytes))
            .ok_or_else(|| ScanFailed("corosync.conf names no cluster".into()))?,
    };
    let vmlist =
        read_bounded(&root.join(".vmlist"), MAX_PVE_FILE_BYTES).map_err(failed(".vmlist"))?;
    let (listed, mut truncated) = parse_vmlist(&vmlist, &node)?;
    let old_ids = load_ids(ids_path)?;

    let pools = match read_bounded(&root.join("user.cfg"), MAX_PVE_FILE_BYTES) {
        Ok(bytes) => parse_pools(&String::from_utf8_lossy(&bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => HashMap::new(),
        Err(_) => {
            truncated = true;
            HashMap::new()
        }
    };

    let mut ids: Ids = old_ids
        .iter()
        .filter(|(key, _)| listed.iter().any(|&(k, v)| map_key(k, v) == **key))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let node_dir = root.join("nodes").join(&node);
    let mut guests = Vec::new();
    for (kind, vmid) in listed {
        let dir = if kind == GuestKind::Vm {
            "qemu-server"
        } else {
            "lxc"
        };
        let conf = match read_conf(&node_dir.join(dir).join(format!("{vmid}.conf"))) {
            Ok(text) => parse_guest_conf(&text),
            Err(_) => {
                truncated = true;
                GuestConf::default()
            }
        };
        let key = map_key(kind, vmid);
        let uuid = match conf
            .smbios_uuid
            .as_deref()
            .filter(|_| kind == GuestKind::Vm)
        {
            Some(own) => {
                ids.remove(&key);
                own.to_ascii_lowercase()
            }
            None => ids.entry(key).or_insert_with(new_uuid).clone(),
        };
        let name = if kind == GuestKind::Vm {
            conf.name
        } else {
            conf.hostname
        };
        guests.push(ProxmoxGuest {
            kind: kind as i32,
            vmid,
            name: name.unwrap_or_default(),
            uuid,
            pool: pools.get(&vmid).cloned().unwrap_or_default(),
            tags: conf.tags,
        });
    }
    if ids != old_ids {
        let json = serde_json::to_vec(&ids).map_err(|e| ScanFailed(e.to_string()))?;
        write_private_file(ids_path, &json).map_err(failed("the source id file"))?;
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
    }

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
}
