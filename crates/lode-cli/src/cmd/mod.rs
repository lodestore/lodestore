mod backup;
mod backup_agent;
mod dump;
mod key;
mod ls;
mod repo;
mod restore;
mod snapshots;

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result};

use crate::cli::{Cli, Command, UsageError};
use crate::open::resolve_repository;
use crate::output::Output;
use crate::profile::load_profile;

/// Returns the process exit code of a command that did not fail: 0, 1 for a
/// restore that left entries out, or 3 for an incomplete snapshot.
/// The format library's futures are not `Send`, so commands run on the
/// calling thread.
pub fn run(cli: Cli) -> Result<u8> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?;
    let Cli {
        repo,
        profile,
        json,
        mut secret,
        command,
    } = cli;
    let profile = profile.as_deref().map(load_profile).transpose()?;
    if let Some(profile) = &profile {
        secret
            .profile_password_file
            .clone_from(&profile.password_file);
        secret
            .profile_password_command
            .clone_from(&profile.password_command);
    }
    let out = Output { json };
    if let Command::Backup { flags, .. } = &command
        && flags.stdin
    {
        let repo_configured = repo.is_some()
            || std::env::var_os("LODE_REPOSITORY").is_some_and(|v| !v.is_empty())
            || profile.as_ref().is_some_and(|p| p.repository.is_some());
        let default_socket_exists = Path::new(backup_agent::DEFAULT_SOCKET).exists();
        if use_agent(
            flags.agent,
            flags.agent_socket.is_some(),
            repo_configured,
            default_socket_exists,
        )? {
            return backup_agent::run(flags, out);
        }
    }
    let url = resolve_repository(repo, profile.as_ref())?;
    rt.block_on(async {
        match command {
            Command::Repo { command } => repo::run(&url, &secret, out, command).await,
            Command::Backup { paths, flags } => {
                backup::run(&url, &secret, out, &paths, flags).await
            }
            Command::Key { command } => key::run(&url, &secret, out, command).await.map(|()| 0),
            Command::Snapshots => snapshots::run(&url, &secret, out).await.map(|()| 0),
            Command::Ls { snapshot, long } => ls::run(&url, &secret, out, &snapshot, long)
                .await
                .map(|()| 0),
            Command::Dump { snapshot, path } => {
                dump::run(&url, &secret, &snapshot, &path).await.map(|()| 0)
            }
            Command::Restore {
                snapshot,
                target,
                flags,
            } => restore::run(&url, &secret, out, &snapshot, &target, flags).await,
        }
    })
}

/// Whether `lode backup --stdin` hands the data to the local agent (true) or
/// writes to a repository itself (false).
pub(crate) fn use_agent(
    agent_flag: bool,
    socket_flag: bool,
    repo_configured: bool,
    default_socket_exists: bool,
) -> Result<bool, UsageError> {
    let asked = agent_flag || socket_flag;
    if asked && repo_configured {
        return Err(UsageError(
            "--agent and --agent-socket cannot be combined with a repository from -r/--repo, \
LODE_REPOSITORY or a profile"
                .into(),
        ));
    }
    Ok(asked || (!repo_configured && default_socket_exists))
}

/// Creates `path` with mode 0600 and refuses to touch an existing file. A
/// failed write removes the file again.
pub(crate) fn write_new_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    if let Err(err) = written {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(err).with_context(|| format!("cannot write {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::use_agent;

    #[test]
    fn use_agent_mode_selection() {
        assert!(use_agent(true, false, false, false).unwrap());
        assert!(use_agent(false, true, false, false).unwrap());
        assert!(use_agent(false, false, false, true).unwrap());
        assert!(!use_agent(false, false, false, false).unwrap());
        assert!(!use_agent(false, false, true, true).unwrap());
        assert!(use_agent(true, false, true, true).is_err());
        assert!(use_agent(false, true, true, false).is_err());
    }
}
