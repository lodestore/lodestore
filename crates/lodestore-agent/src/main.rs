use std::io::BufRead;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use lodestore_agent::{AgentError, EnrollArgs, NetOptions, Paths, RunArgs};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "lodestore-agent", about = "Lodestore host agent")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct DirArgs {
    #[arg(long, default_value = "/etc/lodestore")]
    config_dir: PathBuf,
    #[arg(long, default_value = "/var/lib/lodestore")]
    state_dir: PathBuf,
}

impl DirArgs {
    fn paths(self) -> Paths {
        Paths {
            config_dir: self.config_dir,
            state_dir: self.state_dir,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    Enroll {
        #[command(flatten)]
        dirs: DirArgs,
        #[arg(long)]
        hostname: Option<String>,
        #[arg(long = "ca-bundle")]
        ca_bundle: Option<PathBuf>,
    },
    Run {
        #[command(flatten)]
        dirs: DirArgs,
    },
    Version,
}

fn read_token() -> Result<String, AgentError> {
    if let Ok(token) = std::env::var("LODESTORE_JOIN_TOKEN") {
        return Ok(token);
    }
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    if line.trim().is_empty() {
        return Err(AgentError::InvalidToken(
            "set LODESTORE_JOIN_TOKEN or pass the token on the first line of stdin".into(),
        ));
    }
    Ok(line)
}

async fn shutdown_on_signal(token: CancellationToken) {
    use tokio::signal::unix::{SignalKind, signal};
    let Ok(mut term) = signal(SignalKind::terminate()) else {
        return;
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    token.cancel();
}

async fn execute(command: Command) -> Result<(), AgentError> {
    match command {
        Command::Version => {
            println!("lodestore-agent {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Enroll {
            dirs,
            hostname,
            ca_bundle,
        } => {
            let enrolled = lodestore_agent::enroll(EnrollArgs {
                token: read_token()?,
                paths: dirs.paths(),
                hostname,
                net: NetOptions {
                    ca_bundle,
                    ..NetOptions::from_env()
                },
            })
            .await?;
            println!("registered as {}", enrolled.agent_id);
            Ok(())
        }
        Command::Run { dirs } => {
            let shutdown = CancellationToken::new();
            tokio::spawn(shutdown_on_signal(shutdown.clone()));
            lodestore_agent::run(RunArgs {
                paths: dirs.paths(),
                net: NetOptions::from_env(),
                heartbeat: Duration::from_secs(15),
                shutdown,
            })
            .await
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let cli = Cli::parse();
    match execute(cli.command).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(e.exit_code())
        }
    }
}
