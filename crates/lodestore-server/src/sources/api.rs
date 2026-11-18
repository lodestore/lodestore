//! The Sources tab of a host: what the agent found and whether a policy covers it.

use std::collections::{BTreeSet, HashMap};

use axum::Json;
use axum::extract::{Path, State};
use lodestore_format::source::{Meta, SourceKey, label};
use lodestore_policy::source::{Source, normalized};
use serde::Serialize;
use sqlx::Row;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::select::{selects_db, selects_guest};
use super::{
    DbServer, DbType, Discovery, GuestType, ScopeKind, declares, parse_capabilities, parse_stored,
};
use crate::auth::tokens::{HostsRead, Scoped};
use crate::error::{ApiError, ErrorBody};
use crate::hosts::labels::Label;
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
         WHERE host_id = $1 AND incomplete = 0 AND verified_at IS NOT NULL GROUP BY source_ref",
    )
    .bind(host_id)
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

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(host_sources))
}
