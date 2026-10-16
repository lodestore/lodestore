use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum_extra::extract::cookie::CookieJar;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::limits::{GuardError, client_ip};
use super::{
    admin_count, check_password, check_username, hash_password, random_token, start_session,
};
use crate::db::now_ms;
use crate::error::{ApiError, ErrorBody};
use crate::state::AppState;

#[derive(Clone, Default)]
pub struct SetupState {
    token_hash: Arc<Mutex<Option<[u8; 32]>>>,
}

fn hash_token(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl SetupState {
    fn arm(&self, token: &str) {
        *self.token_hash.lock().expect("setup token lock") = Some(hash_token(token));
    }

    fn matches(&self, candidate: &str) -> bool {
        let guard = self.token_hash.lock().expect("setup token lock");
        guard.is_some_and(|stored| ct_eq(&stored, &hash_token(candidate)))
    }

    fn take(&self, candidate: &str) -> bool {
        let mut guard = self.token_hash.lock().expect("setup token lock");
        if guard.is_some_and(|stored| ct_eq(&stored, &hash_token(candidate))) {
            *guard = None;
            return true;
        }
        false
    }
}

pub fn announce(state: &AppState) {
    let token = random_token();
    state.auth.setup.arm(&token);
    tracing::info!(
        "open {}/setup?token={token} to create the administrator",
        state.config.public_url.trim_end_matches('/')
    );
}

#[derive(Deserialize, ToSchema)]
pub struct VerifyRequest {
    pub token: String,
}

#[derive(Deserialize, ToSchema)]
pub struct SetupRequest {
    pub token: String,
    pub username: String,
    pub password: String,
}

#[utoipa::path(
    post,
    tag = "Setup",
    summary = "Check a one-time setup link",
    security(()),
    path = "/api/v1/setup/verify",
    request_body = VerifyRequest,
    responses(
        (status = 204, description = "The setup link is valid"),
        (status = 404, description = "The setup link is invalid or already used", body = ErrorBody),
        (status = 429, description = "Too many attempts", body = ErrorBody),
    )
)]
async fn verify(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<VerifyRequest>,
) -> Result<StatusCode, GuardError> {
    let client = client_ip(&state.config, peer, &headers);
    state.auth.limits.check_setup(client)?;
    if state.auth.setup.matches(&req.token) {
        state.auth.limits.setup_succeeded(client);
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound("the setup link is invalid or already used".into()).into())
    }
}

#[utoipa::path(
    post,
    tag = "Setup",
    summary = "Create the administrator",
    security(()),
    path = "/api/v1/setup",
    request_body = SetupRequest,
    responses(
        (status = 204, description = "The administrator is created and signed in"),
        (status = 404, description = "The setup link is invalid or already used", body = ErrorBody),
        (status = 409, description = "An administrator already exists", body = ErrorBody),
        (status = 422, description = "Invalid username or password", body = ErrorBody),
        (status = 429, description = "Too many attempts", body = ErrorBody),
    )
)]
async fn create_admin(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    jar: CookieJar,
    Json(req): Json<SetupRequest>,
) -> Result<(CookieJar, StatusCode), GuardError> {
    let client = client_ip(&state.config, peer, &headers);
    state.auth.limits.check_setup(client)?;
    if admin_count(&state).await? > 0 {
        return Err(ApiError::Conflict("an administrator already exists".into()).into());
    }
    if !state.auth.setup.matches(&req.token) {
        return Err(ApiError::NotFound("the setup link is invalid or already used".into()).into());
    }
    check_username(&req.username).map_err(|m| ApiError::Validation(m.into()))?;
    check_password(&req.password).map_err(|m| ApiError::Validation(m.into()))?;

    let password = req.password;
    let hash = tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|e| ApiError::Internal(e.into()))??;
    if !state.auth.setup.take(&req.token) {
        return Err(ApiError::NotFound("the setup link is invalid or already used".into()).into());
    }
    let user_id = uuid::Uuid::now_v7().to_string();
    let inserted = sqlx::query(
        "INSERT INTO admin_users (id, username, password_hash, created_at) VALUES ($1, $2, $3, $4)",
    )
    .bind(&user_id)
    .bind(&req.username)
    .bind(hash)
    .bind(now_ms())
    .execute(&state.db.pool)
    .await;
    if let Err(e) = inserted {
        state.auth.setup.arm(&req.token);
        return Err(e.into());
    }
    tracing::info!(username = %req.username, "admin account created");
    state.auth.limits.setup_succeeded(client);
    let jar = start_session(&state, jar, &user_id).await?;
    Ok((jar, StatusCode::NO_CONTENT))
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(verify))
        .routes(routes!(create_admin))
}
