use std::io::BufRead;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use lodestore_agent::update::switch::Systemd;
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
        #[arg(long = "previous-agent-id", env = "LODESTORE_PREVIOUS_AGENT_ID")]
        previous_agent_id: Option<String>,
    },
    Run {
        #[command(flatten)]
        dirs: DirArgs,
    },
    Version,
    UpdateRollback {
        #[command(flatten)]
        dirs: DirArgs,
        #[arg(long)]
        exe: PathBuf,
    },
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
            previous_agent_id,
        } => {
            let args = EnrollArgs {
                token: read_token()?,
                paths: dirs.paths(),
                hostname,
                net: NetOptions {
                    ca_bundle,
                    ..NetOptions::from_env()
                },
            };
            let enrolled = match previous_agent_id.filter(|id| !id.is_empty()) {
                Some(id) => lodestore_agent::enroll_with_previous(args, &id).await?,
                None => lodestore_agent::enroll(args).await?,
            };
            println!("registered as {}", enrolled.agent_id);
            Ok(())
        }
        Command::UpdateRollback { dirs, exe } => {
            let paths = dirs.paths();
            let rolled = lodestore_agent::update::guard::rollback(&paths, &exe, &Systemd, |p| {
                format!(
                    "The new version did not connect within 2 min. Running {} again.",
                    p.previous_version
                )
            })?;
            if rolled {
                println!("rolled back");
            }
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

const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
}

/// A blocking thread stuck in a hung FUSE read cannot be stopped; waiting for
/// it would keep a stopping agent alive until systemd kills it.
fn finish(runtime: tokio::runtime::Runtime, grace: Duration) {
    runtime.shutdown_timeout(grace);
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let cli = Cli::parse();
    let runtime = match runtime() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(execute(cli.command));
    finish(runtime, SHUTDOWN_GRACE);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(e.exit_code())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{finish, runtime};

    #[test]
    fn runtime_shutdown_does_not_wait_for_a_hung_blocking_task() {
        let runtime = runtime().unwrap();
        runtime.spawn_blocking(|| std::thread::sleep(Duration::from_secs(30)));
        let started = Instant::now();
        finish(runtime, Duration::from_millis(200));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
