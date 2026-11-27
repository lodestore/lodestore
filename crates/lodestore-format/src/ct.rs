//! Proxmox VE container snapshots (ADR 0008): the tree of the container
//! below `/fs`, its configuration below `/config`, the snapshot metadata and
//! the id map between the node's and the container's view of owners.
//!
//! The layout is a one-way decision: `ct-vectors.json` freezes it, and a
//! change needs a new ADR. Everything read from a snapshot is untrusted.

use std::io::Write;

use crate::crypto::BlobId;
use crate::error::{Error, Result};
use crate::pb;
use crate::reader::Reader;
use crate::source::{self, Meta, SourceType};
use crate::store::BlobStore;

pub const CT_LAYOUT_VERSION: &str = "1";
pub const META_CT_LAYOUT: &str = "ct_layout";
pub const META_CT_MODE: &str = "ct_mode";
pub const META_CT_UNPRIVILEGED: &str = "ct_unprivileged";
pub const META_CT_IDMAP: &str = "ct_idmap";

pub const FS_DIR: &[u8] = b"fs";
pub const CONFIG_DIR: &[u8] = b"config";
pub const CONFIG_FILE: &[u8] = b"pct.conf";
pub const FIREWALL_FILE: &[u8] = b"pct.fw";
/// The one entry of `Snapshot.paths` of a container snapshot.
pub const FS_PATH: &[u8] = b"/fs";

/// The largest `pct.conf` and `pct.fw` a snapshot may hold.
pub const MAX_CT_CONFIG_BYTES: usize = 64 << 10;
/// The longest canonical id map; a container with a longer one is not
/// backed up.
pub const MAX_CT_IDMAP_LEN: usize = 256;
/// The code of the error for an id map longer than [`MAX_CT_IDMAP_LEN`].
pub const CT_IDMAP_UNSUPPORTED: &str = "ct_idmap_unsupported";

fn bad(reason: impl Into<String>) -> Error {
    Error::corrupt("container snapshot", reason)
}

/// The mode vzdump applied to the container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtMode {
    Snapshot,
    Suspend,
    Stop,
}

impl CtMode {
    pub const ALL: [CtMode; 3] = [CtMode::Snapshot, CtMode::Suspend, CtMode::Stop];

    pub fn as_str(self) -> &'static str {
        match self {
            CtMode::Snapshot => "snapshot",
            CtMode::Suspend => "suspend",
            CtMode::Stop => "stop",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        CtMode::ALL.into_iter().find(|m| m.as_str() == text)
    }
}

/// Which ids an id map entry covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IdKind {
    User,
    Group,
}

impl IdKind {
    fn letter(self) -> &'static str {
        match self {
            IdKind::User => "u",
            IdKind::Group => "g",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "u" => Some(IdKind::User),
            "g" => Some(IdKind::Group),
            _ => None,
        }
    }
}

/// `count` ids from `ct` in the container are `host` and up on the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct IdRange {
    pub kind: IdKind,
    pub ct: u32,
    pub host: u32,
    pub count: u32,
}

impl IdRange {
    fn ct_end(&self) -> u64 {
        u64::from(self.ct) + u64::from(self.count)
    }

    fn host_end(&self) -> u64 {
        u64::from(self.host) + u64::from(self.count)
    }
}

/// The id map of a container: ranges of user and group ids, ordered by kind
/// (users first) and then by container id, none of them overlapping another
/// of its kind on either side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdMap {
    ranges: Vec<IdRange>,
}

impl IdMap {
    /// Checks and orders `ranges`.
    pub fn new(ranges: Vec<IdRange>) -> Result<Self> {
        IdMap::build(ranges).map_err(Error::InvalidInput)
    }

    fn build(mut ranges: Vec<IdRange>) -> std::result::Result<Self, String> {
        if ranges.is_empty() {
            return Err("an id map has at least one entry".into());
        }
        const LIMIT: u64 = 1 << 32;
        for r in &ranges {
            if r.count == 0 {
                return Err("an id map entry covers at least one id".into());
            }
            if r.ct_end() > LIMIT || r.host_end() > LIMIT {
                return Err("an id map entry runs past the largest id".into());
            }
        }
        ranges.sort();
        for kind in [IdKind::User, IdKind::Group] {
            let mut of_kind: Vec<&IdRange> = ranges.iter().filter(|r| r.kind == kind).collect();
            // Sorted by `ct` already.
            if of_kind
                .windows(2)
                .any(|w| matches!(w, [a, b] if a.ct_end() > u64::from(b.ct)))
            {
                return Err("id map entries overlap in the container".into());
            }
            of_kind.sort_by_key(|r| r.host);
            if of_kind
                .windows(2)
                .any(|w| matches!(w, [a, b] if a.host_end() > u64::from(b.host)))
            {
                return Err("id map entries overlap on the node".into());
            }
        }
        Ok(IdMap { ranges })
    }

    /// The map Proxmox VE gives an unprivileged container without
    /// `lxc.idmap` lines.
    pub fn default_unprivileged() -> Self {
        IdMap {
            ranges: vec![
                IdRange {
                    kind: IdKind::User,
                    ct: 0,
                    host: 100_000,
                    count: 65_536,
                },
                IdRange {
                    kind: IdKind::Group,
                    ct: 0,
                    host: 100_000,
                    count: 65_536,
                },
            ],
        }
    }

    pub fn ranges(&self) -> &[IdRange] {
        &self.ranges
    }

    /// The container's view of a node id, `None` outside the map.
    pub fn to_ct(&self, kind: IdKind, host: u32) -> Option<u32> {
        self.ranges
            .iter()
            .filter(|r| r.kind == kind && host >= r.host && u64::from(host) < r.host_end())
            .find_map(|r| r.ct.checked_add(host - r.host))
    }

    /// The node's view of a container id, `None` outside the map.
    pub fn to_host(&self, kind: IdKind, ct: u32) -> Option<u32> {
        self.ranges
            .iter()
            .filter(|r| r.kind == kind && ct >= r.ct && u64::from(ct) < r.ct_end())
            .find_map(|r| r.host.checked_add(ct - r.ct))
    }

    /// `u:{ct}:{host}:{count}` entries and then `g:` entries, each group in
    /// ascending order of `ct`, separated by commas. Longer than
    /// [`MAX_CT_IDMAP_LEN`] fails with [`CT_IDMAP_UNSUPPORTED`].
    pub fn canonical(&self) -> Result<String> {
        let text = self.render();
        if text.len() > MAX_CT_IDMAP_LEN {
            return Err(Error::InvalidInput(format!(
                "{CT_IDMAP_UNSUPPORTED}: the container's id map takes {} bytes, \
                 more than the {MAX_CT_IDMAP_LEN} a snapshot records",
                text.len()
            )));
        }
        Ok(text)
    }

    fn render(&self) -> String {
        self.ranges
            .iter()
            .map(|r| format!("{}:{}:{}:{}", r.kind.letter(), r.ct, r.host, r.count))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// The inverse of [`IdMap::canonical`]: only the canonical spelling of
    /// a map is accepted.
    pub fn parse_canonical(text: &str) -> Result<Self> {
        if text.len() > MAX_CT_IDMAP_LEN {
            return Err(bad("the id map is too long"));
        }
        let mut ranges = Vec::new();
        for entry in text.split(',') {
            let mut parts = entry.split(':');
            let (Some(kind), Some(ct), Some(host), Some(count), None) = (
                parts.next(),
                parts.next(),
                parts.next(),
                parts.next(),
                parts.next(),
            ) else {
                return Err(bad("an id map entry is not kind:ct:host:count"));
            };
            let kind =
                IdKind::parse(kind).ok_or_else(|| bad("an id map entry kind is not u or g"))?;
            let number = |s: &str| decimal(s).ok_or_else(|| bad("an id map number is malformed"));
            ranges.push(IdRange {
                kind,
                ct: number(ct)?,
                host: number(host)?,
                count: number(count)?,
            });
        }
        let map = IdMap::build(ranges).map_err(bad)?;
        if map.render() != text {
            return Err(bad("the id map is not in canonical form"));
        }
        Ok(map)
    }

    /// The map of a container from its `pct.conf`, as Proxmox VE builds it:
    /// the `lxc.idmap` lines when there are any, otherwise the default map
    /// for an unprivileged container, otherwise none.
    pub fn from_config(config: &str) -> Result<Option<Self>> {
        let mut ranges = Vec::new();
        for (key, value) in config_lines(config) {
            if key != "lxc.idmap" {
                continue;
            }
            let invalid = || Error::InvalidInput(format!("failed to parse idmap: {value}"));
            let mut parts = value.split_ascii_whitespace();
            let (Some(kind), Some(ct), Some(host), Some(count), None) = (
                parts.next(),
                parts.next(),
                parts.next(),
                parts.next(),
                parts.next(),
            ) else {
                return Err(invalid());
            };
            let kind = IdKind::parse(kind).ok_or_else(invalid)?;
            let number = |s: &str| {
                if s.bytes().all(|b| b.is_ascii_digit()) {
                    s.parse::<u32>().map_err(|_| invalid())
                } else {
                    Err(invalid())
                }
            };
            ranges.push(IdRange {
                kind,
                ct: number(ct)?,
                host: number(host)?,
                count: number(count)?,
            });
        }
        if !ranges.is_empty() {
            return IdMap::new(ranges).map(Some);
        }
        Ok(unprivileged(config)?.then(IdMap::default_unprivileged))
    }
}

/// A decimal without leading zeros.
fn decimal(value: &str) -> Option<u32> {
    let canonical = !value.is_empty()
        && value.bytes().all(|b| b.is_ascii_digit())
        && (value == "0" || !value.starts_with('0'));
    if canonical { value.parse().ok() } else { None }
}

/// Whether `pct.conf` says `unprivileged`, read as Proxmox VE reads a boolean.
pub fn unprivileged(config: &str) -> Result<bool> {
    let mut flag = false;
    for (key, value) in config_lines(config) {
        if key != "unprivileged" {
            continue;
        }
        flag = match value.to_ascii_lowercase().as_str() {
            "1" | "on" | "yes" | "true" => true,
            "0" | "off" | "no" | "false" => false,
            _ => {
                return Err(Error::InvalidInput(format!(
                    "unprivileged: {value:?} is not a boolean"
                )));
            }
        };
    }
    Ok(flag)
}

/// The `key: value` pairs of the main section of a `pct.conf` (everything
/// before the first line that starts with `[`), by the rules of
/// `PVE::LXC::Config::parse_pct_config`: `#` lines are the description,
/// `lxc.*` keys may also be written `key = value`, other keys are
/// `[a-z][a-z_]*\d*` with a non-empty value. Lines that fit none of that
/// are skipped. Nothing is allocated.
pub fn config_lines(text: &str) -> impl Iterator<Item = (&str, &str)> {
    text.split('\n')
        .take_while(|line| !line.starts_with('['))
        .filter_map(config_line)
}

fn config_line(line: &str) -> Option<(&str, &str)> {
    if line.starts_with('#') {
        return None;
    }
    if line.starts_with("lxc.") {
        let end = line
            .bytes()
            .skip(4)
            .position(|b| !matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.'))
            .map_or(line.len(), |p| p + 4);
        if end == 4 {
            return None;
        }
        let (key, rest) = line.split_at_checked(end)?;
        let value = match rest.strip_prefix(':') {
            Some(value) => value,
            None => rest.trim_ascii_start().strip_prefix('=')?,
        };
        return Some((key, value.trim_ascii()));
    }
    let bytes = line.as_bytes();
    if !bytes.first().is_some_and(u8::is_ascii_lowercase) {
        return None;
    }
    let letters = bytes
        .iter()
        .position(|b| !(b.is_ascii_lowercase() || *b == b'_'))
        .unwrap_or(bytes.len());
    let digits = bytes
        .get(letters..)?
        .iter()
        .position(|b| !b.is_ascii_digit())
        .map_or(bytes.len(), |p| p + letters);
    let (key, rest) = line.split_at_checked(digits)?;
    let value = rest.strip_prefix(':')?.trim_ascii();
    (!value.is_empty()).then_some((key, value))
}

/// The metadata of a container snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtMeta {
    pub mode: CtMode,
    pub unprivileged: bool,
    /// Required for an unprivileged container; a privileged one has a map
    /// only when its configuration has `lxc.idmap` lines.
    pub idmap: Option<IdMap>,
}

impl CtMeta {
    /// The metadata of a backup from the configuration vzdump passed.
    pub fn from_config(mode: CtMode, config: &str) -> Result<Self> {
        Ok(CtMeta {
            mode,
            unprivileged: unprivileged(config)?,
            idmap: IdMap::from_config(config)?,
        })
    }

    /// The keys from ADR 0008 § Names. Fails for an unprivileged container
    /// without a map and for a map longer than [`MAX_CT_IDMAP_LEN`].
    pub fn to_meta(&self) -> Result<Meta> {
        let mut meta = Meta::from([
            (META_CT_LAYOUT.to_owned(), CT_LAYOUT_VERSION.to_owned()),
            (META_CT_MODE.to_owned(), self.mode.as_str().to_owned()),
            (
                META_CT_UNPRIVILEGED.to_owned(),
                if self.unprivileged { "1" } else { "0" }.to_owned(),
            ),
        ]);
        match &self.idmap {
            Some(map) => {
                meta.insert(META_CT_IDMAP.to_owned(), map.canonical()?);
            }
            None if self.unprivileged => {
                return Err(Error::InvalidInput(
                    "an unprivileged container has an id map".into(),
                ));
            }
            None => {}
        }
        Ok(meta)
    }

    /// Fails closed on a layout this build does not read; keys it does not
    /// know are skipped.
    pub fn from_meta(meta: &Meta) -> Result<Self> {
        let get = |key: &str| meta.get(key).map(String::as_str);
        match get(META_CT_LAYOUT) {
            Some(CT_LAYOUT_VERSION) => {}
            Some(v) if matches!(decimal(v), Some(2..)) => {
                return Err(bad(format!(
                    "container layout {v} is newer than this build reads"
                )));
            }
            _ => return Err(bad("the container layout version is missing or malformed")),
        }
        let mode = get(META_CT_MODE)
            .and_then(CtMode::parse)
            .ok_or_else(|| bad("the backup mode is missing or unknown"))?;
        let unprivileged = match get(META_CT_UNPRIVILEGED) {
            Some("0") => false,
            Some("1") => true,
            _ => return Err(bad("ct_unprivileged is missing or not 0 or 1")),
        };
        let idmap = get(META_CT_IDMAP).map(IdMap::parse_canonical).transpose()?;
        if unprivileged && idmap.is_none() {
            return Err(bad("an unprivileged container snapshot has no id map"));
        }
        Ok(CtMeta {
            mode,
            unprivileged,
            idmap,
        })
    }
}

/// Checks the manifest of a container snapshot: one path, `/fs`, a
/// `proxmox-ct` source and its metadata.
pub fn check_manifest(snapshot: &pb::Snapshot) -> Result<CtMeta> {
    if snapshot.paths.len() != 1 || snapshot.paths.first().map(Vec::as_slice) != Some(FS_PATH) {
        return Err(bad("a container snapshot has the one path /fs"));
    }
    let (key, meta) = source::decode(snapshot.source.as_ref())?;
    if key.kind() != SourceType::ProxmoxCt {
        return Err(bad("the snapshot is not of a container"));
    }
    CtMeta::from_meta(&meta)
}

fn is_dir(node: &pb::Node) -> bool {
    node.r#type == pb::NodeType::Dir as i32
}

fn is_file(node: &pb::Node) -> bool {
    node.r#type == pb::NodeType::File as i32
}

/// The top of a container snapshot is the directories `config` and `fs`
/// and nothing else.
pub fn check_top(nodes: &[pb::Node]) -> Result<()> {
    let names: Vec<&[u8]> = nodes
        .iter()
        .filter(|n| is_dir(n))
        .map(|n| n.name.as_slice())
        .collect();
    if nodes.len() != 2
        || names.len() != 2
        || !names.contains(&FS_DIR)
        || !names.contains(&CONFIG_DIR)
    {
        return Err(bad("the top of a container snapshot is /config and /fs"));
    }
    Ok(())
}

/// `/config` holds the file `pct.conf` and possibly `pct.fw`, each at most
/// [`MAX_CT_CONFIG_BYTES`], and nothing else. Returns the two nodes.
pub fn check_config_dir(nodes: &[pb::Node]) -> Result<(&pb::Node, Option<&pb::Node>)> {
    let mut conf = None;
    let mut fw = None;
    for node in nodes {
        let slot = match node.name.as_slice() {
            CONFIG_FILE => &mut conf,
            FIREWALL_FILE => &mut fw,
            _ => {
                return Err(bad(
                    "/config holds something other than pct.conf and pct.fw",
                ));
            }
        };
        if !is_file(node) || slot.is_some() {
            return Err(bad("/config/pct.conf and pct.fw are single regular files"));
        }
        if node.size > MAX_CT_CONFIG_BYTES as u64 {
            return Err(bad(format!(
                "a configuration file is larger than {MAX_CT_CONFIG_BYTES} bytes"
            )));
        }
        *slot = Some(node);
    }
    let conf = conf.ok_or_else(|| bad("the snapshot has no /config/pct.conf"))?;
    Ok((conf, fw))
}

/// A writer that refuses to hold more than a configuration file can be.
struct Capped(Vec<u8>);

impl Write for Capped {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.0.len().saturating_add(buf.len()) > MAX_CT_CONFIG_BYTES {
            return Err(std::io::Error::other("configuration file is too large"));
        }
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A container snapshot, checked, with the configuration it holds: these are
/// the bytes that the server shows and the agent restores from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtSnapshot {
    pub meta: CtMeta,
    pub config: String,
    pub firewall: Option<String>,
}

impl CtSnapshot {
    pub async fn open<S: BlobStore>(reader: &mut Reader<S>, id: &BlobId) -> Result<Self> {
        let manifest = reader.snapshot(id).await?;
        let meta = check_manifest(&manifest)?;
        check_top(&reader.list_dir(id, b"/").await?)?;
        let entries = reader.list_dir(id, b"/config").await?;
        let (conf, fw) = check_config_dir(&entries)?;
        let (conf_size, fw_size) = (conf.size, fw.map(|n| n.size));
        let config = read_text(reader, id, b"/config/pct.conf", conf_size).await?;
        let firewall = match fw_size {
            Some(size) => Some(read_text(reader, id, b"/config/pct.fw", size).await?),
            None => None,
        };
        Ok(CtSnapshot {
            meta,
            config,
            firewall,
        })
    }
}

async fn read_text<S: BlobStore>(
    reader: &mut Reader<S>,
    id: &BlobId,
    path: &[u8],
    size: u64,
) -> Result<String> {
    let mut out = Capped(Vec::new());
    reader
        .dump_file(id, path, &mut out)
        .await
        .map_err(|e| match e {
            Error::Io(e) => bad(format!("{}: {e}", String::from_utf8_lossy(path))),
            e => e,
        })?;
    if out.0.len() as u64 != size {
        return Err(bad("a configuration file does not hold its recorded size"));
    }
    String::from_utf8(out.0).map_err(|_| bad("a configuration file is not UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_lines_follow_parse_pct_config() {
        let text = "# description\narch: amd64\nlxc.idmap = u 0 100000 65536\n\
                    lxc.idmap: g 0 100000 65536\nBad: x\nempty:\nnet0: name=eth0  \n\
                    unprivileged: 1\n[snap]\nhostname: other\n";
        let lines: Vec<_> = config_lines(text).collect();
        assert_eq!(
            lines,
            [
                ("arch", "amd64"),
                ("lxc.idmap", "u 0 100000 65536"),
                ("lxc.idmap", "g 0 100000 65536"),
                ("net0", "name=eth0"),
                ("unprivileged", "1"),
            ]
        );
    }

    #[test]
    fn from_config_takes_lines_then_default_then_none() {
        let lines = "lxc.idmap: u 0 100000 1000\nlxc.idmap: g 0 100000 1000\n";
        let map = IdMap::from_config(lines).unwrap().unwrap();
        assert_eq!(map.canonical().unwrap(), "u:0:100000:1000,g:0:100000:1000");
        assert_eq!(
            IdMap::from_config("unprivileged: 1\n").unwrap(),
            Some(IdMap::default_unprivileged())
        );
        assert_eq!(IdMap::from_config("arch: amd64\n").unwrap(), None);
        assert!(IdMap::from_config("lxc.idmap: u 0 x 1\n").is_err());
        assert!(
            IdMap::from_config("lxc.idmap: u 0 100000 10\nlxc.idmap: u 5 200000 10\n").is_err()
        );
        assert!(
            IdMap::from_config("lxc.idmap: u 0 100000 10\nlxc.idmap: u 10 100005 10\n").is_err()
        );
    }

    #[test]
    fn canonical_is_the_only_spelling() {
        let map = IdMap::default_unprivileged();
        let text = map.canonical().unwrap();
        assert_eq!(text, "u:0:100000:65536,g:0:100000:65536");
        assert_eq!(IdMap::parse_canonical(&text).unwrap(), map);
        for other in [
            "g:0:100000:65536,u:0:100000:65536",
            "u:00:100000:65536,g:0:100000:65536",
            "u:0:100000:65536,g:0:100000:65536,",
            "u:0:100000:0,g:0:100000:65536",
            "u:0:4294967295:2,g:0:1:1",
            "",
        ] {
            assert!(IdMap::parse_canonical(other).is_err(), "{other}");
        }
    }

    #[test]
    fn long_map_is_ct_idmap_unsupported() {
        let ranges = (0..20)
            .map(|i| IdRange {
                kind: IdKind::User,
                ct: i * 10,
                host: 100_000 + i * 10,
                count: 10,
            })
            .collect();
        let err = IdMap::new(ranges).unwrap().canonical().unwrap_err();
        assert!(err.to_string().contains(CT_IDMAP_UNSUPPORTED), "{err}");
    }
}
