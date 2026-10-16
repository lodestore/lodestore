use std::io::{self, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::header::{
    CONTENT_DISPOSITION, CONTENT_SECURITY_POLICY, CONTENT_TYPE, X_FRAME_OPTIONS,
};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use lodestore_format::index::{hex32, parse_hex32};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tower_http::set_header::SetResponseHeaderLayer;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::archive::{ArchiveFormat, write_archive};
use crate::api::CSP_SAME_ORIGIN_FRAME;
use crate::auth::AuthUser;
use crate::browse::tree::{
    decode_items, decode_paths, read_error, reader, require_items_exist, snapshot_repository,
};
use crate::db::now_ms;
use crate::error::{ApiError, ErrorBody};
use crate::state::AppState;

pub const TICKET_TTL_MS: i64 = 60_000;
const CHUNK: usize = 64 * 1024;
const BACKLOG: usize = 16;
const PROGRESS_EVERY: Duration = Duration::from_secs(1);

#[derive(Deserialize, ToSchema)]
pub struct DownloadBody {
    pub snapshot_id: String,
    /// Selected paths as base64url bytes without padding.
    pub items: Vec<String>,
    pub format: ArchiveFormat,
}

#[derive(Serialize, ToSchema)]
pub struct DownloadCreated {
    pub id: String,
    /// Single-use link, valid for 60 seconds; open it as a normal page load.
    pub url: String,
    pub file_name: String,
}

pub fn format_name(format: ArchiveFormat) -> &'static str {
    match format {
        ArchiveFormat::TarGz => "tar_gz",
        ArchiveFormat::TarZst => "tar_zst",
        ArchiveFormat::Zip => "zip",
    }
}

pub fn parse_format(name: &str) -> Option<ArchiveFormat> {
    Some(match name {
        "tar_gz" => ArchiveFormat::TarGz,
        "tar_zst" => ArchiveFormat::TarZst,
        "zip" => ArchiveFormat::Zip,
        _ => return None,
    })
}

/// Each character outside `[A-Za-z0-9.-]` becomes `_`; no host gives `snapshot`.
fn safe_host(hostname: &str) -> String {
    if hostname.is_empty() {
        return "snapshot".into();
    }
    hostname
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `{host}-{first 8 of the snapshot id}.{ext}`, the name the interface shows.
fn file_name(hostname: &str, snapshot_id: &str, format: ArchiveFormat) -> String {
    let short = snapshot_id.get(..8).unwrap_or(snapshot_id);
    format!("{}-{short}.{}", safe_host(hostname), format.extension())
}

#[utoipa::path(
    post,
    tag = "Restore",
    summary = "Prepare an archive download",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/downloads",
    request_body = DownloadBody,
    responses(
        (status = 201, description = "A single-use link to the archive", body = DownloadCreated),
        (status = 404, description = "Unknown snapshot, gone from storage, or a selected path is missing", body = ErrorBody),
        (status = 422, description = "The selection is not valid", body = ErrorBody),
        (status = 502, description = "The repository storage is unavailable", body = ErrorBody),
    )
)]
async fn create_download(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<DownloadBody>,
) -> Result<(StatusCode, Json<DownloadCreated>), ApiError> {
    decode_items(&body.items)?;
    let (repository_id, snapshot) = snapshot_repository(&state, &body.snapshot_id).await?;
    let mut source = reader(&state, &repository_id).await.map_err(read_error)?;
    require_items_exist(&mut source, &snapshot, &decode_paths(&body.items)?).await?;
    let snapshot_id = hex32(&snapshot);
    let row = sqlx::query(crate::browse::SNAPSHOT_HOST)
        .bind(&snapshot_id)
        .fetch_one(&state.db.pool)
        .await?;
    let host_id: Option<String> = row.try_get("host_id")?;
    let hostname: String = row.try_get("hostname")?;
    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO downloads (id, repository_id, host_id, hostname, snapshot_id, items_json, \
         format, started_by, state, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'pending', $9)",
    )
    .bind(&id)
    .bind(&repository_id)
    .bind(host_id)
    .bind(&hostname)
    .bind(&snapshot_id)
    .bind(serde_json::to_string(&body.items).map_err(anyhow::Error::from)?)
    .bind(format_name(body.format))
    .bind(&user.username)
    .bind(now_ms())
    .execute(&state.db.pool)
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(DownloadCreated {
            url: format!("/api/v1/downloads/{id}/archive"),
            file_name: file_name(&hostname, &snapshot_id, body.format),
            id,
        }),
    ))
}

struct ChannelWriter {
    tx: mpsc::Sender<io::Result<Bytes>>,
    buf: Vec<u8>,
    sent: u64,
    closed: bool,
    report: Report,
}

/// Writes `bytes_sent` of a running download now and then, so Jobs shows
/// progress before the archive ends.
struct Report {
    state: AppState,
    id: String,
    handle: Handle,
    at: Instant,
}

impl Report {
    fn sent(&mut self, sent: u64) {
        if self.at.elapsed() < PROGRESS_EVERY {
            return;
        }
        self.at = Instant::now();
        let (pool, id) = (self.state.db.pool.clone(), self.id.clone());
        self.handle.spawn(async move {
            let updated = sqlx::query(
                "UPDATE downloads SET bytes_sent = $1 WHERE id = $2 AND state = 'running'",
            )
            .bind(i64::try_from(sent).unwrap_or(i64::MAX))
            .bind(&id)
            .execute(&pool)
            .await;
            if let Err(error) = updated {
                tracing::debug!(%error, download = %id, "could not record download progress");
            }
        });
    }
}

impl ChannelWriter {
    fn send_buf(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let chunk = Bytes::from(std::mem::replace(&mut self.buf, Vec::with_capacity(CHUNK)));
        let len = chunk.len() as u64;
        if self.tx.blocking_send(Ok(chunk)).is_err() {
            self.closed = true;
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        self.sent += len;
        self.report.sent(self.sent);
        Ok(())
    }
}

impl Write for ChannelWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.closed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        self.buf.extend_from_slice(data);
        if self.buf.len() >= CHUNK {
            self.send_buf()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

async fn finish(state: &AppState, id: &str, sent: u64, error_code: Option<&str>) {
    let (new_state, code) = match error_code {
        None => ("succeeded", None),
        Some(code) => ("interrupted", Some(code)),
    };
    let updated = sqlx::query(
        "UPDATE downloads SET state = $1, finished_at = $2, bytes_sent = $3, error_code = $4 \
         WHERE id = $5",
    )
    .bind(new_state)
    .bind(now_ms())
    .bind(i64::try_from(sent).unwrap_or(i64::MAX))
    .bind(code)
    .bind(id)
    .execute(&state.db.pool)
    .await;
    if let Err(error) = updated {
        tracing::warn!(%error, download = id, "could not record the end of a download");
    }
}

#[utoipa::path(
    get,
    tag = "Restore",
    summary = "Download the archive",
    path = "/api/v1/downloads/{id}/archive",
    params(("id" = String, Path, description = "Download id from the link")),
    responses(
        (
            status = 200,
            description = "The archive, streamed while it is built; no resume",
            content(
                (Vec<u8> = "application/gzip"),
                (Vec<u8> = "application/zstd"),
                (Vec<u8> = "application/zip")
            )
        ),
        (status = 404, description = "The link was already used or expired, or the snapshot is gone", body = ErrorBody),
        (status = 502, description = "The repository storage is unavailable", body = ErrorBody),
    )
)]
async fn download_archive(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let now = now_ms();
    let claimed = sqlx::query(
        "UPDATE downloads SET state = 'running', started_at = $1 \
         WHERE id = $2 AND state = 'pending' AND created_at > $3",
    )
    .bind(now)
    .bind(&id)
    .bind(now - TICKET_TTL_MS)
    .execute(&state.db.pool)
    .await?
    .rows_affected()
        == 1;
    if !claimed {
        return Err(ApiError::NotFound(
            "This download link was already used or expired.".into(),
        ));
    }
    match start(&state, &id).await {
        Ok(response) => Ok(response),
        Err(error) => {
            finish(&state, &id, 0, Some("read_failed")).await;
            Err(error)
        }
    }
}

async fn start(state: &AppState, id: &str) -> Result<Response, ApiError> {
    let row = sqlx::query(
        "SELECT repository_id, hostname, snapshot_id, items_json, format FROM downloads WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&state.db.pool)
    .await?;
    let repository_id: String = row.try_get("repository_id")?;
    let hostname: String = row.try_get("hostname")?;
    let format = parse_format(&row.try_get::<String, _>("format")?)
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("unknown download format")))?;
    let snapshot_id: String = row.try_get("snapshot_id")?;
    let snapshot = parse_hex32(&snapshot_id)
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("bad snapshot id")))?;
    let items: Vec<String> = serde_json::from_str(&row.try_get::<String, _>("items_json")?)
        .map_err(|e| ApiError::Internal(e.into()))?;
    let selection = decode_items(&items)?;

    let mut source = reader(state, &repository_id).await.map_err(read_error)?;
    source.list_dir(&snapshot, &[]).await.map_err(read_error)?;

    let (tx, rx) = mpsc::channel(BACKLOG);
    let handle = Handle::current();
    let task_state = state.clone();
    let task_id = id.to_string();
    std::thread::Builder::new()
        .name(format!("download-{id}"))
        .spawn(move || {
            let mut out = ChannelWriter {
                tx,
                buf: Vec::with_capacity(CHUNK),
                sent: 0,
                closed: false,
                report: Report {
                    state: task_state.clone(),
                    id: task_id.clone(),
                    handle: handle.clone(),
                    at: Instant::now(),
                },
            };
            let result = catch_unwind(AssertUnwindSafe(|| {
                write_archive(
                    &handle,
                    &mut source,
                    &snapshot,
                    &selection,
                    format,
                    &mut out,
                )
            }));
            let failure = match result {
                Ok(Ok(_)) => out.send_buf().err().map(|_| "client_closed"),
                Ok(Err(_)) if out.closed => Some("client_closed"),
                Ok(Err(error)) => {
                    tracing::warn!(%error, download = %task_id, "building the archive failed");
                    Some("read_failed")
                }
                Err(_) => Some("read_failed"),
            };
            if failure == Some("read_failed") {
                let _ = out
                    .tx
                    .blocking_send(Err(io::Error::other("the archive failed")));
            }
            let sent = out.sent;
            drop(out);
            handle.block_on(finish(&task_state, &task_id, sent, failure));
        })
        .map_err(|e| ApiError::Internal(e.into()))?;

    Response::builder()
        .header(CONTENT_TYPE, format.content_type())
        .header(
            CONTENT_DISPOSITION,
            format!(
                "attachment; filename=\"{}\"",
                file_name(&hostname, &snapshot_id, format)
            ),
        )
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .map_err(|e| ApiError::Internal(e.into()))
}

pub async fn interrupt_running(state: &AppState) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE downloads SET state = 'interrupted', error_code = 'server_restarted', \
         finished_at = $1 WHERE state = 'running'",
    )
    .bind(now_ms())
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

/// The web app opens the archive link in a hidden same-origin frame, so the
/// browser's download manager takes the stream and the app can still read an
/// error answer (used or expired link). Only this route may be framed, and
/// only by the app's own origin.
fn same_origin_frame(router: OpenApiRouter<AppState>) -> OpenApiRouter<AppState> {
    router
        .layer(SetResponseHeaderLayer::overriding(
            X_FRAME_OPTIONS,
            HeaderValue::from_static("SAMEORIGIN"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(CSP_SAME_ORIGIN_FRAME),
        ))
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_download))
        .merge(same_origin_frame(
            OpenApiRouter::new().routes(routes!(download_archive)),
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_name_uses_sanitized_host_and_short_snapshot_id() {
        assert_eq!(safe_host("héllo 🙂"), "h_llo__");
        assert_eq!(safe_host(""), "snapshot");
        assert_eq!(safe_host("web-1"), "web-1");
        let id = "0123456789abcdef".repeat(4);
        assert_eq!(
            file_name("web-1", &id, ArchiveFormat::TarZst),
            "web-1-01234567.tar.zst"
        );
        assert_eq!(
            file_name("", &id, ArchiveFormat::Zip),
            "snapshot-01234567.zip"
        );
    }
}
