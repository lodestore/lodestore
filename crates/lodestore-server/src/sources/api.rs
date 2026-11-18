//! The Sources tab of a host: what the agent found and whether a policy covers it.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use axum::Json;
use axum::extract::{Path, State};
use lodestore_format::provenance::FUTURE_TOLERANCE_MS;
use lodestore_format::source::{Meta, SourceKey, label};
use lodestore_policy::Selector;
use lodestore_policy::source::{Source, normalized};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::select::{selects_db, selects_guest};
use super::{
    DbServer, DbType, Discovery, GuestType, SOURCE_TYPES, ScopeKind, declares, load_all,
    parse_capabilities, parse_stored,
};
use crate::auth::tokens::{HostsRead, PoliciesRead, Scoped};
use crate::error::{ApiError, ErrorBody};
use crate::hosts::labels::Label;
use crate::policies::matching::load_agents;
use crate::policies::{PolicyRow, all_policies};
use crate::state::AppState;

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKindDto {
    Cluster,
    Node,
}

/// The Proxmox cluster or the single node the guests belong to.
#[derive(Serialize, ToSchema)]
pub struct ProxmoxScope {
    pub kind: ScopeKindDto,
    pub name: String,
}

/// A database server on the host. Its databases are items with this `id` as `group_id`.
#[derive(Serialize, ToSchema)]
pub struct SourceGroup {
    /// `<type>:<port>`.
    pub id: String,
    pub source_type: String,
    pub port: u16,
    pub version: Option<String>,
    /// The agent found the server but could not list its databases.
    pub unreachable: bool,
    /// Names of other hosts whose agents found one of the same databases.
    pub shared_with: Vec<String>,
}

#[derive(Serialize, ToSchema, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProtectionState {
    /// A policy of the host's repository selects the instance.
    Protected,
    /// No policy selects the instance.
    Unprotected,
    /// The agent does not declare support for the type.
    AgentTooOld,
}

#[derive(Serialize, ToSchema)]
pub struct Protection {
    pub state: ProtectionState,
    /// Names of the policies that select the instance; empty unless protected.
    pub policies: Vec<String>,
}

/// A guest or a database found on the host.
#[derive(Serialize, ToSchema)]
pub struct SourceItem {
    /// `proxmox-vm`, `proxmox-ct`, `postgres` or `mysql`.
    pub source_type: String,
    /// What a person calls it, like `VM 101 · web-01`.
    pub label: String,
    pub name: Option<String>,
    pub vmid: Option<u32>,
    /// The `id` of the database server a database belongs to.
    pub group_id: Option<String>,
    pub pool: Option<String>,
    pub tags: Vec<String>,
    /// The PostgreSQL database OID.
    pub oid: Option<u32>,
    pub protection: Protection,
    /// Reception time of the latest complete, verified snapshot, in milliseconds.
    pub last_snapshot_at: Option<i64>,
    /// Names of other hosts whose agents found the same instance, like a cloned guest.
    pub shared_with: Vec<String>,
}

/// The policies that back up the files of the host.
#[derive(Serialize, ToSchema)]
pub struct FilesSummary {
    pub policies: Vec<String>,
    pub paths: Vec<String>,
    /// Reception time of the latest complete, verified files snapshot, in milliseconds.
    pub last_snapshot_at: Option<i64>,
}

#[derive(Serialize, ToSchema)]
pub struct HostSources {
    /// When the agent collected the list, in milliseconds; absent until it reports.
    pub collected_at: Option<i64>,
    /// The agent cut the list short, so it may miss instances.
    pub truncated: bool,
    pub scope: Option<ProxmoxScope>,
    pub groups: Vec<SourceGroup>,
    pub items: Vec<SourceItem>,
    /// Present only when a files policy covers the host.
    pub files: Option<FilesSummary>,
}

struct Context<'a> {
    capabilities: &'a [String],
    policies: &'a [PolicyRow],
    snapshots: &'a HashMap<String, i64>,
    shared: &'a HashMap<String, BTreeSet<String>>,
}

impl Context<'_> {
    fn protection(&self, source_type: &str, selected: impl Fn(&Source) -> bool) -> Protection {
        if !declares(self.capabilities, source_type) {
            return Protection {
                state: ProtectionState::AgentTooOld,
                policies: Vec::new(),
            };
        }
        let policies: Vec<String> = self
            .policies
            .iter()
            .filter(|p| normalized(&p.policy.spec.source).is_some_and(&selected))
            .map(|p| p.policy.metadata.name.clone())
            .collect();
        Protection {
            state: if policies.is_empty() {
                ProtectionState::Unprotected
            } else {
                ProtectionState::Protected
            },
            policies,
        }
    }

    fn shared_with(&self, key: &str) -> Vec<String> {
        self.shared
            .get(key)
            .map(|hosts| hosts.iter().cloned().collect())
            .unwrap_or_default()
    }
}

fn type_name(kind: DbType) -> &'static str {
    match kind {
        DbType::Postgres => "postgres",
        DbType::Mysql => "mysql",
    }
}

fn describe(key: &str, meta: &[(&str, Option<&str>)], fallback: String) -> String {
    let meta: Meta = meta
        .iter()
        .filter_map(|(k, v)| v.map(|v| (k.to_string(), v.to_string())))
        .collect();
    SourceKey::parse(key)
        .ok()
        .and_then(|key| label(&key, &meta))
        .unwrap_or(fallback)
}

fn guest_items(found: &Discovery, ctx: &Context) -> Vec<SourceItem> {
    let Some(node) = &found.proxmox else {
        return Vec::new();
    };
    let mut guests: Vec<_> = node.guests.iter().collect();
    guests.sort_by_key(|g| (g.kind == GuestType::Ct, g.vmid));
    guests
        .into_iter()
        .map(|guest| {
            let (source_type, prefix) = match guest.kind {
                GuestType::Vm => ("proxmox-vm", "VM"),
                GuestType::Ct => ("proxmox-ct", "CT"),
            };
            let vmid = guest.vmid.to_string();
            SourceItem {
                source_type: source_type.to_string(),
                label: describe(
                    &guest.key,
                    &[("vmid", Some(&vmid)), ("name", guest.name.as_deref())],
                    format!("{prefix} {vmid}"),
                ),
                name: guest.name.clone(),
                vmid: Some(guest.vmid),
                group_id: None,
                pool: guest.pool.clone(),
                tags: guest.tags.clone(),
                oid: None,
                protection: ctx.protection(source_type, |s| selects_guest(s, guest)),
                last_snapshot_at: ctx.snapshots.get(&guest.key).copied(),
                shared_with: ctx.shared_with(&guest.key),
            }
        })
        .collect()
}

fn group_id(server: &DbServer) -> String {
    format!("{}:{}", type_name(server.kind), server.port)
}

fn database_groups(found: &Discovery, ctx: &Context) -> (Vec<SourceGroup>, Vec<SourceItem>) {
    let mut servers: Vec<_> = found.servers.iter().collect();
    servers.sort_by_key(|s| (s.unreachable, s.port, s.kind == DbType::Mysql));
    let mut groups = Vec::new();
    let mut items = Vec::new();
    for server in servers {
        let id = group_id(server);
        let source_type = type_name(server.kind);
        let mut databases: Vec<_> = server.databases.iter().flatten().collect();
        databases.sort_by(|a, b| a.name.cmp(&b.name));
        let mut shared = BTreeSet::new();
        for db in databases {
            let others = ctx.shared_with(&db.key);
            shared.extend(others.iter().cloned());
            items.push(SourceItem {
                source_type: source_type.to_string(),
                label: describe(
                    &db.key,
                    &[
                        ("database", Some(&db.name)),
                        ("server_version", server.version.as_deref()),
                    ],
                    db.name.clone(),
                ),
                name: Some(db.name.clone()),
                vmid: None,
                group_id: Some(id.clone()),
                pool: None,
                tags: Vec::new(),
                oid: db.oid,
                protection: ctx.protection(source_type, |s| selects_db(s, server.kind, db)),
                last_snapshot_at: ctx.snapshots.get(&db.key).copied(),
                shared_with: others,
            });
        }
        groups.push(SourceGroup {
            id,
            source_type: source_type.to_string(),
            port: server.port,
            version: server.version.clone(),
            unreachable: server.unreachable,
            shared_with: shared.into_iter().collect(),
        });
    }
    (groups, items)
}

fn files_summary(policies: &[PolicyRow], snapshots: &HashMap<String, i64>) -> Option<FilesSummary> {
    let files: Vec<_> = policies
        .iter()
        .filter(|p| normalized(&p.policy.spec.source).is_none())
        .collect();
    if files.is_empty() {
        return None;
    }
    let paths: BTreeSet<_> = files
        .iter()
        .flat_map(|p| p.policy.spec.paths.iter().cloned())
        .collect();
    Some(FilesSummary {
        policies: files
            .iter()
            .map(|p| p.policy.metadata.name.clone())
            .collect(),
        paths: paths.into_iter().collect(),
        last_snapshot_at: snapshots.get("").copied(),
    })
}

async fn latest_snapshots(
    state: &AppState,
    host_id: &str,
) -> Result<HashMap<String, i64>, ApiError> {
    let rows = sqlx::query(
        "SELECT source_ref, MAX(received_at) AS received_at FROM snapshots \
         WHERE host_id = $1 AND incomplete = 0 AND verified_at IS NOT NULL \
         AND received_at <= $2 GROUP BY source_ref",
    )
    .bind(host_id)
    .bind(crate::db::now_ms().saturating_add(FUTURE_TOLERANCE_MS))
    .fetch_all(&state.db.pool)
    .await?;
    rows.iter()
        .map(|row| Ok((row.try_get("source_ref")?, row.try_get("received_at")?)))
        .collect()
}

async fn shared_keys(
    state: &AppState,
    host_id: &str,
) -> Result<HashMap<String, BTreeSet<String>>, ApiError> {
    let rows = sqlx::query(
        "SELECT d.source_ref, a.hostname FROM discovered_sources d \
         JOIN agents a ON a.id = d.host_id \
         WHERE a.status <> 'revoked' AND d.host_id <> $1 AND d.source_ref IN \
         (SELECT source_ref FROM discovered_sources WHERE host_id = $1)",
    )
    .bind(host_id)
    .fetch_all(&state.db.pool)
    .await?;
    let mut shared: HashMap<String, BTreeSet<String>> = HashMap::new();
    for row in &rows {
        shared
            .entry(row.try_get("source_ref")?)
            .or_default()
            .insert(row.try_get("hostname")?);
    }
    Ok(shared)
}

#[utoipa::path(
    get,
    tag = "Hosts",
    summary = "List the sources of a host",
    description = "What the agent found on the host besides files, whether a policy covers each instance, and which other hosts report the same instance. Revoked hosts keep their last data.",
    security(("session" = []), ("bearer" = ["hosts:read"])),
    path = "/api/v1/hosts/{id}/sources",
    params(("id" = String, Path, description = "Host id")),
    responses(
        (status = 200, description = "Sources of the host", body = HostSources),
        (status = 404, description = "Unknown host", body = ErrorBody),
    )
)]
pub(crate) async fn host_sources(
    State(state): State<AppState>,
    _auth: Scoped<HostsRead>,
    Path(id): Path<String>,
) -> Result<Json<HostSources>, ApiError> {
    let row = sqlx::query(
        "SELECT repository_id, labels_json, capabilities_json, sources_json, \
         sources_collected_at FROM agents WHERE id = $1",
    )
    .bind(&id)
    .fetch_optional(&state.db.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("host not found".into()))?;
    let repository_id: String = row.try_get("repository_id")?;
    let labels: Vec<Label> = serde_json::from_str(&row.try_get::<String, _>("labels_json")?)
        .map_err(|e| ApiError::Internal(e.into()))?;
    let capabilities = parse_capabilities(
        row.try_get::<Option<String>, _>("capabilities_json")?
            .as_deref(),
    );
    let stored = parse_stored(
        row.try_get::<Option<String>, _>("sources_json")?.as_deref(),
        row.try_get("sources_collected_at")?,
    );
    let policies: Vec<PolicyRow> = all_policies(&state)
        .await?
        .into_iter()
        .filter(|p| {
            p.repository_id == repository_id
                && p.policy.spec.selector.matches(
                    &id,
                    labels.iter().map(|l| (l.key.as_str(), l.value.as_str())),
                )
        })
        .collect();
    let snapshots = latest_snapshots(&state, &id).await?;
    let shared = shared_keys(&state, &id).await?;
    let ctx = Context {
        capabilities: &capabilities,
        policies: &policies,
        snapshots: &snapshots,
        shared: &shared,
    };
    let files = files_summary(&policies, &snapshots);
    let Some(stored) = stored else {
        return Ok(Json(HostSources {
            collected_at: None,
            truncated: false,
            scope: None,
            groups: Vec::new(),
            items: Vec::new(),
            files,
        }));
    };
    let found = &stored.discovery;
    let mut items = guest_items(found, &ctx);
    let (groups, databases) = database_groups(found, &ctx);
    items.extend(databases);
    Ok(Json(HostSources {
        collected_at: stored.collected_at,
        truncated: found.truncated,
        scope: found.proxmox.as_ref().map(|p| ProxmoxScope {
            kind: match p.scope {
                ScopeKind::Cluster => ScopeKindDto::Cluster,
                ScopeKind::Node => ScopeKindDto::Node,
            },
            name: p.name.clone(),
        }),
        groups,
        items,
        files,
    }))
}

#[derive(Deserialize, ToSchema)]
pub struct SourceOptionsRequest {
    /// Repository name, as in `spec.repository`.
    pub repository: String,
    pub selector: Selector,
    /// `proxmox-vm`, `proxmox-ct`, `postgres` or `mysql`.
    pub source_type: String,
}

/// A guest found on the selected hosts, one row per VMID.
#[derive(Serialize, ToSchema)]
pub struct GuestOption {
    pub vmid: u32,
    /// `vm` or `ct`.
    pub kind: String,
    /// Name, pool and tags come from the first host by name that reports the guest.
    pub name: Option<String>,
    pub pool: Option<String>,
    pub tags: Vec<String>,
    pub hosts: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct NamedCount {
    pub name: String,
    /// How many guest rows carry it.
    pub count: u32,
}

/// A database found on the selected hosts, one row per name.
#[derive(Serialize, ToSchema)]
pub struct DatabaseOption {
    pub name: String,
    pub source_type: String,
    pub hosts: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct TruncatedHost {
    pub hostname: String,
    /// How many instances the server received from the host.
    pub count: u32,
}

#[derive(Serialize, ToSchema)]
pub struct SourceOptions {
    pub guests: Vec<GuestOption>,
    pub pools: Vec<NamedCount>,
    pub tags: Vec<NamedCount>,
    pub databases: Vec<DatabaseOption>,
    /// Hosts whose list was cut short, so it may miss instances.
    pub truncated_hosts: Vec<TruncatedHost>,
    /// Hosts with a database server of the type whose databases the agent could not list.
    pub unreachable_hosts: Vec<String>,
}

fn counted(names: BTreeMap<String, u32>) -> Vec<NamedCount> {
    names
        .into_iter()
        .map(|(name, count)| NamedCount { name, count })
        .collect()
}

#[utoipa::path(
    post,
    tag = "Policies",
    summary = "List the guests and databases a policy form can offer",
    description = "What the agents of the hosts a selector takes found, for the given source type. An unreachable database server is listed apart: it is not an empty one.",
    security(("session" = [], "csrf" = []), ("bearer" = ["policies:read"])),
    path = "/api/v1/policies/source-options",
    request_body = SourceOptionsRequest,
    responses(
        (status = 200, description = "Found instances", body = SourceOptions),
        (status = 422, description = "Unknown repository or unsupported type", body = ErrorBody),
    )
)]
pub(crate) async fn source_options(
    State(state): State<AppState>,
    _auth: Scoped<PoliciesRead>,
    Json(req): Json<SourceOptionsRequest>,
) -> Result<Json<SourceOptions>, ApiError> {
    let source_type = req.source_type.as_str();
    if !SOURCE_TYPES.contains(&source_type) {
        return Err(ApiError::Validation(format!(
            "source_type: \"{source_type}\" has no instances to choose from"
        )));
    }
    let repository = sqlx::query("SELECT id FROM repositories WHERE name = $1")
        .bind(&req.repository)
        .fetch_optional(&state.db.pool)
        .await?
        .ok_or_else(|| {
            ApiError::Validation(format!("Repository \"{}\" does not exist.", req.repository))
        })?;
    let repository_id: String = repository.try_get("id")?;
    let agents = load_agents(&state).await?;
    let sources = load_all(&state).await?;
    let wanted = match source_type {
        "proxmox-vm" => Some(GuestType::Vm),
        "proxmox-ct" => Some(GuestType::Ct),
        _ => None,
    };
    let db_kind = match source_type {
        "postgres" => Some(DbType::Postgres),
        "mysql" => Some(DbType::Mysql),
        _ => None,
    };
    let mut guests: BTreeMap<u32, GuestOption> = BTreeMap::new();
    let mut databases: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut truncated_hosts = Vec::new();
    let mut unreachable_hosts = Vec::new();
    for agent in agents
        .iter()
        .filter(|a| a.applies(&repository_id, &req.selector))
    {
        let Some(stored) = sources.get(&agent.id).and_then(|s| s.stored.as_ref()) else {
            continue;
        };
        let found = &stored.discovery;
        if found.truncated {
            truncated_hosts.push(TruncatedHost {
                hostname: agent.hostname.clone(),
                count: u32::try_from(found.keys().len()).unwrap_or(u32::MAX),
            });
        }
        let node = found.proxmox.iter().flat_map(|p| &p.guests);
        for guest in node.filter(|g| Some(g.kind) == wanted) {
            guests
                .entry(guest.vmid)
                .or_insert_with(|| GuestOption {
                    vmid: guest.vmid,
                    kind: if guest.kind == GuestType::Vm {
                        "vm"
                    } else {
                        "ct"
                    }
                    .to_string(),
                    name: guest.name.clone(),
                    pool: guest.pool.clone(),
                    tags: guest.tags.clone(),
                    hosts: Vec::new(),
                })
                .hosts
                .push(agent.hostname.clone());
        }
        for server in found.servers.iter().filter(|s| Some(s.kind) == db_kind) {
            if server.unreachable && !unreachable_hosts.contains(&agent.hostname) {
                unreachable_hosts.push(agent.hostname.clone());
            }
            for db in server.databases.iter().flatten() {
                let hosts = databases.entry(db.name.clone()).or_default();
                if !hosts.contains(&agent.hostname) {
                    hosts.push(agent.hostname.clone());
                }
            }
        }
    }
    let mut pools = BTreeMap::new();
    let mut tags = BTreeMap::new();
    for guest in guests.values() {
        if let Some(pool) = &guest.pool {
            *pools.entry(pool.clone()).or_default() += 1;
        }
        for tag in &guest.tags {
            *tags.entry(tag.clone()).or_default() += 1;
        }
    }
    Ok(Json(SourceOptions {
        guests: guests.into_values().collect(),
        pools: counted(pools),
        tags: counted(tags),
        databases: databases
            .into_iter()
            .map(|(name, hosts)| DatabaseOption {
                name,
                source_type: source_type.to_string(),
                hosts,
            })
            .collect(),
        truncated_hosts,
        unreachable_hosts,
    }))
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(host_sources, source_options))
}
