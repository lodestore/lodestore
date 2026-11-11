use std::borrow::Cow;

use axum::Json;
use axum::http::header::RETRY_AFTER;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("authentication required")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("The token needs the {0} scope.")]
    MissingScope(&'static str),
    #[error("The token needs the {0} scope to {1}.")]
    MissingScopeFor(&'static str, Cow<'static, str>),
    #[error("Enter your password again to continue.")]
    StepUpRequired,
    #[error("CSRF check failed")]
    Csrf,
    #[error("setup required")]
    SetupRequired,
    #[error("Not available in the demo.")]
    DemoMode,
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
    #[error("{message}")]
    TooManyRuns { message: String, retry_after_s: u64 },
    #[error("{0}")]
    Backend(String),
    #[error("{0}")]
    Unavailable(String),
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
            ApiError::MissingScope(_) | ApiError::MissingScopeFor(..) => {
                (StatusCode::FORBIDDEN, "forbidden")
            }
            ApiError::StepUpRequired => (StatusCode::FORBIDDEN, "step_up_required"),
            ApiError::Csrf => (StatusCode::FORBIDDEN, "csrf"),
            ApiError::SetupRequired => (StatusCode::CONFLICT, "setup_required"),
            ApiError::DemoMode => (StatusCode::FORBIDDEN, "demo_mode"),
            ApiError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            ApiError::Validation(_) => (StatusCode::UNPROCESSABLE_ENTITY, "validation"),
            ApiError::BadRequest(code, _) => (StatusCode::BAD_REQUEST, code),
            ApiError::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            ApiError::PolicyConflict(_) => (StatusCode::CONFLICT, "policy_conflict"),
            ApiError::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            ApiError::TooManyRuns { .. } => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            ApiError::Backend(_) => (StatusCode::BAD_GATEWAY, "backend"),
            ApiError::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "repository_unavailable"),
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
        let retry_after = match &self {
            ApiError::TooManyRuns { retry_after_s, .. } => Some(*retry_after_s),
            _ => None,
        };
        let message = match &self {
            ApiError::Internal(e) => {
                tracing::error!(error = %e, "internal error");
                "internal error".to_string()
            }
            other => other.to_string(),
        };
        let mut response = (
            status,
            Json(ErrorBody {
                code: code.to_string(),
                message,
            }),
        )
            .into_response();
        if let Some(secs) = retry_after {
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from(secs));
        }
        response
    }
}
