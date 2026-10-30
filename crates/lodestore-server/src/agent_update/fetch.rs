use std::time::{Duration, Instant};

use lodestore_proto::agent::v1::{FetchAgentUpdateRequest, UpdateChunk, UpdateFile};
use semver::Version;
use sqlx::Row;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Status;

use crate::agent_update::manifest;
use crate::db::now_ms;
use crate::grpc::internal;
use crate::state::AppState;

pub const UPDATE_CHUNK: usize = 256 * 1024;
const STREAM_DEPTH: usize = 4;
const PROGRESS_EVERY: Duration = Duration::from_secs(5);

fn denied() -> Status {
    Status::permission_denied("no update is assigned to this agent")
}

async fn read_chunk(file: &mut tokio::fs::File) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; UPDATE_CHUNK];
    let mut filled = 0;
    while filled < buf.len() {
        let n = file.read(&mut buf[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    buf.truncate(filled);
    Ok(buf)
}

async fn mark_progress(state: &AppState, update_id: &str) -> bool {
    match sqlx::query(
        "UPDATE agent_updates SET progress_at = $1 WHERE id = $2 AND state = 'assigned'",
    )
    .bind(now_ms())
    .bind(update_id)
    .execute(&state.db.pool)
    .await
    {
        Ok(done) => done.rows_affected() == 1,
        Err(error) => {
            tracing::warn!(%error, update_id, "could not record agent update progress");
            true
        }
    }
}

pub async fn fetch(
    state: &AppState,
    agent_id: &str,
    request: FetchAgentUpdateRequest,
) -> Result<ReceiverStream<Result<UpdateChunk, Status>>, Status> {
    if state.registry.is_revoked(agent_id) {
        return Err(denied());
    }
    let signature = match UpdateFile::try_from(request.file) {
        Ok(UpdateFile::Binary) => false,
        Ok(UpdateFile::Signature) => true,
        _ => return Err(Status::invalid_argument("unknown update file")),
    };
    let row = sqlx::query(
        "SELECT arch, target_version FROM agent_updates \
         WHERE id = $1 AND host_id = $2 AND state = 'assigned'",
    )
    .bind(&request.update_id)
    .bind(agent_id)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(internal)?
    .ok_or_else(denied)?;
    let arch: String = row.try_get("arch").map_err(internal)?;
    let target: String = row.try_get("target_version").map_err(internal)?;
    let not_found = || Status::not_found("the update file is not on this server");
    let version = Version::parse(&target).map_err(|_| not_found())?;
    let binaries = manifest::binaries(&state.config).await;
    let binary = manifest::binary_for(&binaries, &arch, &version).ok_or_else(not_found)?;
    let path = if signature {
        &binary.signature_path
    } else {
        &binary.path
    };
    let mut file = tokio::fs::File::open(path).await.map_err(|error| {
        tracing::warn!(%error, "could not open an agent update file");
        not_found()
    })?;

    let (tx, rx) = mpsc::channel(STREAM_DEPTH);
    let state = state.clone();
    let update_id = request.update_id;
    tokio::spawn(async move {
        let mut last_progress: Option<Instant> = None;
        loop {
            let data = match read_chunk(&mut file).await {
                Ok(data) if data.is_empty() => return,
                Ok(data) => data,
                Err(error) => {
                    tracing::warn!(%error, "reading an agent update file failed");
                    let _ = tx
                        .send(Err(Status::internal("reading the update failed")))
                        .await;
                    return;
                }
            };
            if tx.send(Ok(UpdateChunk { data })).await.is_err() {
                return;
            }
            if last_progress.is_none_or(|t| t.elapsed() >= PROGRESS_EVERY) {
                last_progress = Some(Instant::now());
                if !mark_progress(&state, &update_id).await {
                    let _ = tx
                        .send(Err(Status::failed_precondition(
                            "the update is no longer assigned",
                        )))
                        .await;
                    return;
                }
            }
        }
    });
    Ok(ReceiverStream::new(rx))
}
