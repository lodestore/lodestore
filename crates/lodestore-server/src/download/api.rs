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
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tower_http::set_header::SetResponseHeaderLayer;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::archive::{ArchiveFormat, write_archive};
use crate::api::CSP_SAME_ORIGIN_FRAME;
use crate::auth::tokens::{RestoreDownload, Scoped};
use crate::browse::tree::{
    decode_items, decode_paths, read_error, reader, require_items_exist, snapshot_repository,
};
use crate::db::now_ms;
use crate::error::{ApiError, ErrorBody};
use crate::state::AppState;

pub const TICKET_TTL_MS: i64 = 60_000;
const UNUSED_LINK_KEPT_MS: i64 = 3_600_000;
const MAX_ITEMS_JSON: usize = 256 * 1024;
const CHUNK: usize = 64 * 1024;
const BACKLOG: usize = 16;
const PROGRESS_EVERY: Duration = Duration::from_secs(1);
const STALL_POLL: Duration = Duration::from_millis(20);
const FAILURE_PATIENCE: Duration = Duration::from_secs(5);
const REAP_GRACE: Duration = Duration::from_secs(60);
const RUNNING_LIMIT: Duration = Duration::from_secs(24 * 60 * 60);

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
    security(("session" = [], "csrf" = []), ("bearer" = ["restore:download"])),
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
    auth: Scoped<RestoreDownload>,
    Json(mut body): Json<DownloadBody>,
) -> Result<(StatusCode, Json<DownloadCreated>), ApiError> {
    body.items.sort_unstable();
    body.items.dedup();
    let items_json = serde_json::to_string(&body.items).map_err(anyhow::Error::from)?;
    if items_json.len() > MAX_ITEMS_JSON {
        return Err(ApiError::Validation(
            "This selection is too large. Select whole folders instead of single files.".into(),
        ));
    }
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
    .bind(items_json)
    .bind(format_name(body.format))
    .bind(auth.actor())
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
    deadline: Option<Instant>,
    timed_out: bool,
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
    fn expired(&mut self) -> bool {
        if self.deadline.is_some_and(|d| Instant::now() >= d) {
            self.timed_out = true;
        }
        self.timed_out
    }

    fn push(&mut self, chunk: Bytes) -> io::Result<()> {
        if self.deadline.is_none() {
            return self
                .tx
                .blocking_send(Ok(chunk))
                .map_err(|_| io::ErrorKind::BrokenPipe.into());
        }
        let mut chunk = chunk;
        loop {
            match self.tx.try_send(Ok(chunk)) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Closed(_)) => return Err(io::ErrorKind::BrokenPipe.into()),
                Err(TrySendError::Full(Ok(back))) => {
                    if self.expired() {
                        return Err(io::ErrorKind::TimedOut.into());
                    }
                    chunk = back;
                    std::thread::sleep(STALL_POLL);
                }
                Err(TrySendError::Full(Err(_))) => {
                    return Err(io::Error::other("the download stream failed"));
                }
            }
        }
    }

    fn send_buf(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let chunk = Bytes::from(std::mem::replace(&mut self.buf, Vec::with_capacity(CHUNK)));
        let len = chunk.len() as u64;
        if let Err(error) = self.push(chunk) {
            self.closed = !self.timed_out;
            return Err(error);
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
        if self.expired() {
            return Err(io::ErrorKind::TimedOut.into());
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

/// Fails the response stream so the client doesn't take a cut archive for a
/// whole one. A full channel gets `patience` to drain, then the stalled client
/// gets nothing: it must not hold this thread past the download deadline.
fn send_failure(tx: &mpsc::Sender<io::Result<Bytes>>, message: &str, patience: Duration) {
    let until = Instant::now() + patience;
    let mut item = Err(io::Error::other(message.to_string()));
    while let Err(TrySendError::Full(back)) = tx.try_send(item) {
        if Instant::now() >= until {
            return;
        }
        item = back;
        std::thread::sleep(STALL_POLL);
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
    security(("session" = []), ("bearer" = ["restore:download"])),
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
        (status = 409, description = "Cleanup is deleting packs in the repository; the link stays valid", body = ErrorBody),
        (status = 502, description = "The repository storage is unavailable", body = ErrorBody),
    )
)]
async fn download_archive(
    State(state): State<AppState>,
    _auth: Scoped<RestoreDownload>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let repository_id: Option<String> =
        sqlx::query("SELECT repository_id FROM downloads WHERE id = $1")
            .bind(&id)
            .fetch_optional(&state.db.pool)
            .await?
            .map(|row| row.try_get("repository_id"))
            .transpose()?;
    if let Some(repository_id) = repository_id
        && crate::maintenance::gc::db::sweeping(&state, Some(&repository_id)).await?
    {
        return Err(ApiError::Conflict(
            "Cleanup is deleting packs in this repository. Use the link again when it has \
             finished."
                .into(),
        ));
    }
    // The server drops this future when the client leaves or `TimeoutLayer`
    // fires. In its own task the claim always ends in `finish` or the streaming
    // thread, and the signal still stops a start that waits on slow storage.
    let cancelled = CancellationToken::new();
    let _cancel_on_drop = cancelled.clone().drop_guard();
    tokio::spawn(claim_and_start(state, id, cancelled))
        .await
        .map_err(|e| ApiError::Internal(e.into()))?
}

async fn claim_and_start(
    state: AppState,
    id: String,
    cancelled: CancellationToken,
) -> Result<Response, ApiError> {
    let now = now_ms();
    let max_running = if state.config.demo_mode {
        state.config.demo.max_downloads
    } else {
        i64::MAX
    };
    let claimed = sqlx::query(
        "UPDATE downloads SET state = 'running', started_at = $1 \
         WHERE id = $2 AND state = 'pending' AND created_at > $3 \
         AND (SELECT COUNT(*) FROM downloads WHERE state = 'running') < $4",
    )
    .bind(now)
    .bind(&id)
    .bind(now - TICKET_TTL_MS)
    .bind(max_running)
    .execute(&state.db.pool)
    .await?
    .rows_affected()
        == 1;
    if !claimed {
        if state.config.demo_mode && link_is_fresh(&state, &id, now).await? {
            return Err(ApiError::RateLimited);
        }
        return Err(ApiError::NotFound(
            "This download link was already used or expired.".into(),
        ));
    }
    tokio::select! {
        started = start(&state, &id) => {
            if started.is_err() {
                finish(&state, &id, 0, Some("read_failed")).await;
            }
            started
        }
        () = cancelled.cancelled() => {
            finish(&state, &id, 0, Some("client_closed")).await;
            Err(ApiError::Conflict("The download request was cancelled.".into()))
        }
    }
}

async fn link_is_fresh(state: &AppState, id: &str, now: i64) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(
        "SELECT 1 AS one FROM downloads WHERE id = $1 AND state = 'pending' AND created_at > $2",
    )
    .bind(id)
    .bind(now - TICKET_TTL_MS)
    .fetch_optional(&state.db.pool)
    .await?
    .is_some())
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
    let deadline = state
        .config
        .demo_mode
        .then(|| Instant::now() + state.config.demo.download_timeout);
    std::thread::Builder::new()
        .name(format!("download-{id}"))
        .spawn(move || {
            let mut out = ChannelWriter {
                tx,
                buf: Vec::with_capacity(CHUNK),
                sent: 0,
                closed: false,
                deadline,
                timed_out: false,
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
                Ok(Ok(_)) => out.send_buf().err().map(|_| {
                    if out.timed_out {
                        "timed_out"
                    } else {
                        "client_closed"
                    }
                }),
                Ok(Err(_)) if out.timed_out => Some("timed_out"),
                Ok(Err(_)) if out.closed => Some("client_closed"),
                Ok(Err(error)) => {
                    tracing::warn!(%error, download = %task_id, "building the archive failed");
                    Some("read_failed")
                }
                Err(_) => Some("read_failed"),
            };
            match failure {
                Some("timed_out") => {
                    send_failure(&out.tx, "the download took too long", Duration::ZERO);
                }
                Some("read_failed") => {
                    send_failure(&out.tx, "the archive failed", FAILURE_PATIENCE);
                }
                _ => {}
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

/// Frees `running` rows that nothing will finish, such as a failed `finish`.
/// A long download outside the demo has no deadline of its own: past the limit
/// its row stops holding GC, and the stream keeps going.
pub async fn reap_stale(state: &AppState) -> Result<u64, sqlx::Error> {
    let limit = if state.config.demo_mode {
        state.config.demo.download_timeout + REAP_GRACE
    } else {
        RUNNING_LIMIT
    };
    let now = now_ms();
    let reaped = sqlx::query(
        "UPDATE downloads SET state = 'interrupted', error_code = 'timed_out', finished_at = $1 \
         WHERE state = 'running' AND started_at < $2",
    )
    .bind(now)
    .bind(now.saturating_sub(i64::try_from(limit.as_millis()).unwrap_or(i64::MAX)))
    .execute(&state.db.pool)
    .await?;
    Ok(reaped.rows_affected())
}

/// Jobs lists an unused link as not started for `UNUSED_LINK_KEPT_MS`, then the
/// row goes.
pub async fn purge_unused_links(state: &AppState) -> Result<u64, sqlx::Error> {
    let purged = sqlx::query("DELETE FROM downloads WHERE state = 'pending' AND created_at < $1")
        .bind(now_ms() - TICKET_TTL_MS - UNUSED_LINK_KEPT_MS)
        .execute(&state.db.pool)
        .await?;
    Ok(purged.rows_affected())
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

    fn full_channel() -> (
        mpsc::Sender<io::Result<Bytes>>,
        mpsc::Receiver<io::Result<Bytes>>,
    ) {
        let (tx, rx) = mpsc::channel(1);
        tx.try_send(Ok(Bytes::new())).unwrap();
        (tx, rx)
    }

    #[test]
    fn a_stalled_client_cannot_hold_the_failure_report() {
        let (tx, _rx) = full_channel();
        let (done, returned) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            send_failure(&tx, "failed", Duration::from_millis(50));
            done.send(()).unwrap();
        });
        assert!(returned.recv_timeout(Duration::from_secs(2)).is_ok());
    }

    #[test]
    fn a_draining_client_gets_the_failure() {
        let (tx, mut rx) = full_channel();
        let sender = std::thread::spawn(move || {
            send_failure(&tx, "failed", Duration::from_secs(30));
        });
        std::thread::sleep(STALL_POLL * 2);
        assert!(rx.blocking_recv().unwrap().is_ok());
        assert!(rx.blocking_recv().unwrap().is_err());
        sender.join().unwrap();
    }

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
