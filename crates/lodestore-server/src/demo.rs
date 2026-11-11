use std::net::SocketAddr;
use std::time::Duration;

use axum::Json;
use axum::extract::{ConnectInfo, MatchedPath, Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum_extra::extract::cookie::CookieJar;
use sqlx::Row;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::limits::{self, GuardError};
use crate::auth::{bearer_secret, start_session_for};
use crate::error::{ApiError, ErrorBody};
use crate::state::AppState;

pub const SESSION_LIFETIME: Duration = Duration::from_secs(3600);

const SETTING: &str = "demo_mode";
const CHANNELS: &str = "/api/v1/notifications/channels";

pub const ALLOWED: &[(&str, &str)] = &[
    ("POST", "/api/v1/auth/demo"),
    ("POST", "/api/v1/auth/logout"),
    ("POST", "/api/v1/snapshots/{id}/summary"),
    ("POST", "/api/v1/find"),
    ("POST", "/api/v1/downloads"),
    ("POST", "/api/v1/policies/validate"),
    ("POST", "/api/v1/policies/{name}/retention-preview"),
    ("DELETE", "/api/v1/find/{id}"),
    ("GET", "/api/v1/auth/session"),
    ("GET", "/api/v1/server/status"),
    ("GET", "/api/v1/server/info"),
    ("GET", "/api/v1/account"),
    ("GET", "/api/v1/tokens"),
    ("GET", "/api/v1/agent-policy"),
    ("GET", "/api/v1/fleet"),
    ("GET", "/api/v1/fleet/hosts/{id}"),
    ("GET", "/api/v1/hosts"),
    ("GET", "/api/v1/hosts/{id}"),
    ("GET", "/api/v1/hosts/{id}/snapshots"),
    ("GET", "/api/v1/hosts/{id}/policies"),
    ("GET", "/api/v1/catalog"),
    ("GET", "/api/v1/snapshots/{id}"),
    ("GET", "/api/v1/snapshots/{id}/entries"),
    ("GET", "/api/v1/snapshots/{id}/search"),
    ("GET", "/api/v1/restore/hosts"),
    ("GET", "/api/v1/restore/snapshots"),
    ("GET", "/api/v1/find/{id}"),
    ("GET", "/api/v1/find/{id}/groups/{index}"),
    ("GET", "/api/v1/downloads/{id}/archive"),
    ("GET", "/api/v1/jobs"),
    ("GET", "/api/v1/jobs/{id}"),
    ("GET", "/api/v1/jobs/{id}/events"),
    ("GET", "/api/v1/alerts"),
    ("GET", "/api/v1/alerts/count"),
    ("GET", "/api/v1/policies"),
    ("GET", "/api/v1/policies/{name}"),
    ("GET", "/api/v1/repositories"),
    ("GET", "/api/v1/repositories/{id}"),
    ("GET", "/api/v1/repositories/{id}/gc"),
    (
        "GET",
        "/api/v1/repositories/{id}/gc/cycles/{cycle_id}/report",
    ),
    ("GET", "/api/v1/repositories/{id}/checks"),
    ("GET", "/api/v1/repositories/{id}/checks/runs/{run_id}"),
    (
        "GET",
        "/api/v1/repositories/{id}/checks/runs/{run_id}/report",
    ),
    ("GET", "/api/v1/agent-updates"),
    ("GET", CHANNELS),
    ("GET", "/api/v1/notifications/thresholds"),
];

/// Default-deny by (method, route template): anything not listed in
/// [`ALLOWED`] is refused, including HEAD, OPTIONS and wrong methods.
pub async fn guard(request: Request, next: Next) -> Response {
    if bearer_secret(request.headers()).is_some() {
        return ApiError::DemoMode.into_response();
    }
    let Some(template) = request.extensions().get::<MatchedPath>() else {
        return ApiError::DemoMode.into_response();
    };
    let method = request.method();
    if !ALLOWED
        .iter()
        .any(|(m, path)| *m == method.as_str() && *path == template.as_str())
    {
        return ApiError::DemoMode.into_response();
    }
    if method == Method::GET && template.as_str() == CHANNELS {
        return Json(Vec::<()>::new()).into_response();
    }
    next.run(request).await
}

/// Sessions of one mode must not outlive a restart in the other: a demo
/// cookie would otherwise stay a full administrator session once the flag is
/// switched off, and an administrator session would survive into the demo.
pub async fn reset_sessions_on_mode_change(state: &AppState) -> Result<(), sqlx::Error> {
    let current = if state.config.demo_mode { "1" } else { "0" };
    let stored: Option<String> = sqlx::query("SELECT value FROM settings WHERE key = $1")
        .bind(SETTING)
        .fetch_optional(&state.db.pool)
        .await?
        .map(|row| row.try_get("value"))
        .transpose()?;
    if stored.as_deref().unwrap_or("0") == current {
        return Ok(());
    }
    let mut tx = state.db.pool.begin().await?;
    let removed = sqlx::query("DELETE FROM sessions")
        .execute(&mut *tx)
        .await?
        .rows_affected();
    sqlx::query(
        "INSERT INTO settings (key, value) VALUES ($1, $2) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
    )
    .bind(SETTING)
    .bind(current)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    tracing::info!(
        removed,
        demo_mode = state.config.demo_mode,
        "demo_mode changed; all sessions ended"
    );
    Ok(())
}

#[utoipa::path(
    post,
    tag = "Auth",
    summary = "Sign in to the public demo",
    description = "Only exists on a server started with `demo_mode`; elsewhere it answers 404. \
        Signs in as the administrator without a password for one hour.",
    security(()),
    path = "/api/v1/auth/demo",
    responses(
        (status = 204, description = "Signed in; the session cookie is set"),
        (status = 404, description = "This server is not a demo", body = ErrorBody),
        (status = 409, description = "No administrator exists yet", body = ErrorBody),
        (status = 429, description = "Too many sign-ins", body = ErrorBody),
    )
)]
async fn auth_demo(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    jar: CookieJar,
) -> Result<(CookieJar, StatusCode), GuardError> {
    if !state.config.demo_mode {
        return Err(ApiError::NotFound("not found".into()).into());
    }
    let client = limits::client_ip(&state.config, peer, &headers);
    state.auth.limits.check_demo(client)?;
    let admin =
        sqlx::query("SELECT id, password_hash FROM admin_users ORDER BY created_at LIMIT 1")
            .fetch_optional(&state.db.pool)
            .await?
            .ok_or(ApiError::SetupRequired)?;
    let jar = start_session_for(
        &state,
        jar,
        &admin.try_get::<String, _>("id")?,
        &admin.try_get::<String, _>("password_hash")?,
        SESSION_LIFETIME,
        None,
    )
    .await?;
    Ok((jar, StatusCode::NO_CONTENT))
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(auth_demo))
}
