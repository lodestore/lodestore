use std::time::Duration;

use axum::Router;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS,
    X_FRAME_OPTIONS,
};
use axum::http::{HeaderValue, StatusCode};
use axum::routing::get;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;

use crate::state::AppState;
use crate::{assets, auth, hosts, install, repos, system};

#[derive(OpenApi)]
#[openapi(info(title = "Lodestore", description = "Lodestore server API"))]
struct ApiDoc;

fn api_router() -> OpenApiRouter<AppState> {
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .merge(auth::routes())
        .merge(system::routes())
        .merge(hosts::routes())
        .merge(repos::routes())
}

pub fn openapi() -> utoipa::openapi::OpenApi {
    api_router().split_for_parts().1
}

const CSP: &str = "default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
    object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'";

fn header(
    name: axum::http::HeaderName,
    value: &'static str,
) -> SetResponseHeaderLayer<HeaderValue> {
    SetResponseHeaderLayer::if_not_present(name, HeaderValue::from_static(value))
}

pub fn router(state: AppState) -> Router {
    let (router, _) = api_router().split_for_parts();
    router
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
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
