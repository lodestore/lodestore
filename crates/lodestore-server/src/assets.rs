#[cfg(feature = "embed-ui")]
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use crate::error::ApiError;

#[cfg(feature = "embed-ui")]
#[derive(rust_embed::RustEmbed)]
#[folder = "../../web/dist"]
struct Ui;

pub async fn fallback(uri: Uri) -> Response {
    let path = uri.path();
    if path == "/api" || path.starts_with("/api/") {
        return ApiError::NotFound("not found".into()).into_response();
    }
    serve(path.trim_start_matches('/'))
}

#[cfg(not(feature = "embed-ui"))]
fn serve(_path: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        "the web UI is not built into this binary (build with --features embed-ui)",
    )
        .into_response()
}

#[cfg(feature = "embed-ui")]
fn serve(path: &str) -> Response {
    if !path.is_empty()
        && let Some(file) = Ui::get(path)
    {
        let cache = if path.starts_with("assets/") {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        return file_response(content_type(path), cache, file.data.into_owned());
    }
    if path.starts_with("assets/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    match Ui::get("index.html") {
        Some(index) => file_response(
            "text/html; charset=utf-8",
            "no-cache",
            index.data.into_owned(),
        ),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(feature = "embed-ui")]
fn file_response(content_type: &'static str, cache: &'static str, body: Vec<u8>) -> Response {
    ([(CONTENT_TYPE, content_type), (CACHE_CONTROL, cache)], body).into_response()
}

#[cfg(feature = "embed-ui")]
fn content_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}
