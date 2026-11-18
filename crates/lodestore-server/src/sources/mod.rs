//! The sources an agent reports for its host.
//!
//! The list is untrusted: the server bounds it, builds every canonical key
//! itself and drops what does not fit. It is stored as facts about the host and
//! grants nothing.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Context as _;
use lodestore_format::source::SourceKey;
use lodestore_proto::agent::v1::{DatabaseKind, GuestKind, SourceFacts};
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
        let mut servers = Vec::new();
        let mut identities = HashSet::new();
        let mut databases_left = MAX_DATABASES;
        for server in &facts.databases {
            if servers.len() >= MAX_SERVERS {
                truncated = true;
                break;
            }
            let found = db_server(
                server,
                &mut identities,
                &mut seen,
                &mut databases_left,
                &mut truncated,
            );
            match found {
                Some(server) => servers.push(server),
                None => truncated = true,
            }
        }
        Discovery {
            truncated,
            proxmox,
            servers,
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

fn db_server(
    server: &lodestore_proto::agent::v1::DatabaseServer,
    identities: &mut HashSet<String>,
    seen: &mut HashSet<String>,
    databases_left: &mut usize,
    truncated: &mut bool,
) -> Option<DbServer> {
    let kind = match DatabaseKind::try_from(server.kind).ok()? {
        DatabaseKind::Postgres => DbType::Postgres,
        DatabaseKind::Mysql => DbType::Mysql,
        DatabaseKind::Unspecified => return None,
    };
    let port = u16::try_from(server.port).ok()?;
    if !server.unreachable {
        let valid_id = match kind {
            DbType::Postgres => {
                port != 0
                    && server
                        .server_id
                        .parse::<u64>()
                        .is_ok_and(|n| n != 0 && n.to_string() == server.server_id)
            }
            DbType::Mysql => {
                server.server_id != NIL_UUID && SourceKey::mysql(&server.server_id, "probe").is_ok()
            }
        };
        if !valid_id {
            return None;
        }
    }
    let identity = if !server.server_id.is_empty() {
        format!("{kind:?}/id/{}", server.server_id)
    } else if port != 0 {
        format!("{kind:?}/port/{port}")
    } else {
        String::new()
    };
    if !identity.is_empty() && !identities.insert(identity) {
        return None;
    }
    let databases = (!server.unreachable).then(|| {
        let mut kept = Vec::new();
        for database in &server.databases {
            if *databases_left == 0 {
                *truncated = true;
                break;
            }
            match db_entry(kind, &server.server_id, database, seen) {
                Some(entry) => {
                    *databases_left -= 1;
                    kept.push(entry);
                }
                None => *truncated = true,
            }
        }
        kept
    });
    Some(DbServer {
        kind,
        port,
        version: clipped(&server.version, MAX_VERSION_BYTES),
        unreachable: server.unreachable,
        databases,
    })
}

fn db_entry(
    kind: DbType,
    server_id: &str,
    database: &lodestore_proto::agent::v1::Database,
    seen: &mut HashSet<String>,
) -> Option<Db> {
    let (key, name, oid) = match kind {
        DbType::Postgres => (
            SourceKey::postgres(server_id.parse().ok()?, database.oid).ok()?,
            clipped(&database.name, MAX_NAME_BYTES)?,
            Some(database.oid),
        ),
        DbType::Mysql => (
            SourceKey::mysql(server_id, &database.name).ok()?,
            (database.name.len() <= MAX_NAME_BYTES).then(|| database.name.clone())?,
            None,
        ),
    };
    seen.insert(key.as_str().to_string()).then(|| Db {
        name,
        oid,
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

#[cfg(test)]
mod tests {
    use lodestore_proto::agent::v1::{
        Database, DatabaseKind, DatabaseServer, ProxmoxFacts, ProxmoxGuest,
    };

    use super::*;

    fn uuid(n: u32) -> String {
        format!("{n:08x}-1111-4111-8111-111111111111")
    }

    fn guest(kind: GuestKind, vmid: u32, uuid: &str) -> ProxmoxGuest {
        ProxmoxGuest {
            kind: kind as i32,
            vmid,
            uuid: uuid.to_string(),
            ..Default::default()
        }
    }

    fn vm(vmid: u32) -> ProxmoxGuest {
        guest(GuestKind::Vm, vmid, &uuid(vmid))
    }

    fn ct(vmid: u32) -> ProxmoxGuest {
        guest(GuestKind::Ct, vmid, &uuid(vmid))
    }

    fn node(guests: Vec<ProxmoxGuest>) -> SourceFacts {
        SourceFacts {
            proxmox: Some(ProxmoxFacts {
                cluster: "c1".into(),
                node: "pve1".into(),
                guests,
            }),
            ..Default::default()
        }
    }

    fn db(name: &str, oid: u32) -> Database {
        Database {
            name: name.into(),
            oid,
        }
    }

    fn pg(port: u32, id: &str, databases: Vec<Database>) -> DatabaseServer {
        DatabaseServer {
            kind: DatabaseKind::Postgres as i32,
            port,
            server_id: id.into(),
            databases,
            ..Default::default()
        }
    }

    fn mysql(port: u32, id: &str, schemas: &[&str]) -> DatabaseServer {
        DatabaseServer {
            kind: DatabaseKind::Mysql as i32,
            port,
            server_id: id.into(),
            databases: schemas.iter().map(|s| db(s, 0)).collect(),
            ..Default::default()
        }
    }

    fn servers(servers: Vec<DatabaseServer>) -> SourceFacts {
        SourceFacts {
            databases: servers,
            ..Default::default()
        }
    }

    fn guests(d: &Discovery) -> &[Guest] {
        d.proxmox.as_ref().map_or(&[], |p| p.guests.as_slice())
    }

    #[test]
    fn oversized_lists_are_cut_and_marked_truncated() {
        let within = Discovery::from_proto(&node((100..110).map(vm).collect()));
        assert!(!within.truncated);
        assert_eq!(guests(&within).len(), 10);

        let many = Discovery::from_proto(&node((100..1600).map(vm).collect()));
        assert_eq!(guests(&many).len(), MAX_GUESTS);
        assert!(many.truncated);

        let mut tagged = vm(100);
        tagged.tags = (0..25).rev().map(|n| format!("t{n:02}")).collect();
        tagged.tags.push("t00".into());
        let tagged = Discovery::from_proto(&node(vec![tagged]));
        let tags = &guests(&tagged)[0].tags;
        assert_eq!(tags.len(), MAX_GUEST_TAGS);
        assert_eq!(tags[0], "t00");
        assert_eq!(tags[19], "t19");

        let many_servers = Discovery::from_proto(&servers(
            (1..=20)
                .map(|n| pg(5000 + n, &n.to_string(), vec![]))
                .collect(),
        ));
        assert_eq!(many_servers.servers.len(), MAX_SERVERS);
        assert!(many_servers.truncated);

        let dbs = (1..=1500).map(|n| db("d", n)).collect();
        let many_dbs = Discovery::from_proto(&servers(vec![pg(5432, "7", dbs)]));
        let kept = many_dbs.servers[0].databases.as_ref().unwrap();
        assert_eq!(kept.len(), MAX_DATABASES);
        assert!(many_dbs.truncated);

        let few = Discovery::from_proto(&servers(vec![pg(5432, "7", vec![db("app", 16384)])]));
        assert!(!few.truncated);
    }

    #[test]
    fn fields_are_clipped_on_a_char_boundary() {
        let mut g = vm(100);
        g.name = "é".repeat(300);
        g.pool = "é".repeat(300);
        let mut server = pg(5432, "7", vec![db(&"é".repeat(300), 1)]);
        server.version = format!("16.4{}", "é".repeat(100));
        let mut facts = node(vec![g]);
        facts.databases = vec![server];
        let d = Discovery::from_proto(&facts);
        let name = guests(&d)[0].name.as_deref().unwrap();
        assert!(name.len() <= MAX_NAME_BYTES);
        assert!(name.chars().all(|c| c == 'é'));
        assert!(guests(&d)[0].pool.as_deref().unwrap().len() <= MAX_NAME_BYTES);
        let server = &d.servers[0];
        assert!(server.version.as_deref().unwrap().len() <= MAX_VERSION_BYTES);
        let dbname = &server.databases.as_ref().unwrap()[0].name;
        assert!(dbname.len() <= MAX_NAME_BYTES && dbname.chars().all(|c| c == 'é'));
    }

    #[test]
    fn items_without_a_valid_key_are_dropped() {
        let all = vec![
            vm(100),
            guest(GuestKind::Vm, 101, &uuid(0xabc).to_uppercase()),
            guest(GuestKind::Vm, 102, "not-a-uuid"),
            guest(GuestKind::Vm, 99, &uuid(99)),
            guest(GuestKind::Vm, 1_000_000_000, &uuid(1)),
            guest(GuestKind::Unspecified, 103, &uuid(103)),
            guest(GuestKind::Vm, 100, &uuid(500)),
        ];
        let d = Discovery::from_proto(&node(all));
        assert_eq!(guests(&d).len(), 1);
        assert_eq!(guests(&d)[0].vmid, 100);
        assert!(d.truncated);

        let nodeless = Discovery::from_proto(&SourceFacts {
            proxmox: Some(ProxmoxFacts {
                guests: vec![vm(100)],
                ..Default::default()
            }),
            ..Default::default()
        });
        assert!(nodeless.proxmox.is_none() && nodeless.truncated);

        let server_uuid = uuid(9);
        let long_schema = "s".repeat(65);
        let facts = servers(vec![
            pg(5432, "0", vec![db("a", 1)]),
            pg(5433, "abc", vec![db("a", 1)]),
            pg(5434, "+5", vec![db("a", 1)]),
            pg(0, "6", vec![db("a", 1)]),
            pg(70_000, "8", vec![db("a", 1)]),
            mysql(3306, "nope", &["a"]),
            DatabaseServer {
                kind: DatabaseKind::Unspecified as i32,
                port: 1,
                ..Default::default()
            },
            mysql(3307, &server_uuid, &[&long_schema, "ok"]),
            pg(5435, "10", vec![db("zero", 0), db("fine", 5)]),
        ]);
        let d = Discovery::from_proto(&facts);
        assert!(d.truncated);
        assert_eq!(d.servers.len(), 2);
        let my = d.servers[0].databases.as_ref().unwrap();
        assert_eq!(my.len(), 1);
        assert_eq!(my[0].name, "ok");
        let pgdb = d.servers[1].databases.as_ref().unwrap();
        assert_eq!(pgdb.len(), 1);
        assert_eq!(pgdb[0].key, "postgres/10/5");
    }

    #[test]
    fn repeated_key_is_dropped_and_marks_truncated() {
        let same = uuid(77);
        let d = Discovery::from_proto(&node(vec![
            guest(GuestKind::Vm, 100, &same),
            guest(GuestKind::Vm, 101, &same),
            vm(102),
        ]));
        let vmids: Vec<u32> = guests(&d).iter().map(|g| g.vmid).collect();
        assert_eq!(vmids, [100, 102]);
        assert!(d.truncated);

        let nil = "00000000-0000-0000-0000-000000000000";
        let d = Discovery::from_proto(&node(vec![guest(GuestKind::Ct, 100, nil), ct(101)]));
        assert_eq!(guests(&d).len(), 1);
        assert!(d.truncated);

        let nil_mysql = Discovery::from_proto(&servers(vec![mysql(3306, nil, &["a"])]));
        assert!(nil_mysql.servers.is_empty() && nil_mysql.truncated);

        let d = Discovery::from_proto(&servers(vec![pg(
            5432,
            "7",
            vec![db("a", 3), db("b", 3), db("c", 4)],
        )]));
        assert_eq!(d.keys(), ["postgres/7/3", "postgres/7/4"]);
        assert!(d.truncated);
    }

    #[test]
    fn same_server_id_on_two_ports_is_reported_once() {
        let d = Discovery::from_proto(&servers(vec![
            pg(5432, "7", vec![db("app", 16384)]),
            pg(5433, "7", vec![db("app", 16384)]),
            mysql(3306, &uuid(1), &["a"]),
            mysql(3307, &uuid(1), &["a"]),
        ]));
        assert_eq!(d.servers.len(), 2);
        assert_eq!(d.servers[0].port, 5432);
        assert_eq!(d.servers[1].port, 3306);
        assert!(d.truncated);
        assert_eq!(d.keys().len(), 2);
    }

    #[test]
    fn reachable_mysql_with_port_zero_is_kept() {
        let d = Discovery::from_proto(&servers(vec![mysql(0, &uuid(1), &["a"])]));
        assert_eq!(d.servers.len(), 1);
        assert_eq!(d.servers[0].port, 0);
        assert!(!d.truncated);
        assert!(d.has("mysql"));

        let down = || DatabaseServer {
            kind: DatabaseKind::Mysql as i32,
            unreachable: true,
            ..Default::default()
        };
        let d = Discovery::from_proto(&servers(vec![down(), down()]));
        assert_eq!(d.servers.len(), 2);
        assert!(!d.truncated);
    }

    #[test]
    fn unreachable_server_has_unknown_databases() {
        let mut down = pg(5432, "7", vec![db("a", 1), db("b", 2), db("c", 3)]);
        down.unreachable = true;
        let d = Discovery::from_proto(&servers(vec![down, pg(5433, "8", vec![])]));
        assert_eq!(d.servers[0].databases, None);
        assert!(d.servers[0].unreachable);
        assert_eq!(d.servers[1].databases, Some(vec![]));
        assert!(d.keys().is_empty());
        assert!(d.has("postgres"));
    }

    #[test]
    fn empty_facts_mean_nothing_found() {
        let d = Discovery::from_proto(&SourceFacts::default());
        assert!(d.proxmox.is_none() && d.servers.is_empty() && !d.truncated);
        assert!(d.types().is_empty());
    }

    #[test]
    fn scope_is_cluster_or_node() {
        let mut facts = node(vec![vm(100)]);
        facts.proxmox.as_mut().unwrap().cluster.clear();
        let d = Discovery::from_proto(&facts);
        let p = d.proxmox.as_ref().unwrap();
        assert_eq!(p.scope, ScopeKind::Node);
        assert_eq!(p.guests[0].key, format!("proxmox-vm/pve1/{}", uuid(100)));

        let d = Discovery::from_proto(&node(vec![vm(100)]));
        let p = d.proxmox.as_ref().unwrap();
        assert_eq!(p.scope, ScopeKind::Cluster);
        assert_eq!(p.guests[0].key, format!("proxmox-vm/c1/{}", uuid(100)));
    }

    #[test]
    fn freshness_window() {
        let now = 10_000_000_i64;
        let minute = 60_000;
        assert!(is_fresh(Some(now - 14 * minute), now));
        assert!(!is_fresh(Some(now - 16 * minute), now));
        assert!(!is_fresh(None, now));
    }

    #[test]
    fn types_follow_the_reported_guests() {
        let only_vm = Discovery::from_proto(&node(vec![vm(100)]));
        assert_eq!(only_vm.types(), ["proxmox-vm"]);
        assert!(!only_vm.has("proxmox-ct"));
        let only_ct = Discovery::from_proto(&node(vec![ct(100)]));
        assert_eq!(only_ct.types(), ["proxmox-ct"]);

        let mut empty = node(vec![]);
        empty.truncated = true;
        assert!(Discovery::from_proto(&empty).types().is_empty());

        let mut down = pg(5432, "7", vec![]);
        down.unreachable = true;
        let mut facts = node(vec![vm(100), ct(101)]);
        facts.databases = vec![down];
        assert_eq!(
            Discovery::from_proto(&facts).types(),
            ["proxmox-vm", "proxmox-ct", "postgres"]
        );

        let d = Discovery::from_proto(&servers(vec![mysql(3306, &uuid(1), &[])]));
        assert_eq!(d.types(), ["mysql"]);
        assert!(!d.has("files") && !d.has("whatever"));
    }
}
