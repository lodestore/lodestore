use std::collections::HashSet;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use data_encoding::{BASE64URL_NOPAD, HEXLOWER};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::pattern::Pattern;
use super::{SNAPSHOT_CHIPS, Session, SessionState, Version, VersionKey, walk};
use crate::auth::tokens::{Scoped, SnapshotsRead};
use crate::browse::AGENTLESS;
use crate::browse::tree::EntryKind;
use crate::db::now_ms;
use crate::error::{ApiError, ErrorBody};
use crate::hosts::labels::Label;
use crate::maintenance::gc::api::FieldError;
use crate::state::AppState;

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct FindRequest {
    /// A name fragment (`nginx.conf`), a glob for names (`*.conf`) or a glob for the whole
    /// path (`/etc/nginx/**`); case does not matter; 1 to 255 characters.
    pub pattern: String,
    /// Host keys (an agent id or `agentless:{hostname}`) to search, at most 500. Together with
    /// `labels` this is a union; all hosts when both are empty.
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Also search hosts whose agent carries all of these labels, as `key=value`; at most 20.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Only snapshots received at or after this time, in milliseconds.
    pub since: Option<i64>,
    /// Only snapshots received at or before this time, in milliseconds.
    pub until: Option<i64>,
}

#[derive(Serialize, ToSchema)]
pub struct FindStarted {
    pub id: String,
}

#[derive(Serialize, ToSchema, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum FindState {
    Running,
    Done,
    /// Stopped by a limit; what was found is kept.
    Incomplete,
    Failed,
    Cancelled,
}

#[derive(Serialize, ToSchema, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum FindStop {
    /// The search found more files than the limit allows.
    Limit,
    /// The search ran out of time.
    Timeout,
}

#[derive(Serialize, ToSchema)]
pub struct FindGone {
    /// The newest snapshot of the host that still holds the file.
    pub last_ms: i64,
    /// The next snapshot of the host, which no longer has it.
    pub next_ms: i64,
}

#[derive(Serialize, ToSchema)]
pub struct FindSnapshotRef {
    pub id: String,
    /// When the server received the snapshot, in milliseconds.
    pub time_ms: i64,
}

#[derive(Serialize, ToSchema)]
pub struct FindVersion {
    /// Identifies the content within one repository (16 bytes, hex); empty for an empty file or
    /// a link.
    pub content_id: Option<String>,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime_ms: i64,
    /// When the server received the first snapshot holding this version, in milliseconds.
    pub first_ms: i64,
    /// When the server received the last snapshot holding this version, in milliseconds.
    pub last_ms: i64,
    /// Who signed the provenance of a snapshot holding this version: `server` or `owner`. Null when none is signed; such a version is listed after the signed ones.
    pub verified_by: Option<String>,
    pub snapshot_count: u64,
    /// The newest snapshots holding this version, newest first.
    pub snapshots: Vec<FindSnapshotRef>,
}

#[derive(Serialize, ToSchema)]
pub struct FindGroup {
    /// Position in the order groups were found; it does not change.
    pub index: u64,
    /// An agent id or `agentless:{hostname}`.
    pub host_key: String,
    pub host_id: Option<String>,
    pub hostname: String,
    pub agent: bool,
    pub labels: Vec<Label>,
    /// Exact path bytes, base64url without padding.
    pub path_b64: String,
    pub path: String,
    /// When the server received the newest snapshot holding the file, in milliseconds.
    pub last_seen_ms: i64,
    /// Set when a newer snapshot of the host exists without this file; known once the search is over.
    pub gone: Option<FindGone>,
    pub versions: Vec<FindVersion>,
    /// The revision of the session at the last change of this group.
    pub revision: u64,
}

#[derive(Serialize, ToSchema)]
pub struct FindSession {
    pub id: String,
    pub pattern: String,
    pub state: FindState,
    pub stop_reason: Option<FindStop>,
    /// The most files (groups) a search keeps.
    pub limit: u64,
    pub scanned: u64,
    pub total: u64,
    pub skipped: u64,
    /// Hosts whose snapshots the search reads.
    pub searched_hosts: u64,
    /// Hosts with at least one match.
    pub hosts: u64,
    pub files: u64,
    pub versions: u64,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub error: Option<String>,
    pub revision: u64,
    pub groups: Vec<FindGroup>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct FindQuery {
    /// Only the groups that changed after this revision.
    pub after: Option<u64>,
}

#[derive(Serialize, ToSchema)]
pub struct FindFieldErrors {
    pub errors: Vec<FieldError>,
}

pub enum FindFailure {
    Api(ApiError),
    Field(&'static str, String),
}

impl<E: Into<ApiError>> From<E> for FindFailure {
    fn from(error: E) -> Self {
        FindFailure::Api(error.into())
    }
}

impl IntoResponse for FindFailure {
    fn into_response(self) -> Response {
        match self {
            FindFailure::Api(error) => error.into_response(),
            FindFailure::Field(path, message) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(FindFieldErrors {
                    errors: vec![FieldError {
                        path: path.into(),
                        message,
                    }],
                }),
            )
                .into_response(),
        }
    }
}

fn b64(bytes: &[u8]) -> String {
    BASE64URL_NOPAD.encode(bytes)
}

fn version_dto(version: &Version, limit: usize) -> FindVersion {
    let mut snapshots: Vec<&(String, i64)> = version.snapshots.iter().collect();
    snapshots.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));
    FindVersion {
        content_id: match version.key {
            VersionKey::Content(id) => Some(HEXLOWER.encode(&id)),
            VersionKey::Meta { .. } => None,
        },
        kind: version.kind,
        size: version.size,
        mtime_ms: version.mtime_ms,
        first_ms: version.first_ms,
        last_ms: version.last_ms,
        verified_by: version.verified_by.clone(),
        snapshot_count: version.snapshots.len() as u64,
        snapshots: snapshots
            .into_iter()
            .take(limit)
            .map(|(id, time_ms)| FindSnapshotRef {
                id: id.clone(),
                time_ms: *time_ms,
            })
            .collect(),
    }
}

fn group_dto(index: usize, s: &SessionState, limit: usize) -> FindGroup {
    let group = &s.groups[index];
    let mut versions: Vec<&Version> = group.versions.iter().collect();
    versions.sort_by_key(|v| std::cmp::Reverse((v.verified_by.is_some(), v.last_ms)));
    FindGroup {
        index: index as u64,
        host_key: group.host_key.clone(),
        host_id: group.host_id.clone(),
        hostname: group.hostname.clone(),
        agent: group.host_id.is_some(),
        labels: group.labels.clone(),
        path_b64: b64(&group.path),
        path: String::from_utf8_lossy(&group.path).into_owned(),
        last_seen_ms: group.last_seen_ms,
        gone: s.next_scanned_after(group).map(|next_ms| FindGone {
            last_ms: group.last_seen_ms,
            next_ms,
        }),
        versions: versions
            .into_iter()
            .map(|v| version_dto(v, limit))
            .collect(),
        revision: group.revision,
    }
}

fn session_dto(session: &Session, s: &SessionState, limit: u64, after: u64) -> FindSession {
    FindSession {
        id: session.id.clone(),
        pattern: session.query.pattern.clone(),
        state: s.state,
        stop_reason: s.stop_reason,
        limit,
        scanned: s.scanned,
        total: s.total,
        skipped: s.skipped,
        searched_hosts: s.searched_hosts,
        hosts: s
            .groups
            .iter()
            .map(|g| g.host_key.as_str())
            .collect::<HashSet<_>>()
            .len() as u64,
        files: s.groups.len() as u64,
        versions: s.groups.iter().map(|g| g.versions.len() as u64).sum(),
        started_at: session.started_at,
        finished_at: s.finished_at,
        error: s.error.clone(),
        revision: s.revision,
        groups: s
            .groups
            .iter()
            .enumerate()
            .filter(|(_, g)| g.revision > after)
            .map(|(i, _)| group_dto(i, s, SNAPSHOT_CHIPS))
            .collect(),
    }
}

fn session_of(state: &AppState, id: &str) -> Result<Arc<Session>, ApiError> {
    state
        .find
        .get(id)
        .ok_or_else(|| ApiError::NotFound("search not found".into()))
}

pub const MAX_HOSTS: usize = 500;
pub const MAX_LABELS: usize = 20;

async fn check_filters(state: &AppState, body: &FindRequest) -> Result<(), FindFailure> {
    if body.hosts.len() > MAX_HOSTS {
        return Err(FindFailure::Field(
            "hosts",
            format!("at most {MAX_HOSTS} hosts"),
        ));
    }
    if body.labels.len() > MAX_LABELS {
        return Err(FindFailure::Field(
            "labels",
            format!("at most {MAX_LABELS} labels"),
        ));
    }
    if let (Some(since), Some(until)) = (body.since, body.until)
        && since > until
    {
        return Err(FindFailure::Field(
            "since",
            "since must not be later than until".into(),
        ));
    }
    if walk::parse_labels(&body.labels).is_none() {
        return Err(FindFailure::Field(
            "labels",
            "a label is written as key=value".into(),
        ));
    }
    for host in &body.hosts {
        let known = match host.strip_prefix(AGENTLESS) {
            Some(hostname) => {
                sqlx::query(
                    "SELECT 1 FROM snapshots WHERE host_id IS NULL AND hostname = $1 LIMIT 1",
                )
                .bind(hostname)
                .fetch_optional(&state.db.pool)
                .await?
            }
            None => {
                sqlx::query("SELECT 1 FROM agents WHERE id = $1")
                    .bind(host)
                    .fetch_optional(&state.db.pool)
                    .await?
            }
        };
        if known.is_none() {
            return Err(FindFailure::Field("hosts", format!("unknown host {host}")));
        }
    }
    Ok(())
}

#[utoipa::path(
    post,
    tag = "Restore",
    summary = "Start a search for files in the snapshots of all hosts",
    security(("session" = [], "csrf" = []), ("bearer" = ["snapshots:read"])),
    path = "/api/v1/find",
    request_body = FindRequest,
    responses(
        (status = 202, description = "The search runs; poll it with `GET /api/v1/find/{id}`", body = FindStarted),
        (status = 401, description = "Not signed in", body = ErrorBody),
        (status = 403, description = "The token lacks `snapshots:read`, or the CSRF token is missing", body = ErrorBody),
        (status = 422, description = "The pattern or a filter is not valid", body = FindFieldErrors),
        (status = 429, description = "Too many searches are queued or running", body = ErrorBody),
    )
)]
async fn start_find(
    State(state): State<AppState>,
    _auth: Scoped<SnapshotsRead>,
    Json(mut body): Json<FindRequest>,
) -> Result<(StatusCode, Json<FindStarted>), FindFailure> {
    let matcher = Pattern::compile(&body.pattern)
        .map_err(|e| FindFailure::Field("pattern", e.to_string()))?;
    let mut seen = HashSet::new();
    body.hosts.retain(|host| seen.insert(host.clone()));
    check_filters(&state, &body).await?;
    let now = now_ms();
    state.find.sweep(now);
    let id = uuid::Uuid::now_v7().to_string();
    let session = Arc::new(Session::new(id.clone(), body, matcher, now));
    if !state.find.insert(Arc::clone(&session)) {
        return Err(ApiError::RateLimited.into());
    }
    tokio::spawn(walk::run(state, session));
    Ok((StatusCode::ACCEPTED, Json(FindStarted { id })))
}

#[utoipa::path(
    get,
    tag = "Restore",
    summary = "Read the progress and the results of a search",
    security(("session" = []), ("bearer" = ["snapshots:read"])),
    path = "/api/v1/find/{id}",
    params(("id" = String, Path, description = "Search id"), FindQuery),
    responses(
        (status = 200, description = "Progress and the groups that changed after `after`", body = FindSession),
        (status = 401, description = "Not signed in", body = ErrorBody),
        (status = 403, description = "The token lacks `snapshots:read`", body = ErrorBody),
        (status = 404, description = "Unknown search", body = ErrorBody),
    )
)]
async fn get_find(
    State(state): State<AppState>,
    _auth: Scoped<SnapshotsRead>,
    Path(id): Path<String>,
    Query(query): Query<FindQuery>,
) -> Result<Json<FindSession>, ApiError> {
    let session = session_of(&state, &id)?;
    let s = session.lock();
    Ok(Json(session_dto(
        &session,
        &s,
        state.config.find.max_matches,
        query.after.unwrap_or(0),
    )))
}

#[utoipa::path(
    get,
    tag = "Restore",
    summary = "Read one found file with every snapshot of each version",
    security(("session" = []), ("bearer" = ["snapshots:read"])),
    path = "/api/v1/find/{id}/groups/{index}",
    params(
        ("id" = String, Path, description = "Search id"),
        ("index" = u64, Path, description = "Index of the group in the search"),
    ),
    responses(
        (status = 200, description = "The group with the full list of snapshots per version", body = FindGroup),
        (status = 401, description = "Not signed in", body = ErrorBody),
        (status = 403, description = "The token lacks `snapshots:read`", body = ErrorBody),
        (status = 404, description = "Unknown search or group", body = ErrorBody),
    )
)]
async fn get_find_group(
    State(state): State<AppState>,
    _auth: Scoped<SnapshotsRead>,
    Path((id, index)): Path<(String, u64)>,
) -> Result<Json<FindGroup>, ApiError> {
    let session = session_of(&state, &id)?;
    let s = session.lock();
    let index = usize::try_from(index)
        .ok()
        .filter(|&i| i < s.groups.len())
        .ok_or_else(|| ApiError::NotFound("group not found".into()))?;
    Ok(Json(group_dto(index, &s, usize::MAX)))
}

#[utoipa::path(
    delete,
    tag = "Restore",
    summary = "Stop a search; what it found stays",
    security(("session" = [], "csrf" = []), ("bearer" = ["snapshots:read"])),
    path = "/api/v1/find/{id}",
    params(("id" = String, Path, description = "Search id")),
    responses(
        (status = 204, description = "The search is stopped, or was over already"),
        (status = 401, description = "Not signed in", body = ErrorBody),
        (status = 403, description = "The token lacks `snapshots:read`, or the CSRF token is missing", body = ErrorBody),
        (status = 404, description = "Unknown search", body = ErrorBody),
    )
)]
async fn cancel_find(
    State(state): State<AppState>,
    _auth: Scoped<SnapshotsRead>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    session_of(&state, &id)?.cancel.cancel();
    Ok(StatusCode::NO_CONTENT)
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(start_find))
        .routes(routes!(get_find, cancel_find))
        .routes(routes!(get_find_group))
}
