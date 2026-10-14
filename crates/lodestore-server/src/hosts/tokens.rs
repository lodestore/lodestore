use axum::Json;
use axum::extract::{Path, State};
use data_encoding::BASE64URL_NOPAD;
use lodestore_format::crypto::Entropy;
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::Row;
use utoipa::ToSchema;

use crate::auth::AuthUser;
use crate::db::{Db, now_ms};
use crate::error::{ApiError, ErrorBody};
use crate::hosts::HostStatus;
use crate::hosts::labels::Label;
use crate::state::AppState;

pub const TOKEN_PREFIX: &str = "lode1.";

#[derive(Debug)]
pub struct IssuedToken {
    pub id: String,
    pub token: String,
    pub expires_at: i64,
}

#[derive(Debug)]
pub struct Consumed {
    pub repository_id: String,
    pub labels_json: String,
}

pub fn hash_secret(secret: &[u8]) -> Vec<u8> {
    Sha256::digest(secret).to_vec()
}

pub fn decode_secret(encoded: &str) -> Option<[u8; 32]> {
    BASE64URL_NOPAD
        .decode(encoded.as_bytes())
        .ok()?
        .try_into()
        .ok()
}

pub async fn issue(
    state: &AppState,
    repository_id: &str,
    labels: &[Label],
) -> Result<IssuedToken, ApiError> {
    let secret = Entropy::os().bytes::<32>();
    let now = now_ms();
    let expires_at = now + state.config.join_token_ttl_secs * 1000;
    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO join_tokens (id, token_hash, repository_id, labels_json, created_at, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(&id)
    .bind(hash_secret(&secret))
    .bind(repository_id)
    .bind(serde_json::to_string(labels).map_err(|e| ApiError::Internal(e.into()))?)
    .bind(now)
    .bind(expires_at)
    .execute(&state.db.pool)
    .await?;
    let payload = json!({
        "s": BASE64URL_NOPAD.encode(&secret),
        "u": state.config.public_url,
        "g": state.config.grpc.public_address,
        "f": state.pki.trust_pin(),
    });
    Ok(IssuedToken {
        id,
        token: format!(
            "{TOKEN_PREFIX}{}",
            BASE64URL_NOPAD.encode(payload.to_string().as_bytes())
        ),
        expires_at,
    })
}

pub async fn peek(db: &Db, token_hash: &[u8]) -> Result<Option<String>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT repository_id FROM join_tokens WHERE token_hash = $1 AND used_at IS NULL AND expires_at > $2",
    )
    .bind(token_hash.to_vec())
    .bind(now_ms())
    .fetch_optional(&db.pool)
    .await?;
    row.map(|r| r.try_get("repository_id")).transpose()
}

pub async fn consume(
    conn: &mut sqlx::AnyConnection,
    token_hash: &[u8],
    agent_id: &str,
) -> Result<Option<Consumed>, sqlx::Error> {
    let row = sqlx::query(
        "UPDATE join_tokens SET used_at = $1, used_by_agent_id = $2 \
         WHERE token_hash = $3 AND used_at IS NULL AND expires_at > $1 \
         RETURNING repository_id, labels_json",
    )
    .bind(now_ms())
    .bind(agent_id)
    .bind(token_hash.to_vec())
    .fetch_optional(conn)
    .await?;
    row.map(|r| {
        Ok(Consumed {
            repository_id: r.try_get("repository_id")?,
            labels_json: r.try_get("labels_json")?,
        })
    })
    .transpose()
}

#[derive(Serialize, ToSchema, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TokenStatus {
    Pending,
    Used,
    Expired,
}

#[derive(Serialize, ToSchema)]
pub struct TokenHost {
    pub id: String,
    pub hostname: String,
    pub version: String,
    pub status: HostStatus,
}

#[derive(Serialize, ToSchema)]
pub struct JoinTokenStatus {
    pub status: TokenStatus,
    pub expires_at: i64,
    pub server_time: i64,
    pub host: Option<TokenHost>,
}

#[utoipa::path(
    get,
    path = "/api/v1/join-tokens/{id}",
    params(("id" = String, Path, description = "Join token id")),
    responses(
        (status = 200, description = "Token status and the host that used it", body = JoinTokenStatus),
        (status = 404, description = "Unknown token", body = ErrorBody),
    )
)]
pub async fn get_token(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<JoinTokenStatus>, ApiError> {
    let row = sqlx::query(
        "SELECT t.expires_at, t.used_at, a.id AS agent_id, a.hostname, a.version, a.status \
         FROM join_tokens t LEFT JOIN agents a ON a.id = t.used_by_agent_id WHERE t.id = $1",
    )
    .bind(&id)
    .fetch_optional(&state.db.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("join token not found".into()))?;
    let now = now_ms();
    let expires_at: i64 = row.try_get("expires_at")?;
    let used_at: Option<i64> = row.try_get("used_at")?;
    let status = if used_at.is_some() {
        TokenStatus::Used
    } else if expires_at <= now {
        TokenStatus::Expired
    } else {
        TokenStatus::Pending
    };
    let host = match row.try_get::<Option<String>, _>("agent_id")? {
        Some(agent_id) => Some(TokenHost {
            status: if row.try_get::<String, _>("status")? == "revoked" {
                HostStatus::Revoked
            } else if state.registry.is_online(&agent_id) {
                HostStatus::Online
            } else {
                HostStatus::Offline
            },
            hostname: row.try_get("hostname")?,
            version: row.try_get("version")?,
            id: agent_id,
        }),
        None => None,
    };
    Ok(Json(JoinTokenStatus {
        status,
        expires_at,
        server_time: now,
        host,
    }))
}
