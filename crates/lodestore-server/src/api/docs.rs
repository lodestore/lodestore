use std::sync::OnceLock;

use axum::Router;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::{Html, IntoResponse};
use axum::routing::get;

use crate::auth::AuthUser;
use crate::state::AppState;

const BUNDLE_PATH: &str = "/api/docs/scalar-1.74.0.js";
const BUNDLE: &[u8] = include_bytes!("../../assets/scalar/standalone-1.74.0.js");
const INIT_PATH: &str = "/api/docs/init.js";
const INIT: &str = include_str!("docs-init.js");
const SPEC_PATH: &str = "/api/v1/openapi.json";

const CONFIGURATION: &str = r#"{"withDefaultFonts":false,"telemetry":false,"hideTestRequestButton":true,"hideClientButton":true,"showDeveloperTools":"never","hideDarkModeToggle":true,"agent":{"disabled":true},"mcp":{"disabled":true}}"#;

fn page() -> &'static str {
    static PAGE: OnceLock<String> = OnceLock::new();
    PAGE.get_or_init(|| {
        format!(
            "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
             <title>Lodestore API reference</title>\n</head>\n<body>\n\
             <script id=\"api-reference\" data-url=\"{SPEC_PATH}\" data-configuration='{CONFIGURATION}'></script>\n\
             <script src=\"{INIT_PATH}\"></script>\n\
             <script src=\"{BUNDLE_PATH}\"></script>\n</body>\n</html>\n"
        )
    })
}

fn spec() -> &'static str {
    static SPEC: OnceLock<String> = OnceLock::new();
    SPEC.get_or_init(|| {
        super::openapi()
            .to_json()
            .expect("the OpenAPI document serialises")
    })
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/docs", get(|_: AuthUser| async { Html(page()) }))
        .route(
            BUNDLE_PATH,
            get(|_: AuthUser| async {
                (
                    [
                        (CONTENT_TYPE, "text/javascript; charset=utf-8"),
                        (CACHE_CONTROL, "private, max-age=31536000, immutable"),
                    ],
                    BUNDLE,
                )
                    .into_response()
            }),
        )
        .route(
            INIT_PATH,
            get(|_: AuthUser| async {
                ([(CONTENT_TYPE, "text/javascript; charset=utf-8")], INIT).into_response()
            }),
        )
        .route(
            SPEC_PATH,
            get(|_: AuthUser| async {
                ([(CONTENT_TYPE, "application/json")], spec()).into_response()
            }),
        )
}
