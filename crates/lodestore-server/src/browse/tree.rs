use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use data_encoding::BASE64URL_NOPAD;
use lodestore_format::Error as FormatError;
use lodestore_format::crypto::BlobId;
use lodestore_format::index::{hex32, parse_hex32};
use lodestore_format::pb;
use lodestore_format::reader::{DEFAULT_TREE_BUDGET, Reader, ReaderCache};
use lodestore_format::selection::Selection;
use lodestore_format::store::{BlobError, Store};
use lodestore_format::tree::{PlanStep, SelectionWalk};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::tokens::{Scoped, SnapshotsRead};
use crate::catalog::{RepositoryUnavailable, open_repository};
use crate::error::{ApiError, ErrorBody};
use crate::state::AppState;

const ENTRIES_PAGE: usize = 200;
const SEARCH_LIMIT: u64 = 500;
const SEARCH_QUERY_CHARS: usize = 255;
const SEARCH_BACKLOG: usize = 64;
const SELECTION_ITEMS: usize = 10_000;

#[derive(Clone, Default)]
pub struct RepoReaders {
    caches: Arc<Mutex<HashMap<String, Arc<ReaderCache>>>>,
}

impl RepoReaders {
    pub fn cache(&self, repository_id: &str) -> Arc<ReaderCache> {
        let mut caches = self.caches.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(
            caches
                .entry(repository_id.to_string())
                .or_insert_with(|| ReaderCache::new(DEFAULT_TREE_BUDGET)),
        )
    }

    /// Forgets what was cached for the repository; readers in use keep theirs.
    pub fn invalidate(&self, repository_id: &str) {
        self.caches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(repository_id);
    }
}

pub async fn reader(state: &AppState, repository_id: &str) -> anyhow::Result<Reader<Store>> {
    let open = open_repository(state, repository_id).await?;
    let cache = state.readers.cache(repository_id);
    Ok(Reader::open_shared(
        open.store,
        open.keys,
        &open.config,
        cache,
    )?)
}

pub async fn snapshot_repository(
    state: &AppState,
    snapshot_id: &str,
) -> Result<(String, BlobId), ApiError> {
    let unknown = || ApiError::NotFound("snapshot not found".into());
    let id = parse_hex32(snapshot_id).ok_or_else(unknown)?;
    let row = sqlx::query("SELECT repository_id FROM snapshots WHERE id = $1")
        .bind(hex32(&id))
        .fetch_optional(&state.db.pool)
        .await?
        .ok_or_else(unknown)?;
    Ok((row.try_get("repository_id")?, id))
}

pub fn read_error(error: impl Into<anyhow::Error>) -> ApiError {
    let error = error.into();
    if error.downcast_ref::<RepositoryUnavailable>().is_some() {
        return ApiError::Unavailable(error.to_string());
    }
    match error.downcast_ref::<FormatError>() {
        Some(FormatError::Store(BlobError::NotFound)) => {
            ApiError::NotFound("the snapshot is no longer in storage".into())
        }
        Some(FormatError::InvalidInput(message)) => ApiError::NotFound(message.clone()),
        Some(FormatError::WalkLimit { .. }) => {
            ApiError::BadRequest("tree_too_large", error.to_string())
        }
        Some(_) => ApiError::Backend(format!("the repository storage failed: {error}")),
        None if error.downcast_ref::<BlobError>().is_some() => {
            ApiError::Backend(format!("the repository storage failed: {error}"))
        }
        None => ApiError::Internal(error),
    }
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn b64(bytes: &[u8]) -> String {
    BASE64URL_NOPAD.encode(bytes)
}

pub(crate) fn decode_b64(text: &str) -> Result<Vec<u8>, ApiError> {
    BASE64URL_NOPAD
        .decode(text.as_bytes())
        .map_err(|_| ApiError::Validation("The path is not valid base64url.".into()))
}

fn decode_path(text: Option<&str>) -> Result<Vec<u8>, ApiError> {
    let raw = match text.filter(|t| !t.is_empty()) {
        Some(text) => decode_b64(text)?,
        None => Vec::new(),
    };
    let mut path = Vec::with_capacity(raw.len());
    for part in raw.split(|&b| b == b'/').filter(|c| !c.is_empty()) {
        if part == b"." || part == b".." || part.contains(&0) {
            return Err(ApiError::Validation("The path is not valid.".into()));
        }
        path.push(b'/');
        path.extend_from_slice(part);
    }
    Ok(path)
}

#[derive(Serialize, ToSchema, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Dir,
    File,
    Symlink,
    Other,
}

#[derive(Serialize, ToSchema)]
pub struct EntryItem {
    pub name: String,
    /// Exact name bytes, base64url without padding.
    pub name_b64: String,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime_ms: i64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub link_target: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct EntryPage {
    pub path: String,
    /// Exact path bytes, base64url without padding.
    pub path_b64: String,
    pub entries: Vec<EntryItem>,
    pub total: u64,
    pub offset: u64,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct EntriesQuery {
    /// Directory path as base64url bytes; empty is the top of the snapshot.
    pub path: Option<String>,
    /// Index of the first entry, in steps of 200.
    pub offset: Option<u64>,
}

pub(crate) fn kind_of(node: &pb::Node) -> EntryKind {
    match pb::NodeType::try_from(node.r#type) {
        Ok(pb::NodeType::Dir) => EntryKind::Dir,
        Ok(pb::NodeType::File) => EntryKind::File,
        Ok(pb::NodeType::Symlink) => EntryKind::Symlink,
        _ => EntryKind::Other,
    }
}

pub(crate) fn mtime_ms(node: &pb::Node) -> i64 {
    node.mtime.as_ref().map_or(0, |t| {
        t.seconds
            .saturating_mul(1000)
            .saturating_add(i64::from(t.nanos / 1_000_000))
    })
}

pub fn entry_item(node: &pb::Node) -> EntryItem {
    EntryItem {
        name: lossy(&node.name),
        name_b64: b64(&node.name),
        kind: kind_of(node),
        size: node.size,
        mtime_ms: mtime_ms(node),
        mode: node.mode,
        uid: node.uid,
        gid: node.gid,
        link_target: (!node.link_target.is_empty()).then(|| lossy(&node.link_target)),
    }
}

#[utoipa::path(
    get,
    tag = "Restore",
    summary = "List a directory of a snapshot",
    security(("session" = []), ("bearer" = ["snapshots:read"])),
    path = "/api/v1/snapshots/{id}/entries",
    params(("id" = String, Path, description = "Snapshot id"), EntriesQuery),
    responses(
        (status = 200, description = "One page of the directory: folders first, then names without regard to case", body = EntryPage),
        (status = 404, description = "Unknown snapshot, gone from storage, or no such directory", body = ErrorBody),
        (status = 422, description = "The path is not valid", body = ErrorBody),
        (status = 400, description = "The directory tree is larger than one walk may visit (tree_too_large)", body = ErrorBody),
        (status = 502, description = "The repository storage is unavailable", body = ErrorBody),
        (status = 503, description = "The repository does not match what this server recorded (repository_unavailable)", body = ErrorBody),
    )
)]
async fn list_snapshot_entries(
    State(state): State<AppState>,
    _auth: Scoped<SnapshotsRead>,
    Path(id): Path<String>,
    Query(query): Query<EntriesQuery>,
) -> Result<Json<EntryPage>, ApiError> {
    let path = decode_path(query.path.as_deref())?;
    let (repository_id, snapshot) = snapshot_repository(&state, &id).await?;
    let mut reader = reader(&state, &repository_id).await.map_err(read_error)?;
    let mut nodes = reader
        .list_dir(&snapshot, &path)
        .await
        .map_err(read_error)?;
    nodes.sort_by_cached_key(|n| {
        (
            n.r#type != pb::NodeType::Dir as i32,
            lossy(&n.name).to_lowercase(),
            n.name.clone(),
        )
    });
    let total = nodes.len();
    let offset = usize::try_from(query.offset.unwrap_or(0)).unwrap_or(usize::MAX);
    let entries = nodes
        .iter()
        .skip(offset)
        .take(ENTRIES_PAGE)
        .map(entry_item)
        .collect();
    Ok(Json(EntryPage {
        path: lossy(&path),
        path_b64: b64(&path),
        entries,
        total: total as u64,
        offset: offset.min(total) as u64,
    }))
}

#[derive(Serialize, ToSchema)]
pub struct SearchMatch {
    #[serde(flatten)]
    pub entry: EntryItem,
    /// Directory holding the match.
    pub parent: String,
    /// Exact bytes of the directory path, base64url without padding.
    pub parent_b64: String,
}

#[derive(Serialize, ToSchema)]
pub struct SearchDone {
    pub count: u64,
    /// More than 500 names match; the rest was not sent.
    pub truncated: bool,
}

/// One line of the search stream.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SearchLine {
    Match(SearchMatch),
    Done(SearchDone),
    Error(String),
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SearchQuery {
    /// Directory to search below, as base64url bytes; empty is the whole snapshot.
    pub path: Option<String>,
    /// Text to find in names, ignoring case; 1 to 255 characters.
    pub q: Option<String>,
}

enum Stop {
    Closed,
    Failed(String),
}

type Lines = mpsc::Sender<Result<Bytes, std::convert::Infallible>>;

async fn send(tx: &Lines, line: &SearchLine) -> bool {
    let mut bytes = serde_json::to_vec(line).unwrap_or_default();
    bytes.push(b'\n');
    tx.send(Ok(Bytes::from(bytes))).await.is_ok()
}

async fn search_walk(
    reader: &mut Reader<Store>,
    snapshot: &BlobId,
    base: &[u8],
    needle: &str,
    tx: &Lines,
) -> Result<SearchDone, Stop> {
    let failed = |e: FormatError| Stop::Failed(e.to_string());
    let selection = Selection::from_paths([base.to_vec()]).map_err(failed)?;
    let mut walk = SelectionWalk::start(reader, snapshot, &selection)
        .await
        .map_err(failed)?;
    let mut dirs: Vec<Vec<u8>> = Vec::new();
    let mut count = 0u64;
    loop {
        if tx.is_closed() {
            return Err(Stop::Closed);
        }
        let Some(step) = walk.next(reader).await.map_err(failed)? else {
            return Ok(SearchDone {
                count,
                truncated: false,
            });
        };
        let (node, ancestor, enters) = match step {
            PlanStep::Leave => {
                dirs.pop();
                continue;
            }
            PlanStep::Enter { node, ancestor } => (node, ancestor, true),
            PlanStep::Entry(node) => (node, false, false),
        };
        let parent = dirs.last().cloned().unwrap_or_default();
        let mut path = parent.clone();
        path.push(b'/');
        path.extend_from_slice(&node.name);
        if enters {
            dirs.push(path.clone());
        }
        if ancestor || path == base || !lossy(&node.name).to_lowercase().contains(needle) {
            continue;
        }
        if count == SEARCH_LIMIT {
            return Ok(SearchDone {
                count,
                truncated: true,
            });
        }
        count += 1;
        let line = SearchLine::Match(SearchMatch {
            entry: entry_item(&node),
            parent: lossy(&parent),
            parent_b64: b64(&parent),
        });
        if !send(tx, &line).await {
            return Err(Stop::Closed);
        }
    }
}

#[utoipa::path(
    get,
    tag = "Restore",
    summary = "Find names below a directory of a snapshot",
    description = "Streams one JSON object per line as the walk goes: `{\"match\": ...}` for each name that contains the text, then `{\"done\": {\"count\", \"truncated\"}}`, or `{\"error\": \"...\"}` if the walk fails midway.",
    security(("session" = []), ("bearer" = ["snapshots:read"])),
    path = "/api/v1/snapshots/{id}/search",
    params(("id" = String, Path, description = "Snapshot id"), SearchQuery),
    responses(
        (status = 200, description = "Newline-delimited JSON, at most 500 matches", content_type = "application/x-ndjson", body = SearchLine),
        (status = 404, description = "Unknown snapshot, gone from storage, or no such directory", body = ErrorBody),
        (status = 422, description = "The path or the text is not valid", body = ErrorBody),
        (status = 400, description = "The directory tree is larger than one walk may visit (tree_too_large)", body = ErrorBody),
        (status = 502, description = "The repository storage is unavailable", body = ErrorBody),
        (status = 503, description = "The repository does not match what this server recorded (repository_unavailable)", body = ErrorBody),
    )
)]
async fn search_snapshot(
    State(state): State<AppState>,
    _auth: Scoped<SnapshotsRead>,
    Path(id): Path<String>,
    Query(query): Query<SearchQuery>,
) -> Result<Response, ApiError> {
    let text = query.q.unwrap_or_default();
    if text.is_empty() || text.chars().count() > SEARCH_QUERY_CHARS {
        return Err(ApiError::Validation(format!(
            "The text must be 1 to {SEARCH_QUERY_CHARS} characters."
        )));
    }
    let base = decode_path(query.path.as_deref())?;
    let (repository_id, snapshot) = snapshot_repository(&state, &id).await?;
    let mut reader = reader(&state, &repository_id).await.map_err(read_error)?;
    reader
        .list_dir(&snapshot, &base)
        .await
        .map_err(read_error)?;

    let (tx, rx) = mpsc::channel(SEARCH_BACKLOG);
    let needle = text.to_lowercase();
    tokio::spawn(async move {
        let last = match search_walk(&mut reader, &snapshot, &base, &needle, &tx).await {
            Ok(done) => SearchLine::Done(done),
            Err(Stop::Closed) => return,
            Err(Stop::Failed(message)) => SearchLine::Error(message),
        };
        send(&tx, &last).await;
    });
    Ok((
        [(CONTENT_TYPE, "application/x-ndjson")],
        Body::from_stream(ReceiverStream::new(rx)),
    )
        .into_response())
}

#[derive(Deserialize, ToSchema)]
pub struct SelectionBody {
    /// Absolute snapshot paths as base64url bytes; a folder brings everything below it.
    pub items: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct SelectionSummary {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
}

pub fn decode_items(items: &[String]) -> Result<Selection, ApiError> {
    if items.is_empty() || items.len() > SELECTION_ITEMS {
        return Err(ApiError::Validation(format!(
            "Select 1 to {SELECTION_ITEMS} items."
        )));
    }
    Selection::from_paths(decode_paths(items)?).map_err(|e| ApiError::Validation(e.to_string()))
}

pub fn decode_paths(items: &[String]) -> Result<Vec<Vec<u8>>, ApiError> {
    items.iter().map(|item| decode_b64(item)).collect()
}

/// Refuses a selection with a path the snapshot does not hold, with 404.
/// `paths` are absolute byte paths that already passed validation.
pub async fn require_items_exist(
    reader: &mut Reader<Store>,
    snapshot: &BlobId,
    paths: &[Vec<u8>],
) -> Result<(), ApiError> {
    let mut wanted: HashMap<Vec<u8>, Vec<Vec<u8>>> = HashMap::new();
    for path in paths {
        let parts: Vec<&[u8]> = path
            .split(|&b| b == b'/')
            .filter(|p| !p.is_empty())
            .collect();
        if let Some((name, parents)) = parts.split_last() {
            let parent = parents.iter().flat_map(|p| [b"/".as_slice(), p]).flatten();
            wanted
                .entry(parent.copied().collect())
                .or_default()
                .push(name.to_vec());
        }
    }
    for (parent, names) in wanted {
        let nodes = reader
            .list_dir(snapshot, &parent)
            .await
            .map_err(read_error)?;
        for name in names {
            if !nodes.iter().any(|n| n.name == name) {
                return Err(ApiError::NotFound(format!(
                    "{} is not in the snapshot",
                    lossy(&[parent.as_slice(), b"/", &name].concat())
                )));
            }
        }
    }
    Ok(())
}

#[utoipa::path(
    post,
    tag = "Restore",
    summary = "Count what a selection of a snapshot holds",
    security(("session" = [], "csrf" = []), ("bearer" = ["snapshots:read"])),
    path = "/api/v1/snapshots/{id}/summary",
    params(("id" = String, Path, description = "Snapshot id")),
    request_body = SelectionBody,
    responses(
        (status = 200, description = "Files, folders and bytes the selection would restore", body = SelectionSummary),
        (status = 404, description = "Unknown snapshot, gone from storage, or a selected path is missing", body = ErrorBody),
        (status = 422, description = "Empty selection or a path that is not valid", body = ErrorBody),
        (status = 400, description = "The directory tree is larger than one walk may visit (tree_too_large)", body = ErrorBody),
        (status = 502, description = "The repository storage is unavailable", body = ErrorBody),
        (status = 503, description = "The repository does not match what this server recorded (repository_unavailable)", body = ErrorBody),
    )
)]
async fn summarize_selection(
    State(state): State<AppState>,
    _auth: Scoped<SnapshotsRead>,
    Path(id): Path<String>,
    Json(body): Json<SelectionBody>,
) -> Result<Json<SelectionSummary>, ApiError> {
    let selection = decode_items(&body.items)?;
    let (repository_id, snapshot) = snapshot_repository(&state, &id).await?;
    let mut reader = reader(&state, &repository_id).await.map_err(read_error)?;
    require_items_exist(&mut reader, &snapshot, &decode_paths(&body.items)?).await?;
    let totals = reader
        .selection_totals(&snapshot, &selection)
        .await
        .map_err(read_error)?;
    Ok(Json(SelectionSummary {
        files: totals.files,
        dirs: totals.dirs,
        bytes: totals.bytes,
    }))
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_snapshot_entries))
        .routes(routes!(search_snapshot))
        .routes(routes!(summarize_selection))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_limit_is_a_clear_bad_request() {
        let error = read_error(FormatError::WalkLimit {
            what: "entries",
            limit: 7,
        });
        assert!(
            matches!(&error, ApiError::BadRequest("tree_too_large", message) if message.contains('7')),
            "unexpected mapping"
        );
    }
}
