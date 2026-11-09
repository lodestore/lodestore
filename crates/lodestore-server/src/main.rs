#![forbid(unsafe_code)]

use anyhow::Context;
use clap::{Parser, Subcommand};
use lodestore_server::config::{Config, ConfigArgs};
use lodestore_server::{Listeners, api, serve};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "lodestore-server", version, about = "Lodestore server")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    config: ConfigArgs,
}

#[derive(Subcommand)]
enum Command {
    Serve,
    Healthcheck,
    Openapi,
}

async fn healthcheck(config: &Config) -> anyhow::Result<()> {
    let mut stream = TcpStream::connect(("127.0.0.1", config.listen.port())).await?;
    stream
        .write_all(b"GET /healthz HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .await?;
    let mut response = String::new();
    stream.read_to_string(&mut response).await?;
    anyhow::ensure!(
        response.starts_with("HTTP/1.0 200") || response.starts_with("HTTP/1.1 200"),
        "unhealthy response"
    );
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("installing the SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve) {
        Command::Openapi => {
            println!("{}", api::openapi().to_pretty_json()?);
            Ok(())
        }
        Command::Healthcheck => {
            let config = Config::from_args(cli.config)?;
            healthcheck(&config).await.context("healthcheck failed")
        }
        Command::Serve => {
            let config = Config::from_args(cli.config)?;
            let listeners = Listeners {
                rest: TcpListener::bind(config.listen)
                    .await
                    .with_context(|| format!("binding {}", config.listen))?,
                grpc: TcpListener::bind(config.grpc.listen)
                    .await
                    .with_context(|| format!("binding {}", config.grpc.listen))?,
            };
            let handle = serve(config, listeners).await?;
            shutdown_signal().await;
            tracing::info!("shutting down");
            handle.shutdown().await;
            Ok(())
        }
    }
}
