use std::marker::PhantomData;

use axum::Json;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use data_encoding::BASE64URL_NOPAD;
use lodestore_format::crypto::Entropy;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::any::AnyRow;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::AuthUser;
use crate::db::now_ms;
use crate::error::{ApiError, ErrorBody};
use crate::hosts::tokens::hash_secret;
use crate::state::AppState;

pub const TOKEN_PREFIX: &str = "lsk_";
const SHOWN_PREFIX_CHARS: usize = 10;
const MAX_NAME_CHARS: usize = 64;
const LAST_USED_TOUCH_MS: i64 = 60_000;
const DAY_MS: i64 = 86_400_000;
const EXPIRY_CHOICES_DAYS: [u32; 3] = [30, 90, 365];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum Scope {
    #[serde(rename = "hosts:read")]
    HostsRead,
    #[serde(rename = "hosts:write")]
    HostsWrite,
    #[serde(rename = "alerts:write")]
    AlertsWrite,
    #[serde(rename = "policies:read")]
    PoliciesRead,
    #[serde(rename = "policies:write")]
    PoliciesWrite,
    #[serde(rename = "jobs:read")]
    JobsRead,
    #[serde(rename = "jobs:run")]
    JobsRun,
    #[serde(rename = "metrics:read")]
    MetricsRead,
    #[serde(rename = "snapshots:read")]
    SnapshotsRead,
    #[serde(rename = "snapshots:delete")]
    SnapshotsDelete,
    #[serde(rename = "restore")]
    Restore,
}

impl Scope {
    pub const ALL: [Scope; 11] = [
        Scope::HostsRead,
        Scope::HostsWrite,
        Scope::AlertsWrite,
        Scope::PoliciesRead,
        Scope::PoliciesWrite,
        Scope::JobsRead,
        Scope::JobsRun,
        Scope::MetricsRead,
        Scope::SnapshotsRead,
        Scope::SnapshotsDelete,
        Scope::Restore,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::HostsRead => "hosts:read",
            Scope::HostsWrite => "hosts:write",
            Scope::AlertsWrite => "alerts:write",
            Scope::PoliciesRead => "policies:read",
            Scope::PoliciesWrite => "policies:write",
            Scope::JobsRead => "jobs:read",
            Scope::JobsRun => "jobs:run",
            Scope::MetricsRead => "metrics:read",
            Scope::SnapshotsRead => "snapshots:read",
            Scope::SnapshotsDelete => "snapshots:delete",
            Scope::Restore => "restore",
        }
    }
}

pub trait ScopeName {
    const SCOPE: Scope;
}

macro_rules! scope_markers {
    ($($marker:ident => $scope:ident),* $(,)?) => {
        $(
            pub struct $marker;
            impl ScopeName for $marker {
                const SCOPE: Scope = Scope::$scope;
            }
        )*
    };
}

scope_markers! {
    HostsRead => HostsRead,
    HostsWrite => HostsWrite,
    AlertsWrite => AlertsWrite,
    PoliciesRead => PoliciesRead,
    PoliciesWrite => PoliciesWrite,
    JobsRead => JobsRead,
    JobsRun => JobsRun,
    MetricsRead => MetricsRead,
    SnapshotsRead => SnapshotsRead,
    SnapshotsDelete => SnapshotsDelete,
    RestoreScope => Restore,
}

#[derive(Debug, Clone)]
pub enum Principal {
    Session(AuthUser),
    Token {
        id: String,
        name: String,
        scopes: Vec<Scope>,
    },
}

pub struct Scoped<S: ScopeName>(Principal, PhantomData<S>);

impl<S: ScopeName> Scoped<S> {
    pub fn principal(&self) -> &Principal {
        &self.0
    }

    pub fn require_also(&self, scope: Scope, purpose: &'static str) -> Result<(), ApiError> {
        match &self.0 {
            Principal::Token { scopes, .. } if !scopes.contains(&scope) => {
                Err(ApiError::MissingScopeFor(scope.as_str(), purpose))
            }
            _ => Ok(()),
        }
    }

    pub fn actor(&self) -> String {
        match &self.0 {
            Principal::Session(user) => user.username.clone(),
            Principal::Token { name, .. } => format!("token:{name}"),
        }
    }
}

async fn verify_token(
    state: &AppState,
    secret: &str,
    needed: Scope,
) -> Result<Principal, ApiError> {
    if !secret.starts_with(TOKEN_PREFIX) {
        return Err(ApiError::Unauthorized);
    }
    let now = now_ms();
    let row = sqlx::query(
        "SELECT id, name, scopes_json, last_used_at FROM api_tokens \
         WHERE token_hash = $1 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > $2)",
    )
    .bind(hash_secret(secret.as_bytes()))
    .bind(now)
    .fetch_optional(&state.db.pool)
    .await?
    .ok_or(ApiError::Unauthorized)?;
    let id: String = row.try_get("id")?;
    let stored: Vec<serde_json::Value> =
        serde_json::from_str(&row.try_get::<String, _>("scopes_json")?)
            .map_err(|e| ApiError::Internal(e.into()))?;
    let scopes: Vec<Scope> = stored
        .into_iter()
        .filter_map(|s| serde_json::from_value(s).ok())
        .collect();
    if !scopes.contains(&needed) {
        return Err(ApiError::MissingScope(needed.as_str()));
    }
    let last_used_at: Option<i64> = row.try_get("last_used_at")?;
    if last_used_at.is_none_or(|t| now - t >= LAST_USED_TOUCH_MS)
        && let Err(error) = sqlx::query("UPDATE api_tokens SET last_used_at = $1 WHERE id = $2")
            .bind(now)
            .bind(&id)
            .execute(&state.db.pool)
            .await
    {
        tracing::warn!(%error, "recording the last use of an API token failed");
    }
    Ok(Principal::Token {
        id,
        name: row.try_get("name")?,
        scopes,
    })
}

impl<S: ScopeName> FromRequestParts<AppState> for Scoped<S> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let principal = match super::bearer_secret(&parts.headers) {
            Some(secret) => verify_token(state, secret, S::SCOPE).await?,
            None => Principal::Session(AuthUser::from_request_parts(parts, state).await?),
        };
        Ok(Scoped(principal, PhantomData))
    }
}

#[derive(Serialize, ToSchema)]
pub struct ApiTokenItem {
    pub id: String,
    pub name: String,
    pub prefix: String,
    pub scopes: Vec<Scope>,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub last_used_at: Option<i64>,
    pub revoked_at: Option<i64>,
    pub created_by: String,
}

#[derive(Serialize, ToSchema)]
pub struct CreatedToken {
    pub token: String,
    pub item: ApiTokenItem,
}

#[derive(Deserialize, ToSchema)]
pub struct NewToken {
    pub name: String,
    pub scopes: Vec<Scope>,
    #[schema(minimum = 30, maximum = 365)]
    pub expires_in_days: Option<u32>,
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum TokenState {
    Active,
    Revoked,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TokensQuery {
    #[param(inline)]
    state: Option<TokenState>,
}

const TOKEN_SELECT: &str = "SELECT id, name, prefix, scopes_json, created_by, created_at, \
    expires_at, last_used_at, revoked_at FROM api_tokens";

fn item_from_row(row: &AnyRow) -> Result<ApiTokenItem, ApiError> {
    Ok(ApiTokenItem {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        prefix: row.try_get("prefix")?,
        scopes: serde_json::from_str(&row.try_get::<String, _>("scopes_json")?)
            .map_err(|e| ApiError::Internal(e.into()))?,
        created_at: row.try_get("created_at")?,
        expires_at: row.try_get("expires_at")?,
        last_used_at: row.try_get("last_used_at")?,
        revoked_at: row.try_get("revoked_at")?,
        created_by: row.try_get("created_by")?,
    })
}

#[utoipa::path(
    get,
    tag = "Tokens",
    summary = "List API tokens",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/tokens",
    params(TokensQuery),
    responses(
        (status = 200, description = "Tokens, newest first; the secret is never shown again", body = Vec<ApiTokenItem>),
        (status = 401, description = "Not signed in", body = ErrorBody),
    )
)]
async fn list_tokens(
    State(state): State<AppState>,
    _user: AuthUser,
    Query(query): Query<TokensQuery>,
) -> Result<Json<Vec<ApiTokenItem>>, ApiError> {
    let filter = match query.state {
        None => "",
        Some(TokenState::Active) => " WHERE revoked_at IS NULL",
        Some(TokenState::Revoked) => " WHERE revoked_at IS NOT NULL",
    };
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "{TOKEN_SELECT}{filter} ORDER BY created_at DESC, id DESC"
    )))
    .fetch_all(&state.db.pool)
    .await?
    .iter()
    .map(item_from_row)
    .collect::<Result<_, _>>()
    .map(Json)
}

#[utoipa::path(
    post,
    tag = "Tokens",
    summary = "Create an API token",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/tokens",
    request_body = NewToken,
    responses(
        (status = 201, description = "The token; its secret is shown only in this response", body = CreatedToken),
        (status = 422, description = "Invalid name, scopes or expiry", body = ErrorBody),
    )
)]
async fn create_token(
    State(state): State<AppState>,
    user: AuthUser,
    Json(input): Json<NewToken>,
) -> Result<(StatusCode, Json<CreatedToken>), ApiError> {
    let name = input.name.trim().to_owned();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err(ApiError::Validation("Enter a name for the token.".into()));
    }
    let mut scopes = input.scopes;
    scopes.sort_by_key(|s| Scope::ALL.iter().position(|a| a == s));
    scopes.dedup();
    if scopes.is_empty() {
        return Err(ApiError::Validation(
            "Choose at least one permission.".into(),
        ));
    }
    let now = now_ms();
    let expires_at = match input.expires_in_days {
        None => None,
        Some(days) if EXPIRY_CHOICES_DAYS.contains(&days) => Some(now + i64::from(days) * DAY_MS),
        Some(_) => {
            return Err(ApiError::Validation(
                "Choose 30 days, 90 days, 1 year or no expiry.".into(),
            ));
        }
    };
    let token = format!(
        "{TOKEN_PREFIX}{}",
        BASE64URL_NOPAD.encode(&Entropy::os().bytes::<32>())
    );
    let item = ApiTokenItem {
        id: uuid::Uuid::now_v7().to_string(),
        name,
        prefix: token.chars().take(SHOWN_PREFIX_CHARS).collect(),
        scopes,
        created_at: now,
        expires_at,
        last_used_at: None,
        revoked_at: None,
        created_by: user.username,
    };
    sqlx::query(
        "INSERT INTO api_tokens (id, name, prefix, token_hash, scopes_json, created_by, \
         created_at, expires_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(&item.id)
    .bind(&item.name)
    .bind(&item.prefix)
    .bind(hash_secret(token.as_bytes()))
    .bind(serde_json::to_string(&item.scopes).map_err(|e| ApiError::Internal(e.into()))?)
    .bind(&item.created_by)
    .bind(item.created_at)
    .bind(item.expires_at)
    .execute(&state.db.pool)
    .await?;
    Ok((StatusCode::CREATED, Json(CreatedToken { token, item })))
}

#[utoipa::path(
    post,
    tag = "Tokens",
    summary = "Revoke an API token",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/tokens/{id}/revoke",
    params(("id" = String, Path, description = "Token id")),
    responses(
        (status = 204, description = "The token no longer works; revoking it again changes nothing"),
        (status = 404, description = "Unknown token", body = ErrorBody),
    )
)]
async fn revoke_token(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let revoked =
        sqlx::query("UPDATE api_tokens SET revoked_at = $1 WHERE id = $2 AND revoked_at IS NULL")
            .bind(now_ms())
            .bind(&id)
            .execute(&state.db.pool)
            .await?
            .rows_affected();
    if revoked == 0
        && sqlx::query("SELECT 1 FROM api_tokens WHERE id = $1")
            .bind(&id)
            .fetch_optional(&state.db.pool)
            .await?
            .is_none()
    {
        return Err(ApiError::NotFound("token not found".into()));
    }
    Ok(StatusCode::NO_CONTENT)
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_tokens, create_token))
        .routes(routes!(revoke_token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_scope_round_trips() {
        let scope: Scope = serde_json::from_str("\"metrics:read\"").unwrap();
        assert_eq!(scope, Scope::MetricsRead);
        assert_eq!(scope.as_str(), "metrics:read");
        assert_eq!(serde_json::to_string(&scope).unwrap(), "\"metrics:read\"");
        assert_eq!(Scope::ALL.len(), 11);
        let names: Vec<&str> = Scope::ALL.iter().map(|s| s.as_str()).collect();
        let at = names.iter().position(|n| *n == "metrics:read").unwrap();
        assert_eq!(names[at - 1], "jobs:run");
        assert_eq!(names[at + 1], "snapshots:read");
        let alerts = names.iter().position(|n| *n == "alerts:write").unwrap();
        assert_eq!(names[alerts - 1], "hosts:write");
        for scope in Scope::ALL {
            let parsed: Scope = serde_json::from_str(&format!("\"{}\"", scope.as_str())).unwrap();
            assert_eq!(parsed, scope);
        }
    }
}
