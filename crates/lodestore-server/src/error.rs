use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("authentication required")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("CSRF check failed")]
    Csrf,
    #[error("setup required")]
    SetupRequired,
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Validation(String),
    /// A request refused with its own error code.
    #[error("{1}")]
    BadRequest(&'static str, String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    PolicyConflict(String),
    #[error("too many requests")]
    RateLimited,
    #[error("{0}")]
    Backend(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

#[derive(Serialize, ToSchema)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

impl ApiError {
    fn parts(&self) -> (StatusCode, &'static str) {
        match self {
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            ApiError::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
            ApiError::Csrf => (StatusCode::FORBIDDEN, "csrf"),
            ApiError::SetupRequired => (StatusCode::CONFLICT, "setup_required"),
            ApiError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            ApiError::Validation(_) => (StatusCode::UNPROCESSABLE_ENTITY, "validation"),
            ApiError::BadRequest(code, _) => (StatusCode::BAD_REQUEST, code),
            ApiError::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            ApiError::PolicyConflict(_) => (StatusCode::CONFLICT, "policy_conflict"),
            ApiError::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            ApiError::Backend(_) => (StatusCode::BAD_GATEWAY, "backend"),
            ApiError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        }
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::Internal(e.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = self.parts();
        let message = match &self {
            ApiError::Internal(e) => {
                tracing::error!(error = %e, "internal error");
                "internal error".to_string()
            }
            other => other.to_string(),
        };
        (
            status,
            Json(ErrorBody {
                code: code.to_string(),
                message,
            }),
        )
            .into_response()
    }
}
