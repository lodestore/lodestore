//! Lodestore server: control plane and catalog.

pub mod api;
pub mod assets;
pub mod auth;
pub mod catalog;
pub mod config;
pub mod db;
pub mod error;
pub mod grpc;
pub mod hosts;
pub mod install;
pub mod jobs;
pub mod pki;
pub mod policies;
pub mod repos;
pub mod secrets;
pub mod state;
pub mod system;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use lodestore_proto::agent::v1::DisconnectReason;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::db::Db;
use crate::grpc::registry::Registry;
use crate::pki::Pki;
use crate::secrets::Sealer;
use crate::state::AppState;

pub struct Listeners {
    pub rest: TcpListener,
    pub grpc: TcpListener,
}

pub struct ServerHandle {
    pub rest_addr: SocketAddr,
    pub grpc_addr: SocketAddr,
    state: AppState,
    stop: CancellationToken,
    tasks: Vec<JoinHandle<anyhow::Result<()>>>,
}

impl ServerHandle {
    pub fn state(&self) -> &AppState {
        &self.state
    }

    pub async fn shutdown(self) {
        self.stop.cancel();
        self.state
            .registry
            .disconnect_all(DisconnectReason::Shutdown);
        for task in self.tasks {
            match tokio::time::timeout(Duration::from_secs(10), task).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => tracing::warn!(%error, "listener stopped with an error"),
                Ok(Err(error)) => tracing::warn!(%error, "listener task failed"),
                Err(_) => tracing::warn!("listener did not stop in time"),
            }
        }
    }
}

pub async fn serve(config: Config, listeners: Listeners) -> anyhow::Result<ServerHandle> {
    std::fs::create_dir_all(&config.data_dir)?;
    let db = Db::connect(&config.database_url).await?;
    let sealer = Sealer::load(&db, &config).await?;
    let pki = Arc::new(Pki::load_or_create(&db, &sealer, &config).await?);
    if config.grpc_public_address_defaulted {
        tracing::warn!(
            address = %config.grpc.public_address,
            "grpc.public_address is not set; agents must be able to reach this address"
        );
    }
    let state = AppState {
        db,
        config: Arc::new(config),
        sealer,
        pki,
        registry: Registry::default(),
        auth: auth::AuthState::default(),
        repos: repos::ReposState::default(),
    };
    auth::bootstrap_admin(&state).await?;

    let rest_addr = listeners.rest.local_addr()?;
    let grpc_addr = listeners.grpc.local_addr()?;
    let stop = CancellationToken::new();

    let router = api::router(state.clone());
    let rest_stop = stop.clone();
    let rest = tokio::spawn(async move {
        axum::serve(
            listeners.rest,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(rest_stop.cancelled_owned())
        .await?;
        Ok(())
    });
    let grpc = tokio::spawn(grpc::serve(state.clone(), listeners.grpc, stop.clone()));
    let janitor = tokio::spawn(auth::session_janitor(state.clone(), stop.clone()));
    let catalog = tokio::spawn(catalog::sync_loop(state.clone(), stop.clone()));

    tracing::info!(%rest_addr, %grpc_addr, "lodestore server started");
    Ok(ServerHandle {
        rest_addr,
        grpc_addr,
        state,
        stop,
        tasks: vec![rest, grpc, janitor, catalog],
    })
}
