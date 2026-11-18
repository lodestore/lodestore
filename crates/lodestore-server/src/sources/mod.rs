//! The sources an agent reports for its host.
//!
//! The list is untrusted: the server bounds it, builds every canonical key
//! itself and drops what does not fit. It is stored as facts about the host and
//! grants nothing.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Context as _;
use lodestore_format::source::SourceKey;
use lodestore_proto::agent::v1::{GuestKind, SourceFacts};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::state::AppState;

/// Guests kept per host.
pub const MAX_GUESTS: usize = 1000;
/// Tags kept per guest.
pub const MAX_GUEST_TAGS: usize = 20;
/// Database servers kept per host.
pub const MAX_SERVERS: usize = 16;
/// Databases kept per host over all servers.
pub const MAX_DATABASES: usize = 1000;
/// Longest guest name, pool, tag and PostgreSQL database name.
pub const MAX_NAME_BYTES: usize = 64;
/// Longest server version string.
pub const MAX_VERSION_BYTES: usize = 32;
/// How long a collection counts as current.
pub const FRESH_FOR: Duration = Duration::from_secs(15 * 60);

const MIN_VMID: u32 = 100;
const MAX_VMID: u32 = 999_999_999;
const NIL_UUID: &str = "00000000-0000-0000-0000-000000000000";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    Cluster,
    Node,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestType {
    Vm,
    Ct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DbType {
    Postgres,
    Mysql,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Guest {
    pub kind: GuestType,
    pub vmid: u32,
    pub name: Option<String>,
    pub pool: Option<String>,
    pub tags: Vec<String>,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProxmoxNode {
    pub scope: ScopeKind,
    pub name: String,
    pub guests: Vec<Guest>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Db {
    pub name: String,
    pub oid: Option<u32>,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DbServer {
    pub kind: DbType,
    pub port: u16,
    pub version: Option<String>,
    pub unreachable: bool,
    /// `None`: the databases are unknown, not absent.
    pub databases: Option<Vec<Db>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Discovery {
    pub truncated: bool,
    pub proxmox: Option<ProxmoxNode>,
    pub servers: Vec<DbServer>,
}

pub struct Stored {
    pub discovery: Discovery,
    pub collected_at: Option<i64>,
}

pub fn is_fresh(collected_at: Option<i64>, now_ms: i64) -> bool {
    let window = i64::try_from(FRESH_FOR.as_millis()).unwrap_or(i64::MAX);
    collected_at.is_some_and(|at| now_ms.saturating_sub(at) < window)
}

fn clip(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

fn clipped(text: &str, max: usize) -> Option<String> {
    (!text.is_empty()).then(|| clip(text, max))
}

impl Discovery {
    pub fn from_proto(facts: &SourceFacts) -> Discovery {
        let mut truncated = facts.truncated;
        let mut seen = HashSet::new();
        let proxmox = facts
            .proxmox
            .as_ref()
            .and_then(|p| proxmox_node(p, &mut seen, &mut truncated));
        Discovery {
            truncated,
            proxmox,
            servers: Vec::new(),
        }
    }

    pub fn has(&self, source_type: &str) -> bool {
        let guest = |kind| {
            self.proxmox
                .as_ref()
                .is_some_and(|p| p.guests.iter().any(|g| g.kind == kind))
        };
        let server = |kind| self.servers.iter().any(|s| s.kind == kind);
        match source_type {
            "proxmox-vm" => guest(GuestType::Vm),
            "proxmox-ct" => guest(GuestType::Ct),
            "postgres" => server(DbType::Postgres),
            "mysql" => server(DbType::Mysql),
            _ => false,
        }
    }

    pub fn types(&self) -> Vec<&'static str> {
        ["proxmox-vm", "proxmox-ct", "postgres", "mysql"]
            .into_iter()
            .filter(|t| self.has(t))
            .collect()
    }

    pub fn keys(&self) -> Vec<String> {
        let guests = self
            .proxmox
            .iter()
            .flat_map(|p| p.guests.iter().map(|g| g.key.clone()));
        let databases = self
            .servers
            .iter()
            .flat_map(|s| s.databases.iter().flatten().map(|d| d.key.clone()));
        guests.chain(databases).collect()
    }
}

fn proxmox_node(
    facts: &lodestore_proto::agent::v1::ProxmoxFacts,
    seen: &mut HashSet<String>,
    truncated: &mut bool,
) -> Option<ProxmoxNode> {
    let (scope, name) = if facts.cluster.is_empty() {
        (ScopeKind::Node, facts.node.as_str())
    } else {
        (ScopeKind::Cluster, facts.cluster.as_str())
    };
    if name.is_empty() {
        *truncated = true;
        return None;
    }
    let mut vmids = HashSet::new();
    let mut guests = Vec::new();
    for guest in &facts.guests {
        if guests.len() >= MAX_GUESTS {
            *truncated = true;
            break;
        }
        match proxmox_guest(guest, name, &mut vmids, seen) {
            Some(guest) => guests.push(guest),
            None => *truncated = true,
        }
    }
    Some(ProxmoxNode {
        scope,
        name: name.to_string(),
        guests,
    })
}

fn proxmox_guest(
    guest: &lodestore_proto::agent::v1::ProxmoxGuest,
    scope_name: &str,
    vmids: &mut HashSet<u32>,
    seen: &mut HashSet<String>,
) -> Option<Guest> {
    let kind = match GuestKind::try_from(guest.kind).ok()? {
        GuestKind::Vm => GuestType::Vm,
        GuestKind::Ct => GuestType::Ct,
        GuestKind::Unspecified => return None,
    };
    if !(MIN_VMID..=MAX_VMID).contains(&guest.vmid) || guest.uuid == NIL_UUID {
        return None;
    }
    let key = match kind {
        GuestType::Vm => SourceKey::proxmox_vm(scope_name, &guest.uuid),
        GuestType::Ct => SourceKey::proxmox_ct(scope_name, &guest.uuid),
    }
    .ok()?;
    if vmids.contains(&guest.vmid) || seen.contains(key.as_str()) {
        return None;
    }
    vmids.insert(guest.vmid);
    seen.insert(key.as_str().to_string());
    let mut tags: Vec<String> = guest
        .tags
        .iter()
        .filter_map(|t| clipped(t, MAX_NAME_BYTES))
        .collect();
    tags.sort();
    tags.dedup();
    tags.truncate(MAX_GUEST_TAGS);
    Some(Guest {
        kind,
        vmid: guest.vmid,
        name: clipped(&guest.name, MAX_NAME_BYTES),
        pool: clipped(&guest.pool, MAX_NAME_BYTES),
        tags,
        key: key.as_str().to_string(),
    })
}

/// Stores what the agent reported. `last` is the list written earlier in this
/// session: an unchanged list only refreshes the collection time.
pub async fn record(
    state: &AppState,
    host_id: &str,
    facts: &SourceFacts,
    last: &mut Option<Discovery>,
    now_ms: i64,
) -> anyhow::Result<()> {
    let discovery = Discovery::from_proto(facts);
    let age = i64::try_from(facts.age_ms).unwrap_or(i64::MAX);
    let collected_at = now_ms.saturating_sub(age).max(0);
    if last.as_ref() == Some(&discovery) {
        sqlx::query("UPDATE agents SET sources_collected_at = $1 WHERE id = $2")
            .bind(collected_at)
            .bind(host_id)
            .execute(&state.db.pool)
            .await
            .context("recording the collection time")?;
        return Ok(());
    }
    let mut tx = state
        .db
        .pool
        .begin()
        .await
        .context("starting a transaction")?;
    sqlx::query("UPDATE agents SET sources_json = $1, sources_collected_at = $2 WHERE id = $3")
        .bind(serde_json::to_string(&discovery)?)
        .bind(collected_at)
        .bind(host_id)
        .execute(&mut *tx)
        .await
        .context("storing the sources")?;
    sqlx::query("DELETE FROM discovered_sources WHERE host_id = $1")
        .bind(host_id)
        .execute(&mut *tx)
        .await
        .context("clearing the discovered keys")?;
    for key in discovery.keys() {
        sqlx::query("INSERT INTO discovered_sources (host_id, source_ref) VALUES ($1, $2)")
            .bind(host_id)
            .bind(key)
            .execute(&mut *tx)
            .await
            .context("storing a discovered key")?;
    }
    tx.commit().await.context("committing the sources")?;
    *last = Some(discovery);
    Ok(())
}

pub fn parse_stored(json: Option<&str>, collected_at: Option<i64>) -> Option<Stored> {
    let json = json?;
    match serde_json::from_str(json) {
        Ok(discovery) => Some(Stored {
            discovery,
            collected_at,
        }),
        Err(error) => {
            tracing::warn!(%error, "stored sources are unreadable");
            None
        }
    }
}

pub async fn load(state: &AppState, host_id: &str) -> anyhow::Result<Option<Stored>> {
    let row = sqlx::query("SELECT sources_json, sources_collected_at FROM agents WHERE id = $1")
        .bind(host_id)
        .fetch_optional(&state.db.pool)
        .await
        .context("reading the sources")?;
    let Some(row) = row else { return Ok(None) };
    let json: Option<String> = row.try_get("sources_json")?;
    let collected_at: Option<i64> = row.try_get("sources_collected_at")?;
    Ok(parse_stored(json.as_deref(), collected_at))
}
