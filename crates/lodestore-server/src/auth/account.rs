//! The administrator's own account: the email that alerts go to by default
//! and the password. Session only: an API token never reaches these routes.

use std::net::SocketAddr;

use axum::Json;
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum_extra::extract::cookie::CookieJar;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::limits::{GuardError, client_ip};
use super::{
    AuthUser, MAX_SUBMITTED_PASSWORD_BYTES, SESSION_COOKIE, check_password, hash_password,
    session_hash, verify_password,
};
use crate::error::{ApiError, ErrorBody};
use crate::maintenance::gc::api::FieldError;
use crate::state::AppState;

const INVALID_EMAIL: &str = "Enter an address such as admin@example.com.";
const WRONG_PASSWORD: &str = "Current password is wrong.";

#[derive(Debug, Serialize, ToSchema)]
pub struct Account {
    pub username: String,
    pub email: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AccountInput {
    /// Empty or left out clears the address.
    pub email: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct PasswordChange {
    pub current_password: String,
    pub new_password: String,
}

#[derive(Serialize, ToSchema)]
pub struct AccountErrors {
    pub errors: Vec<FieldError>,
}

pub enum AccountFailure {
    Guard(GuardError),
    Email,
    Rejected(&'static str, &'static str),
}

impl<E: Into<GuardError>> From<E> for AccountFailure {
    fn from(error: E) -> Self {
        AccountFailure::Guard(error.into())
    }
}

impl IntoResponse for AccountFailure {
    fn into_response(self) -> Response {
        match self {
            AccountFailure::Guard(error) => error.into_response(),
            AccountFailure::Email => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(AccountErrors {
                    errors: vec![FieldError {
                        path: "email".into(),
                        message: INVALID_EMAIL.into(),
                    }],
                }),
            )
                .into_response(),
            AccountFailure::Rejected(code, message) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ErrorBody {
                    code: code.into(),
                    message: message.into(),
                }),
            )
                .into_response(),
        }
    }
}

/// The address of the administrator, when one is set.
pub async fn admin_email(state: &AppState) -> anyhow::Result<Option<String>> {
    let row = sqlx::query("SELECT email FROM admin_users ORDER BY created_at, id LIMIT 1")
        .fetch_optional(&state.db.pool)
        .await?;
    Ok(row
        .map(|row| row.try_get::<Option<String>, _>("email"))
        .transpose()?
        .flatten())
}

async fn account_of(state: &AppState, user: &AuthUser) -> Result<Account, ApiError> {
    let row = sqlx::query("SELECT email FROM admin_users WHERE id = $1")
        .bind(&user.user_id)
        .fetch_one(&state.db.pool)
        .await?;
    Ok(Account {
        username: user.username.clone(),
        email: row.try_get("email")?,
    })
}

#[utoipa::path(
    get,
    tag = "Account",
    operation_id = "get_account",
    summary = "Get the administrator's account",
    security(("session" = [])),
    path = "/api/v1/account",
    responses(
        (status = 200, description = "The signed-in administrator", body = Account),
        (status = 401, description = "Not signed in", body = ErrorBody),
    )
)]
async fn get_account(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Account>, ApiError> {
    Ok(Json(account_of(&state, &user).await?))
}

#[utoipa::path(
    put,
    tag = "Account",
    operation_id = "update_account",
    summary = "Change the administrator's email",
    description = "The address is where email channels send alerts unless a channel names its own.",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/account",
    request_body = AccountInput,
    responses(
        (status = 200, description = "Saved", body = Account),
        (status = 401, description = "Not signed in", body = ErrorBody),
        (status = 422, description = "The address is not valid", body = AccountErrors),
    )
)]
async fn update_account(
    State(state): State<AppState>,
    user: AuthUser,
    Json(input): Json<AccountInput>,
) -> Result<Json<Account>, AccountFailure> {
    let email = input
        .email
        .as_deref()
        .map(str::trim)
        .filter(|email| !email.is_empty());
    if let Some(email) = email
        && email.parse::<lettre::Address>().is_err()
    {
        return Err(AccountFailure::Email);
    }
    sqlx::query("UPDATE admin_users SET email = $1 WHERE id = $2")
        .bind(email)
        .bind(&user.user_id)
        .execute(&state.db.pool)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(account_of(&state, &user).await?))
}

#[utoipa::path(
    post,
    tag = "Account",
    operation_id = "change_password",
    summary = "Change the administrator's password",
    description = "Every other session is signed out; the current one stays. A wrong current \
        password answers 422, not 401, so the interface does not take it for an expired session.",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/account/password",
    request_body = PasswordChange,
    responses(
        (status = 204, description = "The password is changed"),
        (status = 401, description = "Not signed in", body = ErrorBody),
        (status = 422, description = "`wrong_password` or `weak_password`", body = ErrorBody),
        (status = 429, description = "Too many attempts", body = ErrorBody),
    )
)]
async fn change_password(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    user: AuthUser,
    jar: CookieJar,
    Json(input): Json<PasswordChange>,
) -> Result<StatusCode, AccountFailure> {
    if let Err(message) = check_password(&input.new_password) {
        return Err(AccountFailure::Rejected("weak_password", message));
    }
    if input.current_password.len() > MAX_SUBMITTED_PASSWORD_BYTES {
        return Err(AccountFailure::Rejected("wrong_password", WRONG_PASSWORD));
    }
    let client = client_ip(&state.config, peer, &headers);
    state.auth.limits.check_login(client, &user.username)?;

    let row = sqlx::query("SELECT password_hash FROM admin_users WHERE id = $1")
        .bind(&user.user_id)
        .fetch_one(&state.db.pool)
        .await
        .map_err(ApiError::from)?;
    let current_hash: String = row.try_get("password_hash").map_err(ApiError::from)?;
    let current = input.current_password;
    let correct = match state
        .auth
        .argon
        .run(move || verify_password(&current, &current_hash))
        .await
    {
        Ok(correct) => correct,
        Err(busy) => {
            state.auth.limits.login_released(client, &user.username);
            return Err(busy.into());
        }
    };
    if !correct {
        return Err(AccountFailure::Rejected("wrong_password", WRONG_PASSWORD));
    }
    state.auth.limits.login_succeeded(client, &user.username);

    let new = input.new_password;
    let new_hash = state.auth.argon.run(move || hash_password(&new)).await??;
    let keep = jar
        .get(SESSION_COOKIE)
        .map(|cookie| session_hash(cookie.value()))
        .unwrap_or_default();
    let mut tx = state.db.pool.begin().await.map_err(ApiError::from)?;
    sqlx::query("UPDATE admin_users SET password_hash = $1 WHERE id = $2")
        .bind(new_hash)
        .bind(&user.user_id)
        .execute(&mut *tx)
        .await
        .map_err(ApiError::from)?;
    sqlx::query("DELETE FROM sessions WHERE user_id = $1 AND id_hash <> $2")
        .bind(&user.user_id)
        .bind(keep)
        .execute(&mut *tx)
        .await
        .map_err(ApiError::from)?;
    tx.commit().await.map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_account, update_account))
        .routes(routes!(change_password))
}
