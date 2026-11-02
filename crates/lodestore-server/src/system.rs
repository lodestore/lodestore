use axum::Json;
use axum::extract::State;
use serde::Serialize;
use sqlx::Row;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::{AuthUser, admin_count};
use crate::error::{ApiError, ErrorBody};
use crate::install;
use crate::state::AppState;

#[derive(Serialize, ToSchema)]
pub struct ServerStatus {
    pub setup_required: bool,
    pub demo_mode: bool,
}

#[derive(Serialize, ToSchema)]
pub struct ServerInfo {
    pub version: String,
    pub unlock_key_source: String,
    pub public_url: String,
    pub grpc_public_address: String,
    pub agent_arches: Vec<String>,
    pub demo_mode: bool,
}

#[utoipa::path(
    get,
    tag = "Server",
    summary = "Get server status",
    security(()),
    path = "/api/v1/server/status",
    responses((status = 200, description = "Whether the first administrator still has to be created", body = ServerStatus))
)]
async fn status(State(state): State<AppState>) -> Result<Json<ServerStatus>, ApiError> {
    Ok(Json(ServerStatus {
        setup_required: admin_count(&state).await? == 0,
        demo_mode: state.config.demo_mode,
    }))
}

#[utoipa::path(
    get,
    tag = "Server",
    summary = "Get server information",
    path = "/api/v1/server/info",
    responses(
        (status = 200, description = "Server version and connection details", body = ServerInfo),
        (status = 401, description = "Not signed in", body = ErrorBody),
    )
)]
async fn info(
    State(state): State<AppState>,
    _user: AuthUser,
) -> Result<Json<ServerInfo>, ApiError> {
    let unlock_key_source: String =
        sqlx::query("SELECT value FROM settings WHERE key = 'unlock_key_source'")
            .fetch_one(&state.db.pool)
            .await?
            .try_get("value")?;
    Ok(Json(ServerInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        unlock_key_source,
        public_url: state.config.public_url.clone(),
        grpc_public_address: state.config.grpc.public_address.clone(),
        agent_arches: install::available_arches(&state.config)
            .into_iter()
            .map(str::to_string)
            .collect(),
        demo_mode: state.config.demo_mode,
    }))
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(status))
        .routes(routes!(info))
}
