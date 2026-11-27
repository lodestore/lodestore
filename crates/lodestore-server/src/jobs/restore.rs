use lodestore_format::crypto::BlobId;
use lodestore_format::ct::{self, IdMap};
use lodestore_format::image::{ImageMeta, load_image};
use lodestore_format::index::parse_hex32;
use lodestore_format::reader::Reader;
use lodestore_format::selection::Selection;
use lodestore_format::source::{self, Meta, SourceKey, SourceType};
use lodestore_format::store::Store;
use lodestore_proto::agent::v1::{ConflictPolicy, RestoreSpec};
use prost::Message;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tokio::sync::Semaphore;

use crate::browse::tree::{read_error, reader, require_items_exist};
use crate::db::now_ms;
use crate::error::ApiError;
use crate::grpc::registry::CAP_RESTORE_ROOTS;
use crate::grpc::restore::{MAX_IMAGE_ENTRY, check_image_entry};
use crate::state::AppState;

pub const MAX_ITEMS: usize = 10_000;
pub const IMAGE_ENQUEUE_LIMIT: usize = 2;
/// Bounds the image layouts the server holds at once, in enqueue checks and in plans.
pub static IMAGE_ENQUEUE: Semaphore = Semaphore::const_new(IMAGE_ENQUEUE_LIMIT);

#[derive(Debug, Clone)]
pub struct RestoreRequest {
    pub snapshot_id: String,
    pub items: Vec<Vec<u8>>,
    pub target_host_id: String,
    pub destination: String,
    pub in_place: bool,
    pub conflict: ConflictPolicy,
    pub restore_owners: bool,
    pub accept_unverified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreRecord {
    pub snapshot_id: String,
    pub source_repository_id: String,
    pub source_host_id: Option<String>,
    pub source_hostname: String,
    pub items_b64: Vec<String>,
    pub destination: String,
    pub in_place: bool,
    pub conflict: String,
    pub restore_owners: bool,
    pub started_by: String,
    /// The disk to restore whole as `NAME.img`; absent for a restore of files.
    #[serde(default)]
    pub image: Option<String>,
    /// The guest to create or replace; absent for a restore of files.
    #[serde(default)]
    pub guest: Option<GuestRestoreRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuestRestoreRecord {
    pub node: String,
    pub vmid: u32,
    pub storage: String,
    pub replace: bool,
    /// The target of a replace as the user confirmed it.
    pub expected: Option<ExpectedTarget>,
    /// (uid of the snapshot, uid of the target), empty for none.
    pub confirm_other_guest: Option<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExpectedTarget {
    pub vmid: u32,
    pub name: String,
    pub pve_uid: String,
    pub pve_rtime: u64,
    pub instance_uuid: String,
    pub volumes: Vec<crate::sources::VolumeLine>,
}

impl RestoreRecord {
    pub fn items(&self) -> Option<Vec<Vec<u8>>> {
        self.items_b64
            .iter()
            .map(|item| data_encoding::BASE64URL_NOPAD.decode(item.as_bytes()).ok())
            .collect()
    }
}

pub fn conflict_name(policy: ConflictPolicy) -> &'static str {
    match policy {
        ConflictPolicy::SkipExisting => "skip_existing",
        ConflictPolicy::StopIfNotEmpty => "stop_if_not_empty",
        ConflictPolicy::OverwriteChanged | ConflictPolicy::Unspecified => "overwrite_changed",
    }
}

fn invalid(message: &str) -> ApiError {
    ApiError::Validation(message.to_string())
}

/// Checks the destination and returns it without empty and `.` components.
fn check_destination(req: &RestoreRequest) -> Result<String, ApiError> {
    let destination = &req.destination;
    if !destination.starts_with('/') || destination.contains('\0') {
        return Err(invalid("The destination must be an absolute path."));
    }
    let parts: Vec<&str> = destination
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    if parts.contains(&"..") {
        return Err(invalid("The destination must not contain \"..\"."));
    }
    let normalized = format!("/{}", parts.join("/"));
    if req.in_place {
        if normalized != "/" {
            return Err(invalid("A restore in place needs the destination \"/\"."));
        }
        if req.conflict == ConflictPolicy::StopIfNotEmpty {
            return Err(ApiError::BadRequest(
                "restore_in_place_not_empty",
                "\"Stop if the target is not empty\" cannot be used with a restore in place: \
                 the original location is never empty."
                    .into(),
            ));
        }
    } else if normalized == "/" {
        return Err(ApiError::BadRequest(
            "restore_destination_root",
            "Choose a folder other than \"/\"; use a restore in place to write to the \
             original paths."
                .into(),
        ));
    }
    Ok(normalized)
}

/// An early hint from what the agent reported. The agent checks its own roots
/// again when the job runs; unknown or unreadable facts let the request through.
fn check_agent_roots(
    req: &RestoreRequest,
    destination: &str,
    hostname: &str,
    facts_json: Option<&str>,
) -> Result<(), ApiError> {
    let Some(restore) = facts_json
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .and_then(|facts| facts.get("restore").cloned())
    else {
        return Ok(());
    };
    if req.in_place {
        if restore["allow_in_place"] == serde_json::Value::Bool(false) {
            return Err(ApiError::BadRequest(
                "restore_in_place_disabled",
                format!(
                    "{hostname} does not allow restores in place. Set restore.allow_in_place: \
                     true in the agent's agent.yaml, or install with install.sh \
                     --allow-in-place-restore."
                ),
            ));
        }
        return Ok(());
    }
    let Some(roots) = restore["roots"].as_array() else {
        return Ok(());
    };
    let roots: Vec<&str> = roots.iter().filter_map(|root| root.as_str()).collect();
    let target = std::path::Path::new(destination);
    if roots.iter().any(|root| target.starts_with(root)) {
        return Ok(());
    }
    let listed = if roots.is_empty() {
        "none".to_string()
    } else {
        roots.join(", ")
    };
    Err(ApiError::BadRequest(
        "restore_outside_roots",
        format!(
            "{destination} is outside the folders {hostname} restores into: {listed}. Add the \
             folder to restore.allowed_restore_roots in the agent's agent.yaml."
        ),
    ))
}

pub(crate) const ROOTS_OUTDATED: &str =
    "Update the agent on this host to restore to it with an API token.";

/// Whether the host can take a token's restore. An agent that has not declared
/// `restore_roots_v1` writes wherever a job says. An unknown host passes and is
/// left to `enqueue_restore` to report.
pub(crate) async fn agent_declares_roots(state: &AppState, host_id: &str) -> sqlx::Result<bool> {
    let Some(row) = sqlx::query("SELECT capabilities_json FROM agents WHERE id = $1")
        .bind(host_id)
        .fetch_optional(&state.db.pool)
        .await?
    else {
        return Ok(true);
    };
    let capabilities: Vec<String> = row
        .try_get::<Option<String>, _>("capabilities_json")?
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();
    Ok(capabilities.iter().any(|c| c == CAP_RESTORE_ROOTS))
}

/// An API token must not queue a restore on an agent that does not declare
/// `restore_roots_v1`.
pub(crate) async fn require_roots_capability(
    state: &AppState,
    host_id: &str,
) -> Result<(), ApiError> {
    if agent_declares_roots(state, host_id).await? {
        return Ok(());
    }
    Err(ApiError::BadRequest(
        "restore_agent_outdated",
        ROOTS_OUTDATED.into(),
    ))
}

/// Whether the restore could replace an existing file with the whole image;
/// only that needs more than `restore:host`.
fn replaces_a_disk_image(source_type: &str, req: &RestoreRequest) -> bool {
    source_type == "image"
        && req.items == [b"/".to_vec()]
        && matches!(
            req.conflict,
            ConflictPolicy::OverwriteChanged | ConflictPolicy::Unspecified
        )
}

pub async fn enqueue_restore(
    state: &AppState,
    req: RestoreRequest,
    started_by: &str,
) -> Result<String, ApiError> {
    enqueue_restore_checked(state, req, started_by, || Ok(())).await
}

/// [`enqueue_restore`] that calls `on_image_replace` when the restore could
/// replace an existing file with a whole disk image, decided from the same
/// snapshot row the restore is queued from.
pub async fn enqueue_restore_checked(
    state: &AppState,
    req: RestoreRequest,
    started_by: &str,
    on_image_replace: impl FnOnce() -> Result<(), ApiError>,
) -> Result<String, ApiError> {
    let destination = check_destination(&req)?;
    if req.items.is_empty() {
        return Err(invalid("Select at least one file or folder."));
    }
    if req.items.len() > MAX_ITEMS {
        return Err(invalid("Too many items are selected."));
    }
    Selection::from_paths(req.items.iter().cloned())
        .map_err(|e| ApiError::Validation(e.to_string()))?;

    let snapshot = sqlx::query(crate::browse::SNAPSHOT_HOST)
        .bind(&req.snapshot_id)
        .fetch_optional(&state.db.pool)
        .await?
        .ok_or_else(|| ApiError::NotFound("The snapshot was not found.".into()))?;
    let source_type: String = snapshot.try_get("source_type")?;
    let container = source_type == "proxmox-ct";
    let mut image = None;
    if source_type == "image" && req.in_place {
        return Err(invalid(
            "A disk image cannot be restored in place; restore it into a folder.",
        ));
    }
    if container && req.in_place {
        return Err(invalid(
            "A container snapshot cannot be restored in place; restore it as a container or \
             into a folder.",
        ));
    }
    if replaces_a_disk_image(&source_type, &req) {
        on_image_replace()?;
    }
    if source_type == "image" && req.items == [b"/".to_vec()] {
        image = Some(signed_image(state, &req.snapshot_id).await?);
    } else if source_type != "files" && source_type != "image" && !container {
        return Err(ApiError::Validation(format!(
            "This snapshot is a {source_type} backup and cannot be restored as files."
        )));
    }
    if snapshot.try_get::<Option<i64>, _>("verified_at")?.is_none() {
        if req.in_place {
            return Err(ApiError::BadRequest(
                "restore_unverified_in_place",
                "Nothing signed this snapshot, so it may not come from the host it names. \
                 Restore it into another folder or download it."
                    .into(),
            ));
        }
        if !req.accept_unverified {
            return Err(ApiError::BadRequest(
                "restore_unverified_not_confirmed",
                "Nothing signed this snapshot. Confirm the restore to restore it into \
                 another folder."
                    .into(),
            ));
        }
    }
    let target = sqlx::query("SELECT status, hostname, facts_json FROM agents WHERE id = $1")
        .bind(&req.target_host_id)
        .fetch_optional(&state.db.pool)
        .await?
        .ok_or_else(|| ApiError::NotFound("The host was not found.".into()))?;
    if target.try_get::<String, _>("status")? == "revoked" {
        return Err(invalid("The host was revoked and cannot restore."));
    }
    check_agent_roots(
        &req,
        &destination,
        &target.try_get::<String, _>("hostname")?,
        target
            .try_get::<Option<String>, _>("facts_json")?
            .as_deref(),
    )?;
    let source_repository_id: String = snapshot.try_get("repository_id")?;
    let snapshot_id = parse_hex32(&req.snapshot_id)
        .ok_or_else(|| ApiError::NotFound("The snapshot was not found.".into()))?;
    let mut source = reader(state, &source_repository_id)
        .await
        .map_err(read_error)?;
    match &image {
        Some((key, name)) => check_image(&mut source, &snapshot_id, key, name).await?,
        None => require_items_exist(&mut source, &snapshot_id, &req.items).await?,
    }
    let image = image.map(|(_, name)| name);
    let ct_idmap = if container && req.restore_owners {
        let manifest = source.snapshot(&snapshot_id).await.map_err(read_error)?;
        let meta = ct::check_manifest(&manifest)
            .map_err(|error| ApiError::Validation(error.to_string()))?;
        owner_shift(meta.idmap.as_ref())?
    } else {
        String::new()
    };

    let record = RestoreRecord {
        snapshot_id: req.snapshot_id.clone(),
        source_repository_id,
        source_host_id: snapshot.try_get("host_id")?,
        source_hostname: snapshot.try_get("hostname")?,
        items_b64: req
            .items
            .iter()
            .map(|item| data_encoding::BASE64URL_NOPAD.encode(item))
            .collect(),
        destination: destination.clone(),
        in_place: req.in_place,
        conflict: conflict_name(req.conflict).to_string(),
        restore_owners: req.restore_owners,
        started_by: started_by.to_string(),
        image,
        guest: None,
    };
    let spec = RestoreSpec {
        snapshot_id: req.snapshot_id,
        source_repo_id: Vec::new(),
        k_id: Vec::new(),
        destination,
        in_place: req.in_place,
        conflict: req.conflict as i32,
        restore_owners: req.restore_owners,
        target: None,
        ct_idmap,
        guest_snapshot: container,
    };
    insert_restore_job(state, &req.target_host_id, &spec, &record).await
}

/// The id map the agent shifts the owners of a container's files by. A map
/// that gives any id of the container to root of the node is refused: the
/// agent would not apply it, and the files would silently keep the ids of the
/// container's view.
fn owner_shift(map: Option<&IdMap>) -> Result<String, ApiError> {
    let Some(map) = map else {
        return Ok(String::new());
    };
    if map.ranges().iter().any(|range| range.host == 0) {
        return Err(ApiError::Unprocessable(
            "ct_idmap_unsupported",
            "The id map of this container gives an id to root of the node, so its owners \
             cannot be restored. Restore it without owners."
                .into(),
        ));
    }
    map.canonical()
        .map_err(|e| ApiError::Unprocessable("ct_idmap_unsupported", e.to_string()))
}

pub(crate) async fn insert_restore_job(
    state: &AppState,
    host_id: &str,
    spec: &RestoreSpec,
    record: &RestoreRecord,
) -> Result<String, ApiError> {
    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO jobs (id, host_id, source, state, spec, queued_at, kind, restore_json) \
         VALUES ($1, $2, 'manual', 'queued', $3, $4, 'restore', $5)",
    )
    .bind(&id)
    .bind(host_id)
    .bind(spec.encode_to_vec())
    .bind(now_ms())
    .bind(serde_json::to_string(record).map_err(anyhow::Error::from)?)
    .execute(&state.db.pool)
    .await?;
    Ok(id)
}

/// The key the catalog row was signed with, and the disk it names.
async fn signed_image(
    state: &AppState,
    snapshot_id: &str,
) -> Result<(SourceKey, String), ApiError> {
    let row = sqlx::query("SELECT source_ref FROM snapshots WHERE id = $1")
        .bind(snapshot_id)
        .fetch_one(&state.db.pool)
        .await?;
    let key = SourceKey::parse(&row.try_get::<String, _>("source_ref")?)
        .ok()
        .filter(|key| key.kind() == SourceType::Image);
    let name = key
        .as_ref()
        .and_then(|key| source::label(key, &Meta::new()));
    match (key, name) {
        (Some(key), Some(name)) => Ok((key, name)),
        _ => Err(invalid("The snapshot does not name a disk image.")),
    }
}

/// Refuses an image the planner would not read, before the agent gets a job.
async fn check_image(
    reader: &mut Reader<Store>,
    snapshot: &BlobId,
    key: &SourceKey,
    name: &str,
) -> Result<(), ApiError> {
    let manifest = reader.snapshot(snapshot).await.map_err(read_error)?;
    let (claimed, meta) = source::decode(manifest.source.as_ref())
        .map_err(|error| ApiError::Validation(error.to_string()))?;
    if claimed.as_str() != key.as_str() {
        return Err(invalid(
            "The snapshot manifest does not match its signature.",
        ));
    }
    ImageMeta::from_meta(&meta, name).map_err(|error| ApiError::Validation(error.to_string()))?;
    // the layout of a 16 TiB image holds hundreds of MB of ids while it is checked
    let _permit = IMAGE_ENQUEUE
        .acquire()
        .await
        .map_err(|error| ApiError::Internal(error.into()))?;
    let image = load_image(reader, snapshot, name)
        .await
        .map_err(read_error)?;
    let head = image.head().map_err(read_error)?;
    check_image_entry(&head, MAX_IMAGE_ENTRY).map_err(|_| {
        invalid(
            "This disk image has too many holes to restore through the server; \
             restore it with lode restore.",
        )
    })
}

#[cfg(test)]
mod tests {
    use lodestore_format::ct::{IdKind, IdRange};

    use super::*;

    #[test]
    fn owner_shift_never_maps_to_root_of_the_node() {
        let range = |kind, host| IdRange {
            kind,
            ct: 0,
            host,
            count: 65536,
        };
        assert_eq!(
            owner_shift(Some(&IdMap::default_unprivileged())).unwrap(),
            "u:0:100000:65536,g:0:100000:65536"
        );
        let to_root =
            IdMap::new(vec![range(IdKind::User, 100000), range(IdKind::Group, 0)]).unwrap();
        assert!(matches!(
            owner_shift(Some(&to_root)),
            Err(ApiError::Unprocessable("ct_idmap_unsupported", _))
        ));
        assert_eq!(owner_shift(None).unwrap(), "");
    }
}
