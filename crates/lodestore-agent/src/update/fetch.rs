//! Downloading an announced agent update over the agent's own mTLS channel.

use std::path::{Path, PathBuf};

use lodestore_proto::agent::v1::agent_client::AgentClient;
use lodestore_proto::agent::v1::{FetchAgentUpdateRequest, UpdateAssign, UpdateChunk, UpdateFile};
use tokio::io::AsyncWriteExt;
use tokio_stream::{Stream, StreamExt};
use tonic::Status;
use tonic::transport::Channel;

use super::MAX_UPDATE_BYTES;
use super::apply::temp_path;

pub const MAX_SIGNATURE_BYTES: usize = 4096;

pub fn download_limit(size: u64) -> u64 {
    size.min(MAX_UPDATE_BYTES)
}

fn interrupted(status: &Status) -> String {
    format!("The download was interrupted: {}", status.message())
}

/// Writes the stream to a new file at `dest`; the file is removed on any
/// failure, so a refused download leaves nothing behind.
pub async fn save_binary(
    stream: impl Stream<Item = Result<UpdateChunk, Status>>,
    dest: &Path,
    limit: u64,
) -> Result<(), String> {
    let result = write_all(stream, dest, limit).await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(dest).await;
    }
    result
}

async fn write_all(
    stream: impl Stream<Item = Result<UpdateChunk, Status>>,
    dest: &Path,
    limit: u64,
) -> Result<(), String> {
    let io = |e: std::io::Error| format!("Could not save the download: {e}");
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dest)
        .await
        .map_err(io)?;
    let mut stream = std::pin::pin!(stream);
    let mut total = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|s| interrupted(&s))?;
        total += chunk.data.len() as u64;
        if total > limit {
            return Err("The download is larger than announced.".into());
        }
        file.write_all(&chunk.data).await.map_err(io)?;
    }
    file.sync_all().await.map_err(io)
}

pub async fn read_signature(
    stream: impl Stream<Item = Result<UpdateChunk, Status>>,
) -> Result<Vec<u8>, String> {
    let mut stream = std::pin::pin!(stream);
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        bytes.extend(chunk.map_err(|s| interrupted(&s))?.data);
        if bytes.len() > MAX_SIGNATURE_BYTES {
            return Err("The signature file is larger than 4 KiB.".into());
        }
    }
    Ok(bytes)
}

async fn open(
    client: &mut AgentClient<Channel>,
    update_id: &str,
    file: UpdateFile,
) -> Result<tonic::Streaming<UpdateChunk>, String> {
    client
        .fetch_agent_update(FetchAgentUpdateRequest {
            update_id: update_id.to_string(),
            file: file as i32,
        })
        .await
        .map(tonic::Response::into_inner)
        .map_err(|s| format!("The server refused the download: {}", s.message()))
}

/// Fetches the signature, then the binary into a new file next to `exe`.
pub async fn download(
    channel: Channel,
    assign: &UpdateAssign,
    exe: &Path,
) -> Result<(PathBuf, Vec<u8>), String> {
    let mut client = AgentClient::new(channel);
    let signature =
        read_signature(open(&mut client, &assign.update_id, UpdateFile::Signature).await?).await?;
    let dest = temp_path(exe).map_err(|e| format!("Could not save the download: {e}"))?;
    let binary = open(&mut client, &assign.update_id, UpdateFile::Binary).await?;
    save_binary(binary, &dest, download_limit(assign.size)).await?;
    Ok((dest, signature))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(
        parts: Vec<Result<Vec<u8>, Status>>,
    ) -> impl Stream<Item = Result<UpdateChunk, Status>> {
        tokio_stream::iter(
            parts
                .into_iter()
                .map(|p| p.map(|data| UpdateChunk { data })),
        )
    }

    #[tokio::test]
    async fn binary_over_assigned_size_is_rejected_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("new");
        let stream = chunks(vec![Ok(vec![1; 6]), Ok(vec![2; 6])]);
        let error = save_binary(stream, &dest, 10).await.unwrap_err();
        assert_eq!(error, "The download is larger than announced.");
        assert!(!dest.exists());
    }

    #[tokio::test]
    async fn binary_within_the_limit_is_saved() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("new");
        save_binary(chunks(vec![Ok(vec![1; 6]), Ok(vec![2; 4])]), &dest, 10)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap().len(), 10);
    }

    #[test]
    fn download_limit_is_capped_at_256_mib() {
        assert_eq!(download_limit(u64::MAX), MAX_UPDATE_BYTES);
        assert_eq!(download_limit(10), 10);
    }

    #[tokio::test]
    async fn signature_over_4_kib_is_rejected() {
        let long = chunks(vec![Ok(vec![b'a'; 4000]), Ok(vec![b'a'; 200])]);
        assert!(read_signature(long).await.is_err());
        let ok = chunks(vec![Ok(vec![b'a'; 4096])]);
        assert_eq!(read_signature(ok).await.unwrap().len(), 4096);
    }

    #[tokio::test]
    async fn stream_error_removes_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("new");
        let stream = chunks(vec![Ok(vec![1; 6]), Err(Status::unavailable("gone"))]);
        let error = save_binary(stream, &dest, 100).await.unwrap_err();
        assert!(error.contains("interrupted"), "{error}");
        assert!(!dest.exists());
    }
}
