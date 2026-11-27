//! Restore of a Proxmox VE container as a guest (ADR 0008 § Restore). The
//! server only queues the job and shows what the filter of the agent will
//! drop; the agent filters the snapshot configuration itself.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use lodestore_format::Error as FormatError;
use lodestore_format::ct::CtSnapshot;
use lodestore_format::index::parse_hex32;
use lodestore_format::source::{self, Meta};
use lodestore_proto::agent::v1::restore_spec::Target;
use lodestore_proto::agent::v1::{ConflictPolicy, GuestRestoreResult, GuestTarget, RestoreSpec};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use utoipa::ToSchema;

use super::api::RestoreCreated;
use super::restore::{GuestRestoreRecord, RestoreRecord, conflict_name, insert_restore_job};
use crate::auth::tokens::{RestoreGuestWrite, Scope, Scoped};
use crate::browse::tree::{read_error, reader};
use crate::error::{ApiError, ErrorBody};
use crate::grpc::registry::CAP_GUEST_RESTORE_CT;
use crate::sources::{self, ProxmoxNode, Stored};
use crate::state::AppState;

/// The agent's report keeps at most this many dropped keys.
pub const MAX_DROPPED_REPORTED: usize = 256;
/// A replace is handed to the agent only this long after it was confirmed.
pub const REPLACE_TTL_MS: i64 = 15 * 60 * 1000;
/// The reasons the filter gives (`DropReason::as_str`); anything else the agent reports is `unknown`.
pub const DROP_REASONS: [&str; 11] = [
    "hookscript",
    "lxc",
    "bind_mount",
    "device",
    "privileged",
    "features",
    "args",
    "network",
    "firewall",
    "unknown",
    "section",
];
const MAX_REPORTED_VALUE: usize = 256;
const MAX_NETWORK_REPORTED: usize = 32;
const MAX_NETWORK_VALUE: usize = 512;
const MAX_NAME: usize = 64;
const MIN_VMID: u32 = 100;
const MAX_VMID: u32 = 999_999_999;
const CT_ITEMS: [&[u8]; 2] = [b"/fs", b"/config"];

const NEW_PURPOSE: &str = "restore a container to a new VMID";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct DroppedKey {
    pub key: String,
    pub value: String,
    /// One of hookscript, lxc, bind_mount, device, privileged, features, args, network, firewall, unknown, section.
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct GuestConfigLine {
    pub key: String,
    pub value: String,
}

/// The guest a restore job creates or replaces.
#[derive(Serialize, ToSchema)]
pub struct GuestRestoreInfo {
    pub node: String,
    pub vmid: u32,
    pub storage: String,
    pub replace: bool,
}

/// What the agent reports about the guest it restored. It is the agent's
/// claim, clipped: at most 256 dropped keys with values of at most 256 bytes.
#[derive(Debug, Default, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct GuestResultDto {
    pub vmid: u32,
    pub name: String,
    pub node: String,
    pub storage: String,
    pub replaced: bool,
    pub dropped: Vec<DroppedKey>,
    /// How many keys the agent dropped; more than `dropped` holds when the list was cut.
    pub dropped_total: u64,
    /// The network interfaces that stay, as `netN` and its value.
    pub network: Vec<GuestConfigLine>,
    /// The `netN` that got a new MAC address.
    pub new_mac_nets: Vec<String>,
    pub unprivileged: bool,
    /// The uid of root of the container on the node.
    pub root_host_uid: u32,
}

fn clip(text: &str, max: usize) -> String {
    text[..text.floor_char_boundary(max)].to_owned()
}

/// The agent's report of a guest restore, bounded before it is stored.
pub fn guest_result(report: &GuestRestoreResult) -> GuestResultDto {
    GuestResultDto {
        vmid: report.vmid,
        name: clip(&report.name, MAX_NAME),
        node: clip(&report.node, MAX_NAME),
        storage: clip(&report.storage, MAX_NAME),
        replaced: report.replaced,
        dropped: report
            .dropped
            .iter()
            .take(MAX_DROPPED_REPORTED)
            .map(|d| DroppedKey {
                key: clip(&d.key, MAX_REPORTED_VALUE),
                value: clip(&d.value, MAX_REPORTED_VALUE),
                reason: DROP_REASONS
                    .iter()
                    .find(|r| **r == d.reason)
                    .unwrap_or(&"unknown")
                    .to_string(),
            })
            .collect(),
        dropped_total: report.dropped.len() as u64,
        network: report
            .network
            .iter()
            .take(MAX_NETWORK_REPORTED)
            .map(|line| GuestConfigLine {
                key: clip(&line.key, MAX_NAME),
                value: clip(&line.value, MAX_NETWORK_VALUE),
            })
            .collect(),
        new_mac_nets: report
            .new_mac_nets
            .iter()
            .take(MAX_NETWORK_REPORTED)
            .map(|net| clip(net, MAX_NAME))
            .collect(),
        unprivileged: report.unprivileged,
        root_host_uid: report.root_host_uid,
    }
}

/// A snapshot of a container as the catalog lists it.
pub(crate) struct CtRow {
    pub id: String,
    pub repository_id: String,
    pub host_id: Option<String>,
    pub hostname: String,
    pub verified: bool,
    pub source_ref: String,
}

impl CtRow {
    /// The scope of the key: the cluster, or the node outside a cluster.
    pub fn cluster(&self) -> &str {
        self.source_ref.split('/').nth(1).unwrap_or_default()
    }
}

pub(crate) async fn load_ct_row(state: &AppState, snapshot_id: &str) -> Result<CtRow, ApiError> {
    let unknown = || ApiError::NotFound("The snapshot was not found.".into());
    parse_hex32(snapshot_id).ok_or_else(unknown)?;
    let row = sqlx::query(
        "SELECT s.repository_id, s.host_id, s.verified_at, s.source_type, s.source_ref, \
         COALESCE(a.hostname, s.hostname) AS hostname \
         FROM snapshots s LEFT JOIN agents a ON a.id = s.host_id WHERE s.id = $1",
    )
    .bind(snapshot_id)
    .fetch_optional(&state.db.pool)
    .await?
    .ok_or_else(unknown)?;
    if row.try_get::<String, _>("source_type")? != "proxmox-ct" {
        return Err(ApiError::Unprocessable(
            "restore_snapshot_not_ct",
            "This snapshot is not of a Proxmox VE container.".into(),
        ));
    }
    Ok(CtRow {
        id: snapshot_id.to_string(),
        repository_id: row.try_get("repository_id")?,
        host_id: row.try_get("host_id")?,
        hostname: row.try_get("hostname")?,
        verified: row.try_get::<Option<i64>, _>("verified_at")?.is_some(),
        source_ref: row.try_get("source_ref")?,
    })
}

/// A container snapshot read from the repository, with the source meta.
pub(crate) struct OpenedCt {
    pub ct: CtSnapshot,
    pub meta: Meta,
}

impl OpenedCt {
    pub fn vmid(&self) -> Option<u32> {
        self.meta.get("vmid").and_then(|v| v.parse().ok())
    }

    pub fn name(&self) -> Option<String> {
        self.meta.get("name").cloned()
    }
}

pub(crate) async fn open_ct(state: &AppState, row: &CtRow) -> Result<OpenedCt, ApiError> {
    let id = parse_hex32(&row.id)
        .ok_or_else(|| ApiError::NotFound("The snapshot was not found.".into()))?;
    let mut reader = reader(state, &row.repository_id)
        .await
        .map_err(read_error)?;
    let unreadable = |error: FormatError| match error {
        FormatError::Corrupt { .. } => ApiError::Unprocessable(
            "guest_config_unreadable",
            format!("The container snapshot cannot be read: {error}."),
        ),
        other => read_error(other),
    };
    let ct = CtSnapshot::open(&mut reader, &id)
        .await
        .map_err(unreadable)?;
    let manifest = reader.snapshot(&id).await.map_err(read_error)?;
    let (_, meta) = source::decode(manifest.source.as_ref()).map_err(unreadable)?;
    Ok(OpenedCt { ct, meta })
}

/// An agent that is not revoked, with what it last reported.
pub(crate) struct AgentHost {
    pub id: String,
    pub hostname: String,
    pub capabilities: Vec<String>,
    pub stored: Option<Stored>,
}

impl AgentHost {
    pub fn proxmox(&self) -> Option<&ProxmoxNode> {
        self.stored.as_ref()?.discovery.proxmox.as_ref()
    }

    /// The node name Proxmox VE uses; the hostname when the agent did not say.
    pub fn node(&self) -> String {
        self.proxmox()
            .map(|p| p.node.clone())
            .filter(|node| !node.is_empty())
            .unwrap_or_else(|| self.hostname.clone())
    }
}

pub(crate) async fn load_hosts(state: &AppState) -> Result<Vec<AgentHost>, ApiError> {
    let rows = sqlx::query(
        "SELECT id, hostname, capabilities_json, sources_json, sources_collected_at \
         FROM agents WHERE status <> 'revoked' ORDER BY hostname, id",
    )
    .fetch_all(&state.db.pool)
    .await?;
    rows.iter()
        .map(|row| {
            let capabilities: Option<String> = row.try_get("capabilities_json")?;
            let json: Option<String> = row.try_get("sources_json")?;
            Ok(AgentHost {
                id: row.try_get("id")?,
                hostname: row.try_get("hostname")?,
                capabilities: sources::parse_capabilities(capabilities.as_deref()),
                stored: sources::parse_stored(
                    json.as_deref(),
                    row.try_get("sources_collected_at")?,
                ),
            })
        })
        .collect()
}

/// Whether two reports name the same scope: one cluster, or one node outside a cluster.
fn same_scope(a: &ProxmoxNode, b: &ProxmoxNode) -> bool {
    a.scope == b.scope && a.name == b.name
}

#[derive(Deserialize, ToSchema)]
pub struct GuestRestoreBody {
    pub snapshot_id: String,
    /// The host of the Proxmox VE node to restore on.
    pub target_host_id: String,
    /// The storage of the node for the volumes of the container; it must hold `rootdir`.
    pub storage: String,
    /// The VMID of a new container. Give either `vmid` or `replace_vmid`.
    pub vmid: Option<u32>,
    /// The VMID of the container to replace on the node.
    pub replace_vmid: Option<u32>,
    /// Must be true to replace a container with a snapshot of another guest, or when either has no `meta uid`.
    #[serde(default)]
    pub confirm_other_guest: bool,
    /// Must be true to restore a snapshot that nothing signed as a new container; such a snapshot cannot replace one.
    #[serde(default)]
    pub accept_unverified: bool,
}

#[utoipa::path(
    post,
    tag = "Restore",
    summary = "Restore a container snapshot as a Proxmox VE container",
    security(("session" = [], "csrf" = []), ("bearer" = ["restore:guest"]), ("bearer" = ["restore:guest-in-place"])),
    path = "/api/v1/restores/guest",
    request_body = GuestRestoreBody,
    responses(
        (status = 201, description = "The restore job is queued", body = RestoreCreated),
        (status = 400, description = "`restore_unverified_in_place`: nothing signed the snapshot, so it cannot replace a container; `restore_unverified_not_confirmed`: nothing signed the snapshot and `accept_unverified` is not set; `restore_other_guest_not_confirmed`: the snapshot is of another guest than the target, or one of them has no uid, and `confirm_other_guest` is not set; `guest_in_place_disabled`: the agent does not allow replacing a guest", body = ErrorBody),
        (status = 403, description = "The token lacks `restore:guest` for a new container or `restore:guest-in-place` to replace one; or `step_up_required`: a session replaces a container and the password was not entered in the last 15 minutes", body = ErrorBody),
        (status = 404, description = "Unknown snapshot or host, or the container to replace is not on the node", body = ErrorBody),
        (status = 409, description = "`restore_vmid_taken`: a guest of the cluster has the VMID; `guest_running`: the container to replace is running; `guest_facts_stale`: the node has not listed its containers in the last 15 minutes", body = ErrorBody),
        (status = 422, description = "`restore_snapshot_not_ct`: the snapshot is not of a container; `guest_config_unreadable`: the container snapshot cannot be read; `restore_agent_outdated`: the agent cannot restore containers; `guest_plugin_missing`: the Lodestore plugin is not installed on the node; `restore_storage_invalid`: the node has no such storage for containers; `guest_ha_managed`: the container to replace is an HA resource; `guest_target_unverifiable`: the node did not list every volume of the container to replace; `validation`: neither or both of `vmid` and `replace_vmid`, or a VMID out of range", body = ErrorBody),
        (status = 502, description = "The repository storage is unavailable", body = ErrorBody),
    )
)]
pub async fn create_guest_restore(
    State(state): State<AppState>,
    auth: Scoped<RestoreGuestWrite>,
    Json(body): Json<GuestRestoreBody>,
) -> Result<(StatusCode, Json<RestoreCreated>), ApiError> {
    let vmid = match (body.vmid, body.replace_vmid) {
        (Some(vmid), None) => vmid,
        _ => {
            return Err(ApiError::Validation(
                "Give vmid for a new container.".into(),
            ));
        }
    };
    auth.require_also(Scope::RestoreGuest, NEW_PURPOSE)?;
    if !(MIN_VMID..=MAX_VMID).contains(&vmid) {
        return Err(ApiError::Validation(format!(
            "A VMID is between {MIN_VMID} and {MAX_VMID}."
        )));
    }
    let row = load_ct_row(&state, &body.snapshot_id).await?;
    if !row.verified && !body.accept_unverified {
        return Err(ApiError::BadRequest(
            "restore_unverified_not_confirmed",
            "Nothing signed this snapshot. Confirm the restore to restore it as a new \
             container."
                .into(),
        ));
    }
    let hosts = load_hosts(&state).await?;
    let target = hosts
        .iter()
        .find(|h| h.id == body.target_host_id)
        .ok_or_else(|| ApiError::NotFound("The host was not found.".into()))?;
    if !target
        .capabilities
        .iter()
        .any(|c| c == CAP_GUEST_RESTORE_CT)
    {
        return Err(ApiError::Unprocessable(
            "restore_agent_outdated",
            format!(
                "The agent on {} cannot restore containers yet. Update the agent.",
                target.hostname
            ),
        ));
    }
    let facts = target
        .proxmox()
        .filter(|p| p.plugin_installed)
        .ok_or_else(|| {
            ApiError::Unprocessable(
                "guest_plugin_missing",
                format!(
                    "The Lodestore plugin is not installed on {}. Run lodestore-agent pve install \
                 on the node.",
                    target.hostname
                ),
            )
        })?;
    if !facts
        .storages
        .iter()
        .any(|s| s.id == body.storage && s.content.iter().any(|c| c == "rootdir"))
    {
        return Err(ApiError::Unprocessable(
            "restore_storage_invalid",
            format!(
                "{} has no storage {} for containers.",
                target.hostname, body.storage
            ),
        ));
    }
    let taken = hosts
        .iter()
        .filter_map(AgentHost::proxmox)
        .filter(|p| same_scope(p, facts))
        .flat_map(|p| &p.guests)
        .any(|g| g.vmid == vmid);
    if taken {
        return Err(ApiError::ConflictCode(
            "restore_vmid_taken",
            format!("VMID {vmid} is taken in {}.", facts.name),
        ));
    }
    open_ct(&state, &row).await?;

    let node = target.node();
    let guest = GuestRestoreRecord {
        node: node.clone(),
        vmid,
        storage: body.storage.clone(),
        replace: false,
        expected: None,
        confirm_other_guest: None,
    };
    let spec = RestoreSpec {
        snapshot_id: row.id.clone(),
        conflict: ConflictPolicy::Unspecified as i32,
        target: Some(Target::Guest(GuestTarget {
            node,
            vmid,
            in_place: false,
            storage: body.storage,
            ..Default::default()
        })),
        guest_snapshot: true,
        ..Default::default()
    };
    let record = RestoreRecord {
        snapshot_id: row.id,
        source_repository_id: row.repository_id,
        source_host_id: row.host_id,
        source_hostname: row.hostname,
        items_b64: CT_ITEMS
            .iter()
            .map(|item| data_encoding::BASE64URL_NOPAD.encode(item))
            .collect(),
        destination: String::new(),
        in_place: false,
        conflict: conflict_name(ConflictPolicy::Unspecified).to_string(),
        restore_owners: false,
        started_by: auth.actor(),
        image: None,
        guest: Some(guest),
    };
    let job_id = insert_restore_job(&state, &target.id, &spec, &record).await?;
    Ok((StatusCode::CREATED, Json(RestoreCreated { job_id })))
}

#[cfg(test)]
mod tests {
    use lodestore_format::guest_config::DropReason;

    use super::*;

    #[test]
    fn drop_reasons_are_the_filter_reasons() {
        let all = [
            DropReason::Hookscript,
            DropReason::Lxc,
            DropReason::BindMount,
            DropReason::Device,
            DropReason::Privileged,
            DropReason::Features,
            DropReason::Args,
            DropReason::Network,
            DropReason::Firewall,
            DropReason::Unknown,
            DropReason::Section,
        ];
        assert_eq!(all.map(DropReason::as_str), DROP_REASONS);
    }
}
