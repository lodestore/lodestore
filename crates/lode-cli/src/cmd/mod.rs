mod backup;
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

use crate::cli::{Cli, Command};
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
