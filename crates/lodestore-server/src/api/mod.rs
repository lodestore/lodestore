use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::Request;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_LENGTH, CONTENT_SECURITY_POLICY, CONTENT_TYPE, REFERRER_POLICY,
    X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;

use crate::error::ErrorBody;
use crate::state::AppState;
use crate::{
    assets, auth, browse, catalog, download, hosts, install, jobs, policies, repos, system,
};

mod docs;

#[derive(OpenApi)]
#[openapi(
    info(title = "Lodestore", description = include_str!("description.md")),
    modifiers(&Security),
    security(("session" = [])),
    tags(
        (name = "Auth", description = "Sign in and out, read the session and its CSRF token."),
        (name = "Hosts", description = "Enrolled hosts, their labels, join tokens and manual backups."),
        (name = "Policies", description = "Backup policies: which hosts, which paths, which repository."),
        (name = "Jobs", description = "Backup jobs: history, live progress, log, cancel and rerun."),
        (name = "Snapshots", description = "Snapshots recorded in the server catalog."),
        (name = "Restore", description = "Hosts and snapshots to restore from, newest first."),
        (name = "Repositories", description = "Storage repositories and the credentials agents write with."),
        (name = "Server", description = "Server version and health."),
        (name = "Setup", description = "First start: create the administrator from the setup link."),
    )
)]
struct ApiDoc;

struct Security;

impl utoipa::Modify for Security {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::security::{ApiKey, ApiKeyValue, SecurityScheme};
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "session",
            SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::with_description(
                auth::SESSION_COOKIE,
                "Session cookie set by `POST /api/v1/auth/login`.",
            ))),
        );
        components.add_security_scheme(
            "csrf",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                auth::CSRF_HEADER,
                "Required on every request that changes data. \
                 Read the token from `GET /api/v1/auth/session`.",
            ))),
        );
    }
}

fn api_router() -> OpenApiRouter<AppState> {
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .merge(auth::routes())
        .merge(system::routes())
        .merge(hosts::routes())
        .merge(repos::routes())
        .merge(catalog::routes())
        .merge(browse::routes())
        .merge(download::routes())
        .merge(policies::routes())
        .merge(jobs::api::routes())
}

pub fn openapi() -> utoipa::openapi::OpenApi {
    api_router().split_for_parts().1
}

macro_rules! csp {
    ($frame_ancestors:literal) => {
        concat!(
            "default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; ",
            "object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors ",
            $frame_ancestors
        )
    };
}

const CSP: &str = csp!("'none'");
/// For responses the app itself loads in a hidden frame; other sites still cannot frame them.
pub(crate) const CSP_SAME_ORIGIN_FRAME: &str = csp!("'self'");

fn header(
    name: axum::http::HeaderName,
    value: &'static str,
) -> SetResponseHeaderLayer<HeaderValue> {
    SetResponseHeaderLayer::if_not_present(name, HeaderValue::from_static(value))
}

async fn json_errors(request: Request, next: Next) -> Response {
    let api = request.uri().path().starts_with("/api/");
    let response = next.run(request).await;
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let plain = content_type.is_none_or(|v| v.starts_with("text/plain"));
    if !api || !response.status().is_client_error() || !plain {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let message = axum::body::to_bytes(body, 64 * 1024)
        .await
        .map(|b| String::from_utf8_lossy(&b).trim().to_owned())
        .unwrap_or_default();
    let status = parts.status;
    let code = match status {
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "unsupported_media_type",
        StatusCode::PAYLOAD_TOO_LARGE => "payload_too_large",
        StatusCode::REQUEST_TIMEOUT => "timeout",
        StatusCode::UNPROCESSABLE_ENTITY => "validation",
        StatusCode::METHOD_NOT_ALLOWED => "method_not_allowed",
        StatusCode::NOT_FOUND => "not_found",
        _ => "bad_request",
    };
    let message = if message.is_empty() {
        status
            .canonical_reason()
            .unwrap_or("request failed")
            .to_owned()
    } else {
        message
    };
    parts.headers.remove(CONTENT_TYPE);
    parts.headers.remove(CONTENT_LENGTH);
    let body = ErrorBody {
        code: code.into(),
        message,
    };
    (parts, Json(body)).into_response()
}

pub fn router(state: AppState) -> Router {
    let (router, _) = api_router().split_for_parts();
    router
        .merge(docs::routes())
        .layer(header(CACHE_CONTROL, "no-store"))
        .route("/healthz", get(|| async { "ok" }))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(60),
        ))
        .merge(install::router())
        .fallback(assets::fallback)
        .layer(header(CONTENT_SECURITY_POLICY, CSP))
        .layer(header(X_CONTENT_TYPE_OPTIONS, "nosniff"))
        .layer(header(X_FRAME_OPTIONS, "DENY"))
        .layer(header(REFERRER_POLICY, "no-referrer"))
        .layer(CatchPanicLayer::new())
        .layer(RequestBodyLimitLayer::new(1024 * 1024))
        .layer(axum::middleware::from_fn(json_errors))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
