use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use sha2::{Digest, Sha256};
use tokio_util::io::ReaderStream;

use crate::config::Config;
use crate::state::AppState;

pub(crate) const ARCHES: [(&str, &str); 2] = [
    ("x86_64", "lodestore-agent-linux-x86_64"),
    ("aarch64", "lodestore-agent-linux-aarch64"),
];

const SCRIPT: &str = include_str!("install.sh");

type Fingerprint = (SystemTime, u64);

static CHECKSUMS: Mutex<Option<HashMap<PathBuf, (Fingerprint, String)>>> = Mutex::new(None);

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/install.sh", get(install_script))
        .route("/install/agent/{name}", get(agent_binary))
}

pub fn install_command(public_url: &str, token: &str) -> String {
    let url = public_url.trim_end_matches('/');
    if url.starts_with("https://") {
        format!("curl --proto '=https' --tlsv1.2 -fsSL {url}/install.sh | sh -s -- --join {token}")
    } else {
        format!("curl -fsSL {url}/install.sh | sh -s -- --insecure --join {token}")
    }
}

pub fn available_arches(config: &Config) -> Vec<&'static str> {
    ARCHES
        .iter()
        .filter(|(_, file)| config.agent_dir.join(file).is_file())
        .map(|(arch, _)| *arch)
        .collect()
}

async fn install_script(State(state): State<AppState>) -> Response {
    let body = SCRIPT
        .replace(
            "@@SERVER_URL@@",
            state.config.public_url.trim_end_matches('/'),
        )
        .replace("@@VERSION@@", env!("CARGO_PKG_VERSION"));
    (
        [
            (
                CONTENT_TYPE,
                HeaderValue::from_static("text/x-shellscript; charset=utf-8"),
            ),
            (CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        body,
    )
        .into_response()
}

fn not_found(arch: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        format!("agent binary for {arch} is not available on this server; use --from-github"),
    )
        .into_response()
}

async fn agent_binary(State(state): State<AppState>, UrlPath(name): UrlPath<String>) -> Response {
    let (arch, checksum_only) = match name.strip_suffix(".sha256") {
        Some(arch) => (arch, true),
        None => (name.as_str(), false),
    };
    let Some((arch, file)) = ARCHES.iter().find(|(a, _)| *a == arch) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let path = state.config.agent_dir.join(file);
    if checksum_only {
        return match checksum(path).await {
            Some(hex) => ([(CONTENT_TYPE, "text/plain; charset=utf-8")], hex).into_response(),
            None => not_found(arch),
        };
    }
    let Ok(file) = tokio::fs::File::open(&path).await else {
        return not_found(arch);
    };
    let Ok(meta) = file.metadata().await else {
        return not_found(arch);
    };
    (
        [
            (
                CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            ),
            (CONTENT_LENGTH, HeaderValue::from(meta.len())),
        ],
        Body::from_stream(ReaderStream::new(file)),
    )
        .into_response()
}

pub(crate) async fn checksum(path: PathBuf) -> Option<String> {
    tokio::task::spawn_blocking(move || checksum_blocking(&path))
        .await
        .ok()
        .flatten()
}

fn checksum_blocking(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    let fingerprint = (meta.modified().ok()?, meta.len());
    if let Some((cached, hex)) = CHECKSUMS.lock().ok()?.as_ref().and_then(|m| m.get(path))
        && *cached == fingerprint
    {
        return Some(hex.clone());
    }
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let hex = data_encoding::HEXLOWER.encode(&hasher.finalize());
    CHECKSUMS
        .lock()
        .ok()?
        .get_or_insert_with(HashMap::new)
        .insert(path.to_path_buf(), (fingerprint, hex.clone()));
    Some(hex)
}

#[cfg(test)]
mod tests {
    use super::install_command;

    #[test]
    fn command_requires_https_unless_the_server_is_plain_http() {
        assert_eq!(
            install_command("https://lode.example/", "T"),
            "curl --proto '=https' --tlsv1.2 -fsSL https://lode.example/install.sh | sh -s -- --join T"
        );
        assert_eq!(
            install_command("http://lode:8080", "T"),
            "curl -fsSL http://lode:8080/install.sh | sh -s -- --insecure --join T"
        );
    }
}
