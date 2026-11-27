//! The configuration of a Proxmox VE guest as `pct restore` gets it back
//! (ADR 0008). Restore runs as root@pam, and Proxmox VE then applies
//! `lxc.*`, bind and device mount points, `hookscript` and `features` from
//! the backup without any permission check. The snapshot was written by the
//! host, so this filter is the boundary: it keeps an allowlist of keys,
//! parses every property string the way `PVE::JSONSchema` does, prints what
//! it keeps anew, and reports everything it drops.
//!
//! The filter is a pure function of its input: Proxmox VE asks for the
//! configuration twice during one restore and refuses when the answers
//! differ.

use std::collections::{BTreeMap, BTreeSet};

use crate::ct::{self, MAX_CT_CONFIG_BYTES};
use crate::error::{Error, Result};

/// Reported values are clipped to this many bytes.
pub const MAX_DROPPED_VALUE: usize = 256;
/// A `rootfs` or `mpN` larger than this is dropped.
pub const MAX_CT_VOLUME_BYTES: u64 = 64 << 40;

/// Keys of the main section that are kept as they are: they configure the
/// container from the inside and are not restricted to root@pam.
pub const KEEP_KEYS: &[&str] = &[
    "arch",
    "cmode",
    "console",
    "cores",
    "cpulimit",
    "cpuunits",
    "debug",
    "description",
    "entrypoint",
    "hostname",
    "memory",
    "meta",
    "nameserver",
    "ostype",
    "protection",
    "searchdomain",
    "startup",
    "swap",
    "tags",
    "timezone",
    "tty",
];

const MAX_MOUNT_POINTS: u32 = 256;
const MAX_NETWORKS: u32 = 32;
const GIB: u64 = 1 << 30;
const MOUNT_OPTIONS: &[&str] = &[
    "discard", "lazytime", "noatime", "nodev", "noexec", "nosuid",
];
const NET_KEEP: &[&str] = &[
    "bridge", "gw", "gw6", "ip", "ip6", "mtu", "name", "rate", "tag", "trunks", "type",
];

#[derive(Debug, Clone, Copy)]
pub struct FilterInput<'a> {
    /// `pct.conf` of the snapshot.
    pub config: &'a [u8],
    /// `pct.fw` of the snapshot, if it holds one.
    pub firewall: Option<&'a [u8]>,
    /// The configuration of the container a restore replaces.
    pub target: Option<&'a [u8]>,
    /// The bridges of the target node; `None` skips the check (a preview
    /// without a node).
    pub bridges: Option<&'a BTreeSet<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filtered {
    pub text: String,
    pub dropped: Vec<Dropped>,
    pub volumes: Vec<Volume>,
    /// Key and canonical value of every `netN` kept.
    pub network: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dropped {
    pub key: String,
    pub value: String,
    pub reason: DropReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DropReason {
    Hookscript,
    Lxc,
    BindMount,
    Device,
    Privileged,
    Features,
    Args,
    Network,
    Firewall,
    Unknown,
    Section,
}

impl DropReason {
    pub fn as_str(self) -> &'static str {
        match self {
            DropReason::Hookscript => "hookscript",
            DropReason::Lxc => "lxc",
            DropReason::BindMount => "bind_mount",
            DropReason::Device => "device",
            DropReason::Privileged => "privileged",
            DropReason::Features => "features",
            DropReason::Args => "args",
            DropReason::Network => "network",
            DropReason::Firewall => "firewall",
            DropReason::Unknown => "unknown",
            DropReason::Section => "section",
        }
    }
}

/// A volume that `pct restore` allocates on `--storage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    pub key: String,
    pub mp: String,
    pub size_bytes: u64,
    pub storage: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountKind {
    Volume,
    Bind,
    Device,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mountpoint {
    pub kind: MountKind,
    pub volume: String,
    /// `/` for `rootfs`.
    pub mp: Option<String>,
    pub size_bytes: Option<u64>,
    pub backup: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestIdentity {
    pub uid: String,
    pub rtime: Option<u64>,
}

impl Dropped {
    fn new(key: &str, value: &str, reason: DropReason) -> Self {
        Dropped {
            key: clip(key),
            value: clip(value),
            reason,
        }
    }
}

fn clip(text: &str) -> String {
    text[..text.floor_char_boundary(MAX_DROPPED_VALUE)].to_owned()
}

/// Perl's `\s` on a byte string.
fn perl_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

/// Whether Proxmox VE reads `value` as true or false (`parse_boolean`).
pub fn parse_boolean(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "on" | "yes" | "true" => Some(true),
        "0" | "off" | "no" | "false" => Some(false),
        _ => None,
    }
}

/// A property string split by the rules of
/// `PVE::JSONSchema::parse_property_string`, without a schema: parts are
/// split at `,`, blank parts skipped, `key=value` taken byte for byte (no
/// trim, no case folding), a part without `=` is the value of `default_key`.
/// `None` where Proxmox VE dies: a newline, a part with `=` and an empty
/// key or value, a repeated key, a second default part or a default part
/// without a default key.
pub fn parse_property(value: &str, default_key: Option<&str>) -> Option<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    let mut default_seen = false;
    for part in value.split(',') {
        if part.chars().all(perl_space) {
            continue;
        }
        if part.contains('\n') {
            return None;
        }
        if let Some((key, val)) = part.split_once('=') {
            if key.is_empty() || val.is_empty() || out.contains_key(key) {
                return None;
            }
            out.insert(key.to_owned(), val.to_owned());
        } else {
            let key = default_key?;
            if default_seen {
                return None;
            }
            default_seen = true;
            // Perl tests `!$res->{$default_key}`: an explicit "0" is false
            // and gets replaced.
            if out.get(key).is_some_and(|v| v != "0") {
                return None;
            }
            out.insert(key.to_owned(), part.to_owned());
        }
    }
    Some(out)
}

/// `print_property_string`: the default value first, then `required` in
/// that order, then the rest sorted.
fn print_property(
    default: Option<&str>,
    required: &[&str],
    props: &BTreeMap<String, String>,
) -> String {
    let mut parts: Vec<String> = default.map(str::to_owned).into_iter().collect();
    for key in required {
        if let Some(v) = props.get(*key) {
            parts.push(format!("{key}={v}"));
        }
    }
    for (k, v) in props {
        if !required.contains(&k.as_str()) {
            parts.push(format!("{k}={v}"));
        }
    }
    parts.join(",")
}

/// `PVE::LXC::Config::classify_mountpoint`.
pub fn classify_mountpoint(volume: &str) -> MountKind {
    if volume.starts_with("/dev/") {
        MountKind::Device
    } else if volume.starts_with('/') {
        MountKind::Bind
    } else {
        MountKind::Volume
    }
}

/// A `rootfs`, `mpN` or `unusedN` value as Proxmox VE reads it, without the
/// checks of the filter.
pub fn parse_mountpoint(key: &str, value: &str) -> Option<Mountpoint> {
    let mut props = parse_property(value, Some("volume"))?;
    let volume = props.remove("volume")?;
    let mp = if key == "rootfs" {
        Some("/".to_owned())
    } else {
        props.remove("mp")
    };
    Some(Mountpoint {
        kind: classify_mountpoint(&volume),
        volume,
        mp,
        size_bytes: props.get("size").and_then(|s| parse_size(s)),
        backup: props.get("backup").and_then(|b| parse_boolean(b)),
    })
}

/// The storage of a volume id, `^[a-z][a-z0-9._-]*:`.
fn storage_of(volume: &str) -> Option<&str> {
    let (storage, _) = volume.split_once(':')?;
    let mut bytes = storage.bytes();
    let first = bytes.next()?;
    (first.is_ascii_lowercase()
        && bytes.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-')))
    .then_some(storage)
}

/// `PVE::JSONSchema::parse_size` in exact integer arithmetic.
fn parse_size(text: &str) -> Option<u64> {
    let (number, shift) = [
        ("KiB", 10),
        ("MiB", 20),
        ("GiB", 30),
        ("TiB", 40),
        ("K", 10),
        ("M", 20),
        ("G", 30),
        ("T", 40),
    ]
    .iter()
    .find_map(|(unit, shift)| text.strip_suffix(unit).map(|n| (n, *shift)))
    .unwrap_or((text, 0));
    let (int, frac) = match number.split_once('.') {
        Some((int, frac)) => (int, Some(frac)),
        None => (number, None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(int) || frac.is_some_and(|f| !digits(f) || f.len() > 18) {
        return None;
    }
    let mut bytes = int.parse::<u128>().ok()?.checked_mul(1 << shift)?;
    if let Some(frac) = frac {
        let scale = 10u128.pow(u32::try_from(frac.len()).ok()?);
        bytes = bytes.checked_add((frac.parse::<u128>().ok()? << shift) / scale)?;
    }
    u64::try_from(bytes).ok()
}

/// `PVE::JSONSchema::format_size`: the largest unit that divides evenly.
fn format_size(bytes: u64) -> String {
    for (shift, unit) in [(40, "T"), (30, "G"), (20, "M"), (10, "K")] {
        if bytes.trailing_zeros() >= shift {
            return format!("{}{unit}", bytes >> shift);
        }
    }
    bytes.to_string()
}

/// `PVE::LXC::Config::verify_lxc_mp_string`, plus an absolute path.
fn valid_mp(mp: &str) -> bool {
    mp.starts_with('/')
        && !mp.contains("/./")
        && !mp.contains("/../")
        && !mp.ends_with("/.")
        && !mp.ends_with("/..")
}

/// `[-_.\w\d]+` on a byte string.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// `prefix` followed by a number below `max` written without leading zeros.
fn indexed(key: &str, prefix: &str, max: u32) -> bool {
    key.strip_prefix(prefix).is_some_and(|n| {
        (n == "0" || !n.starts_with('0'))
            && !n.is_empty()
            && n.bytes().all(|b| b.is_ascii_digit())
            && n.parse::<u32>().is_ok_and(|n| n < max)
    })
}

/// `prefix` followed by any digits.
fn numbered(key: &str, prefix: &str) -> bool {
    key.strip_prefix(prefix)
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Keep,
    Meta,
    Description,
    Mountpoint,
    Net,
    Features,
    Unprivileged,
    Onboot,
    Silent,
    Drop(DropReason),
}

fn classify(key: &str) -> Class {
    match key {
        "meta" => Class::Meta,
        "description" => Class::Description,
        "rootfs" => Class::Mountpoint,
        "features" => Class::Features,
        "unprivileged" => Class::Unprivileged,
        "onboot" => Class::Onboot,
        "hookscript" => Class::Drop(DropReason::Hookscript),
        "args" => Class::Drop(DropReason::Args),
        "env" => Class::Drop(DropReason::Lxc),
        "lock" | "template" | "parent" | "snaptime" | "digest" | "pending" => Class::Silent,
        _ if key.starts_with("lxc.") => Class::Drop(DropReason::Lxc),
        _ if ["dev", "hostpci", "usb"].iter().any(|p| numbered(key, p)) => {
            Class::Drop(DropReason::Device)
        }
        _ if numbered(key, "unused") => Class::Silent,
        _ if indexed(key, "mp", MAX_MOUNT_POINTS) => Class::Mountpoint,
        _ if indexed(key, "net", MAX_NETWORKS) => Class::Net,
        _ if KEEP_KEYS.contains(&key) => Class::Keep,
        _ => Class::Drop(DropReason::Unknown),
    }
}

/// One line of the main section.
enum Line<'a> {
    Comment(&'a str),
    Pair {
        line: &'a str,
        key: &'a str,
        value: &'a str,
    },
}

/// Filters the configuration of a container snapshot for `pct restore`.
/// `guest_identity` of the result is the identity Proxmox VE reads during
/// the restore.
pub fn filter_ct(input: &FilterInput) -> Result<Filtered> {
    for (name, bytes) in [
        ("pct.conf", Some(input.config)),
        ("pct.fw", input.firewall),
        ("the target configuration", input.target),
    ] {
        if bytes.is_some_and(|b| b.len() > MAX_CT_CONFIG_BYTES) {
            return Err(Error::InvalidInput(format!(
                "{name} is larger than {MAX_CT_CONFIG_BYTES} bytes"
            )));
        }
    }
    let target = input.target.map(target_networks).unwrap_or_default();
    let mut dropped = Vec::new();
    let mut section = None;
    let mut lines = Vec::new();
    // Proxmox VE keeps the last of a repeated key.
    let mut last = BTreeMap::new();
    let mut raw = input.config.split(|b| *b == b'\n');
    for bytes in raw.by_ref() {
        if bytes.first() == Some(&b'[') {
            section = Some(bytes);
            break;
        }
        // The identity is the last `meta:` line, even one the filter cannot
        // keep (see `guest_identity`): then no `meta` line is kept.
        if bytes.starts_with(b"meta:") && bytes.len() > 5 {
            last.remove("meta");
        }
        let Ok(line) = std::str::from_utf8(bytes) else {
            dropped.push(Dropped::new(
                "",
                &String::from_utf8_lossy(bytes),
                DropReason::Unknown,
            ));
            continue;
        };
        if line.chars().all(perl_space) {
            continue;
        }
        if line.starts_with('#') {
            lines.push(Line::Comment(line));
        } else if let Some((key, value)) = ct::config_lines(line).next() {
            last.insert(key, lines.len());
            lines.push(Line::Pair { line, key, value });
        } else {
            dropped.push(Dropped::new("", line, DropReason::Unknown));
        }
    }

    let mut out = Vec::new();
    let mut volumes = Vec::new();
    let mut network = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let (line, key, value) = match line {
            Line::Comment(text) => {
                out.push((*text).to_owned());
                continue;
            }
            Line::Pair { line, key, value } => (*line, *key, *value),
        };
        let class = classify(key);
        let unique = matches!(
            class,
            Class::Keep | Class::Meta | Class::Mountpoint | Class::Net | Class::Features
        );
        if unique && last.get(key) != Some(&i) {
            dropped.push(Dropped::new(key, value, DropReason::Unknown));
            continue;
        }
        match class {
            Class::Keep | Class::Description => out.push(format!("{key}: {value}")),
            Class::Meta => out.push(line.to_owned()),
            Class::Mountpoint => {
                if let Some((text, volume)) = filter_mountpoint(key, value, &mut dropped) {
                    out.push(format!("{key}: {text}"));
                    volumes.push(volume);
                }
            }
            Class::Net => {
                if let Some(text) =
                    filter_net(key, value, target.get(key), input.bridges, &mut dropped)
                {
                    out.push(format!("{key}: {text}"));
                    network.push((key.to_owned(), text));
                }
            }
            Class::Features => {
                if let Some(text) = filter_features(value, &mut dropped) {
                    out.push(format!("{key}: {text}"));
                }
            }
            Class::Unprivileged => {
                if parse_boolean(value) != Some(true) {
                    dropped.push(Dropped::new(key, value, DropReason::Privileged));
                }
            }
            Class::Onboot | Class::Silent => {}
            Class::Drop(reason) => dropped.push(Dropped::new(key, value, reason)),
        }
    }
    out.push("unprivileged: 1".to_owned());
    out.push("onboot: 0".to_owned());

    if let Some(header) = section {
        let rest = raw.filter(|l| !l.is_empty()).count();
        dropped.push(Dropped::new(
            String::from_utf8_lossy(header).trim_matches(perl_space),
            &format!("{} lines", rest + 1),
            DropReason::Section,
        ));
    }
    if let Some(fw) = input.firewall {
        let n = fw.split(|b| *b == b'\n').filter(|l| !l.is_empty()).count();
        dropped.push(Dropped::new(
            "pct.fw",
            &format!("{n} lines"),
            DropReason::Firewall,
        ));
    }

    let mut text = out.join("\n");
    text.push('\n');
    Ok(Filtered {
        text,
        dropped,
        volumes,
        network,
    })
}

/// `netN` of the target, parsed; the last of a repeated key wins.
fn target_networks(config: &[u8]) -> BTreeMap<String, BTreeMap<String, String>> {
    let text = String::from_utf8_lossy(config);
    ct::config_lines(&text)
        .filter(|(key, _)| indexed(key, "net", MAX_NETWORKS))
        .filter_map(|(key, value)| Some((key.to_owned(), parse_property(value, None)?)))
        .collect()
}

fn filter_mountpoint(
    key: &str,
    value: &str,
    dropped: &mut Vec<Dropped>,
) -> Option<(String, Volume)> {
    let whole = |dropped: &mut Vec<Dropped>, reason| {
        dropped.push(Dropped::new(key, value, reason));
        None
    };
    let Some(mut props) = parse_property(value, Some("volume")) else {
        return whole(dropped, DropReason::Unknown);
    };
    let Some(volume) = props.remove("volume") else {
        return whole(dropped, DropReason::Unknown);
    };
    match classify_mountpoint(&volume) {
        MountKind::Device => return whole(dropped, DropReason::Device),
        MountKind::Bind => return whole(dropped, DropReason::BindMount),
        MountKind::Volume => {}
    }
    let Some(storage) = storage_of(&volume) else {
        return whole(dropped, DropReason::Unknown);
    };
    let rootfs = key == "rootfs";
    let Some(size) = props
        .remove("size")
        .and_then(|s| parse_size(&s))
        .filter(|s| (1..=MAX_CT_VOLUME_BYTES).contains(s))
    else {
        return whole(dropped, DropReason::Unknown);
    };
    let mp = if rootfs {
        "/".to_owned()
    } else {
        match props.remove("mp").filter(|mp| valid_mp(mp)) {
            Some(mp) => mp,
            None => return whole(dropped, DropReason::Unknown),
        }
    };

    let mut keep = BTreeMap::new();
    let mut extra = Vec::new();
    for (k, v) in props {
        let kept = match k.as_str() {
            "acl" | "quota" | "ro" | "replicate" => {
                parse_boolean(&v).map(|b| u8::from(b).to_string())
            }
            "backup" if !rootfs => parse_boolean(&v).map(|b| u8::from(b).to_string()),
            "mountoptions" => v
                .split(';')
                .all(|o| MOUNT_OPTIONS.contains(&o))
                .then(|| v.clone()),
            _ => None,
        };
        match kept {
            Some(v) => {
                keep.insert(k, v);
            }
            None => extra.push(Dropped::new(key, &format!("{k}={v}"), DropReason::Unknown)),
        }
    }
    keep.insert("size".to_owned(), format_size(size));
    if !rootfs {
        keep.insert("mp".to_owned(), mp.clone());
    }
    dropped.extend(extra);
    // An allocation, never a reference to an existing volume.
    let alloc = format!("{storage}:{}", size.div_ceil(GIB).max(1));
    let text = print_property(Some(&alloc), &["mp"], &keep);
    let volume = Volume {
        key: key.to_owned(),
        mp,
        size_bytes: size,
        storage: storage.to_owned(),
    };
    Some((text, volume))
}

fn filter_net(
    key: &str,
    value: &str,
    target: Option<&BTreeMap<String, String>>,
    bridges: Option<&BTreeSet<String>>,
    dropped: &mut Vec<Dropped>,
) -> Option<String> {
    let whole = |dropped: &mut Vec<Dropped>, why: String| {
        dropped.push(Dropped::new(key, &why, DropReason::Network));
        None
    };
    let Some(props) = parse_property(value, None) else {
        return whole(dropped, value.to_owned());
    };
    if !props.get("name").is_some_and(|n| valid_name(n))
        || props.get("type").is_some_and(|t| t != "veth")
        || props.get("bridge").is_some_and(|b| !valid_name(b))
    {
        return whole(dropped, value.to_owned());
    }
    if let (Some(bridge), Some(set)) = (props.get("bridge"), bridges)
        && !set.contains(bridge)
    {
        return whole(
            dropped,
            format!("bridge {bridge} is not on the node: {value}"),
        );
    }
    let mut keep = BTreeMap::new();
    let mut extra = Vec::new();
    let mut firewall = false;
    for (k, v) in props {
        match k.as_str() {
            "hwaddr" => {}
            "firewall" => firewall = parse_boolean(&v) == Some(true),
            _ if NET_KEEP.contains(&k.as_str()) => {
                keep.insert(k, v);
            }
            _ => extra.push(Dropped::new(key, &format!("{k}={v}"), DropReason::Network)),
        }
    }
    // The network identity of the replaced container stays; a new one gets
    // a MAC from Proxmox VE and no firewall.
    if let Some(target) = target {
        for k in ["hwaddr", "firewall"] {
            if let Some(v) = target.get(k) {
                keep.insert(k.to_owned(), v.clone());
            }
        }
    }
    if firewall && keep.get("firewall").and_then(|f| parse_boolean(f)) != Some(true) {
        extra.push(Dropped::new(
            key,
            "firewall=1 (the firewall flag is not restored)",
            DropReason::Network,
        ));
    }
    dropped.extend(extra);
    Some(print_property(None, &["name"], &keep))
}

fn filter_features(value: &str, dropped: &mut Vec<Dropped>) -> Option<String> {
    let Some(mut props) = parse_property(value, None) else {
        dropped.push(Dropped::new("features", value, DropReason::Features));
        return None;
    };
    let nesting = match props.remove("nesting") {
        Some(v) => match parse_boolean(&v) {
            Some(b) => b,
            None => {
                props.insert("nesting".to_owned(), v);
                false
            }
        },
        None => false,
    };
    if !props.is_empty() {
        let rest = print_property(None, &[], &props);
        dropped.push(Dropped::new("features", &rest, DropReason::Features));
    }
    nesting.then(|| "nesting=1".to_owned())
}

/// The identity of a guest, as `PVE::GuestIdentity::parse_identity_from_raw_config`
/// reads it: the last `meta:` line of the main section, its members `uid`
/// and `rtime`; a repeated member is invalid, and so is the identity without
/// a valid uid.
pub fn guest_identity(raw_config: &[u8]) -> Option<GuestIdentity> {
    let text = String::from_utf8_lossy(raw_config);
    let mut meta = None;
    for line in text.split('\n') {
        if line.starts_with('[') {
            break;
        }
        if let Some(rest) = line.strip_prefix("meta:")
            && !rest.is_empty()
        {
            meta = Some(rest.trim_matches(perl_space));
        }
    }
    let mut uid: Option<Option<&str>> = None;
    let mut rtime: Option<Option<&str>> = None;
    for part in meta?.split(',') {
        let Some((key, value)) = part.split_once('=').filter(|(k, _)| !k.is_empty()) else {
            continue;
        };
        let slot = match key {
            "uid" => &mut uid,
            "rtime" => &mut rtime,
            _ => continue,
        };
        *slot = Some(if slot.is_some() { None } else { Some(value) });
    }
    let uid = uid
        .flatten()
        .filter(|u| u.len() == 20 && u.bytes().all(|b| matches!(b, b'a'..=b'z' | b'2'..=b'7')))?;
    let rtime = rtime
        .flatten()
        .filter(|r| (1..=15).contains(&r.len()) && r.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|r| r.parse().ok());
    Some(GuestIdentity {
        uid: uid.to_owned(),
        rtime,
    })
}

/// Two guests are the same only when both have a uid and it is the same.
pub fn same_guest(a: Option<&str>, b: Option<&str>) -> bool {
    matches!((a, b), (Some(a), Some(b)) if a == b)
}

/// Whether `pct restore` needs `--allow-identity-mismatch`: both uids are
/// known and differ.
pub fn needs_identity_mismatch_flag(snapshot: Option<&str>, target: Option<&str>) -> bool {
    matches!((snapshot, target), (Some(s), Some(t)) if s != t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse_and_print_like_pve() {
        assert_eq!(parse_size("8G"), Some(8 << 30));
        assert_eq!(parse_size("8GiB"), Some(8 << 30));
        assert_eq!(parse_size("1.5K"), Some(1536));
        assert_eq!(parse_size("0.1K"), Some(102));
        assert_eq!(parse_size("1000"), Some(1000));
        for bad in [
            "",
            "G",
            "1.G",
            ".5G",
            "1g",
            "1 G",
            "-1",
            "1E",
            "99999999999999999999T",
        ] {
            assert_eq!(parse_size(bad), None, "{bad}");
        }
        assert_eq!(format_size(8 << 30), "8G");
        assert_eq!(format_size(1536), "1536");
        assert_eq!(format_size(3 << 10), "3K");
        assert_eq!(format_size(1 << 40), "1T");
        assert_eq!(format_size(0), "0T");
    }
}
