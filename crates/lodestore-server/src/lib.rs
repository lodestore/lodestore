//! Lodestore server: control plane and catalog.

pub mod alerts;
pub mod api;
pub mod assets;
pub mod auth;
pub mod browse;
pub mod catalog;
pub mod config;
pub mod db;
pub mod download;
pub mod error;
pub mod fleet;
pub mod grpc;
pub mod hosts;
pub mod install;
pub mod jobs;
pub mod maintenance;
pub mod metrics;
pub mod notify;
pub mod pki;
pub mod policies;
pub mod repos;
pub mod schedule;
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
    pub metrics_addr: Option<SocketAddr>,
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
    let started_at_ms = db::now_ms();
    let state = AppState {
        started_at_ms,
        db,
        config: Arc::new(config),
        sealer,
        pki,
        registry: Registry::default(),
        auth: auth::AuthState::default(),
        repos: repos::ReposState::default(),
        catalog: catalog::CatalogJobs::default(),
        readers: browse::tree::RepoReaders::default(),
        gc: maintenance::gc::GcRuns::default(),
        checks: maintenance::check::CheckGate::default(),
    };
    auth::bootstrap_admin(&state).await?;
    notify::sync_config_channels(&state).await?;
    download::interrupt_running(&state).await?;
    maintenance::gc::interrupt_dry_runs(&state).await?;
    maintenance::check::interrupt_running(&state).await?;
    catalog::resume_unsynced(&state).await;

    let rest_addr = listeners.rest.local_addr()?;
    let grpc_addr = listeners.grpc.local_addr()?;
    let stop = CancellationToken::new();
    let metrics_listener = match state.config.metrics_listen {
        Some(addr) => Some(
            TcpListener::bind(addr)
                .await
                .map_err(|error| anyhow::anyhow!("binding metrics_listen {addr}: {error}"))?,
        ),
        None => None,
    };
    let metrics_addr = metrics_listener
        .as_ref()
        .map(TcpListener::local_addr)
        .transpose()?;

    let router = api::router(state.clone()).merge(metrics::router(state.clone()));
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
    let mut tasks = vec![rest, grpc, janitor, catalog];
    if state.config.schedule_tick_secs > 0 {
        tasks.push(tokio::spawn(schedule::run(state.clone(), stop.clone())));
        tasks.push(tokio::spawn(maintenance::gc::run(
            state.clone(),
            stop.clone(),
        )));
        tasks.push(tokio::spawn(maintenance::check::run(
            state.clone(),
            stop.clone(),
        )));
        tasks.push(tokio::spawn(alerts::run(state.clone(), stop.clone())));
        tasks.push(tokio::spawn(notify::run(state.clone(), stop.clone())));
    }

    if let Some(listener) = metrics_listener {
        let router = metrics::open_router(state.clone());
        let metrics_stop = stop.clone();
        tasks.push(tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(metrics_stop.cancelled_owned())
                .await?;
            Ok(())
        }));
    }

    tracing::info!(%rest_addr, %grpc_addr, ?metrics_addr, "lodestore server started");
    Ok(ServerHandle {
        rest_addr,
        grpc_addr,
        metrics_addr,
        state,
        stop,
        tasks,
    })
}
