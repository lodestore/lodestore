//! Canonical keys of backup source instances.
//!
//! `files` is the empty key; every other key is `<type>/<segment>[/<segment>]`
//! where `<type>` is [`SourceType::as_str`]. A segment is percent-encoded
//! UTF-8: the bytes `A-Z a-z 0-9 - . _ ~` are written as they are, every other
//! byte as `%XX` in upper case. A decoded segment is non-empty, at most
//! [`MAX_SEGMENT_LEN`] bytes and valid UTF-8; the whole key is at most
//! [`MAX_KEY_LEN`] bytes. Only the canonical spelling parses: the one that
//! equals the re-encoding of its own decoding.
//!
//! ```text
//! proxmox-vm/<cluster>/<vm uuid>
//! proxmox-ct/<cluster>/<container uuid>
//! postgres/<system_identifier>/<database oid>
//! postgres-wal/<system_identifier>
//! mysql/<server uuid>/<schema>
//! ```
//!
//! A uuid is the lower-case `8-4-4-4-12` form. `system_identifier` is a
//! decimal `u64` and the oid a decimal `u32`, neither zero nor with a leading
//! zero. A guest is told apart by its uuid and never by a VMID alone, so a
//! reused VMID is another instance. The cluster segment is the corosync
//! cluster name, or the node name on a node outside a cluster; a node that
//! joins a cluster later gives its guests new keys.
//!
//! A VM uses the uuid of its `smbios1` line. A container has no uuid of its
//! own: the agent of the node generates one and keeps it in its state
//! (`/var/lib/lodestore/source-ids.json`), writes nothing into the Proxmox
//! configs, and reports a new uuid only once it is saved. A reinstalled node,
//! a migrated container, a switch between container and VM under one VMID and
//! a VMID missing from at least one successful discovery run all give a new
//! uuid and so a new instance, as a migration does. A container destroyed and
//! created again under the same VMID within one discovery interval keeps the
//! old uuid. A VM whose SMBIOS uuid is nil or shared with another VM of the
//! same node is not an instance and is not reported.
//!
//! The key groups retention, `latest` and the dead-man check, so two
//! different identities must never share a key: the encoding is injective.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};

use crate::error::{Error, Result};
use crate::pb;

pub const MAX_KEY_LEN: usize = 512;
pub const MAX_SEGMENT_LEN: usize = 255;
pub const MAX_META_ENTRIES: usize = 32;
pub const MAX_META_KEY_LEN: usize = 64;
pub const MAX_META_VALUE_LEN: usize = 256;

pub type Meta = BTreeMap<String, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SourceType {
    Files,
    ProxmoxVm,
    ProxmoxCt,
    Postgres,
    PostgresWal,
    Mysql,
}

impl SourceType {
    pub const ALL: [SourceType; 6] = [
        SourceType::Files,
        SourceType::ProxmoxVm,
        SourceType::ProxmoxCt,
        SourceType::Postgres,
        SourceType::PostgresWal,
        SourceType::Mysql,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            SourceType::Files => "files",
            SourceType::ProxmoxVm => "proxmox-vm",
            SourceType::ProxmoxCt => "proxmox-ct",
            SourceType::Postgres => "postgres",
            SourceType::PostgresWal => "postgres-wal",
            SourceType::Mysql => "mysql",
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        SourceType::ALL
            .into_iter()
            .find(|t| t.as_str() == text)
            .ok_or_else(|| bad("unknown source type"))
    }

    /// A WAL batch is not a point-in-time image and never competes for `latest`.
    pub fn is_wal_batch(self) -> bool {
        self == SourceType::PostgresWal
    }
}

/// The canonical key of one source instance.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceKey {
    key: String,
    kind: SourceType,
}

impl SourceKey {
    pub fn files() -> Self {
        SourceKey {
            key: String::new(),
            kind: SourceType::Files,
        }
    }

    pub fn proxmox_vm(cluster: &str, vm_uuid: &str) -> Result<Self> {
        Self::build(SourceType::ProxmoxVm, &[cluster, vm_uuid])
    }

    pub fn proxmox_ct(cluster: &str, ct_uuid: &str) -> Result<Self> {
        Self::build(SourceType::ProxmoxCt, &[cluster, ct_uuid])
    }

    pub fn postgres(system_identifier: u64, database_oid: u32) -> Result<Self> {
        Self::build(
            SourceType::Postgres,
            &[&system_identifier.to_string(), &database_oid.to_string()],
        )
    }

    pub fn postgres_wal(system_identifier: u64) -> Result<Self> {
        Self::build(SourceType::PostgresWal, &[&system_identifier.to_string()])
    }

    pub fn mysql(server_uuid: &str, schema: &str) -> Result<Self> {
        Self::build(SourceType::Mysql, &[server_uuid, schema])
    }

    fn build(kind: SourceType, segments: &[&str]) -> Result<Self> {
        let mut key = kind.as_str().to_owned();
        for segment in segments {
            key.push('/');
            encode_segment(segment, &mut key);
        }
        Self::parse(&key)
    }

    /// Accepts the canonical spelling of a key and nothing else.
    pub fn parse(text: &str) -> Result<Self> {
        if text.is_empty() {
            return Ok(Self::files());
        }
        if text.len() > MAX_KEY_LEN {
            return Err(bad("source key is too long"));
        }
        let mut parts = text.split('/');
        let kind = SourceType::parse(parts.next().unwrap_or_default())?;
        let segments = parts.map(decode_segment).collect::<Result<Vec<_>>>()?;
        match (kind, segments.as_slice()) {
            (SourceType::ProxmoxVm | SourceType::ProxmoxCt, [_, guest]) if is_uuid(guest) => {}
            (SourceType::ProxmoxVm | SourceType::ProxmoxCt, [_, _]) => {
                return Err(bad("a guest is identified by its lower-case uuid"));
            }
            (SourceType::Postgres, [id, oid]) => {
                number::<u64>(id)?;
                number::<u32>(oid)?;
            }
            (SourceType::PostgresWal, [id]) => {
                number::<u64>(id)?;
            }
            (SourceType::Mysql, [server, _]) if is_uuid(server) => {}
            (SourceType::Mysql, [_, _]) => return Err(bad("a server is identified by its uuid")),
            (SourceType::Files, _) => return Err(bad("the files key is empty")),
            _ => return Err(bad("wrong number of segments for the source type")),
        }
        Ok(SourceKey {
            key: text.to_owned(),
            kind,
        })
    }

    pub fn kind(&self) -> SourceType {
        self.kind
    }

    pub fn as_str(&self) -> &str {
        &self.key
    }

    pub fn is_files(&self) -> bool {
        self.kind == SourceType::Files
    }

    fn segment(&self, n: usize) -> Option<String> {
        decode_segment(self.key.split('/').nth(n)?).ok()
    }
}

impl fmt::Display for SourceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.is_files() { "files" } else { &self.key })
    }
}

fn bad(message: &str) -> Error {
    Error::InvalidInput(format!("source: {message}"))
}

fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

fn encode_segment(raw: &str, out: &mut String) {
    for b in raw.bytes() {
        if is_unreserved(b) {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
}

fn decode_segment(segment: &str) -> Result<String> {
    let mut bytes = Vec::with_capacity(segment.len());
    let mut it = segment.bytes();
    while let Some(b) = it.next() {
        if b != b'%' {
            bytes.push(b);
            continue;
        }
        let mut hex = || it.next().and_then(|c| char::from(c).to_digit(16));
        match (hex(), hex()) {
            (Some(hi), Some(lo)) => bytes.push(u8::try_from(hi * 16 + lo).unwrap_or_default()),
            _ => return Err(bad("truncated percent escape")),
        }
    }
    if bytes.is_empty() || bytes.len() > MAX_SEGMENT_LEN {
        return Err(bad("segment must be 1 to 255 bytes"));
    }
    let decoded = String::from_utf8(bytes).map_err(|_| bad("segment is not UTF-8"))?;
    let mut again = String::with_capacity(segment.len());
    encode_segment(&decoded, &mut again);
    if again != segment {
        return Err(bad("segment is not in canonical percent-encoding"));
    }
    Ok(decoded)
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => matches!(b, b'0'..=b'9' | b'a'..=b'f'),
        })
}

fn number<T: std::str::FromStr>(s: &str) -> Result<T> {
    if s.starts_with('0') || s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad(
            "number must be decimal, non-zero, without leading zeros",
        ));
    }
    s.parse::<T>().map_err(|_| bad("number is out of range"))
}

fn check_meta<'a>(entries: impl ExactSizeIterator<Item = (&'a str, &'a str)>) -> Result<()> {
    if entries.len() > MAX_META_ENTRIES {
        return Err(bad("too many meta entries"));
    }
    let mut previous: Option<&str> = None;
    for (key, value) in entries {
        let key_ok = (1..=MAX_META_KEY_LEN).contains(&key.len())
            && key
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if !key_ok {
            return Err(bad("meta key must be [a-z0-9_]{1,64}"));
        }
        if value.len() > MAX_META_VALUE_LEN {
            return Err(bad("meta value is too long"));
        }
        if previous.is_some_and(|p| p >= key) {
            return Err(bad("meta keys must be strictly ascending"));
        }
        previous = Some(key);
    }
    Ok(())
}

/// The source a manifest or a provenance record claims; absent is `files`.
pub fn decode(source: Option<&pb::Source>) -> Result<(SourceKey, Meta)> {
    let Some(source) = source else {
        return Ok((SourceKey::files(), Meta::new()));
    };
    let key = SourceKey::parse(&source.key)?;
    if key.is_files() {
        return Ok((key, Meta::new()));
    }
    check_meta(
        source
            .meta
            .iter()
            .map(|m| (m.key.as_str(), m.value.as_str())),
    )?;
    let meta = source
        .meta
        .iter()
        .map(|m| (m.key.clone(), m.value.clone()))
        .collect();
    Ok((key, meta))
}

/// `files` is written as no source at all.
pub fn encode(key: &SourceKey, meta: &Meta) -> Result<Option<pb::Source>> {
    if key.is_files() {
        return Ok(None);
    }
    check_meta(meta.iter().map(|(k, v)| (k.as_str(), v.as_str())))?;
    Ok(Some(pb::Source {
        key: key.as_str().to_owned(),
        meta: meta
            .iter()
            .map(|(k, v)| pb::SourceMeta {
                key: k.clone(),
                value: v.clone(),
            })
            .collect(),
    }))
}

/// What a person calls the instance, from the meta the agent claims.
pub fn label(key: &SourceKey, meta: &Meta) -> Option<String> {
    let get = |k: &str| meta.get(k).map(String::as_str).filter(|v| !v.is_empty());
    let major = || {
        get("server_version")
            .and_then(|v| v.split('.').next())
            .filter(|v| !v.is_empty())
    };
    let guest = |prefix: &str| {
        let vmid = get("vmid")?;
        Some(match get("name") {
            Some(name) => format!("{prefix} {vmid} · {name}"),
            None => format!("{prefix} {vmid}"),
        })
    };
    let database = |name: String, product: &str| match major() {
        Some(v) => format!("{name} · {product} {v}"),
        None => name,
    };
    match key.kind() {
        SourceType::ProxmoxVm => guest("VM"),
        SourceType::ProxmoxCt => guest("CT"),
        SourceType::Postgres => Some(database(get("database")?.to_owned(), "PostgreSQL")),
        SourceType::Mysql => Some(database(key.segment(2)?, "MySQL")),
        SourceType::Files | SourceType::PostgresWal => None,
    }
}
