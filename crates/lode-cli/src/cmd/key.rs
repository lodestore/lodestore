use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Context, Result};
use data_encoding::HEXLOWER;
use lodestore_format::crypto::Entropy;
use lodestore_format::keys::RecoveryKey;
use serde::Serialize;

use crate::cli::{KeyCommand, SecretArgs, UsageError};
use crate::cmd::repo::confirm_recovery_key;
use crate::cmd::write_new_file;
use crate::open::open_repository;
use crate::output::{JsonPath, Output};
use crate::secrets::resolve_new_password;

#[derive(Serialize)]
struct ExportedJson<'a> {
    repository_id: String,
    path: JsonPath<'a>,
}

#[derive(Serialize)]
struct KeyAddedJson<'a> {
    kind: &'a str,
    slot: String,
    recovery_key_file: Option<JsonPath<'a>>,
}

#[derive(Serialize)]
struct PasswdJson {
    new_slot: String,
    old_slot_removed: bool,
}

#[derive(Serialize)]
struct WarningJson<'a> {
    message: &'a str,
}

pub async fn run(url: &str, args: &SecretArgs, out: Output, command: KeyCommand) -> Result<()> {
    match command {
        KeyCommand::ExportWriter { output } => export_writer(url, args, out, &output).await,
        KeyCommand::Add {
            recovery: false,
            new_password_file,
            ..
        } => add_password(url, args, out, new_password_file.as_deref()).await,
        KeyCommand::Add {
            recovery: true,
            recovery_key_file,
            ..
        } => add_recovery(url, args, out, recovery_key_file.as_deref()).await,
        KeyCommand::Passwd { new_password_file } => {
            passwd(url, args, out, new_password_file.as_deref()).await
        }
    }
}

async fn export_writer(url: &str, args: &SecretArgs, out: Output, output: &Path) -> Result<()> {
    let (_store, repo) = open_repository(url, args).await?;
    let keys = repo.read_keys().write_keys(repo.config_hash());
    let text = keys.to_file_text();
    write_new_file(output, text.as_bytes())?;
    if out.json {
        out.emit(
            "writer_keys_exported",
            ExportedJson {
                repository_id: HEXLOWER.encode(repo.read_keys().repo_id()),
                path: JsonPath(output.as_os_str().as_bytes()),
            },
        );
    } else {
        println!("writer keys saved to {}", output.display());
    }
    Ok(())
}

async fn add_password(
    url: &str,
    args: &SecretArgs,
    out: Output,
    new_password_file: Option<&Path>,
) -> Result<()> {
    let (store, repo) = open_repository(url, args).await?;
    let password = resolve_new_password(new_password_file)?;
    let slot = repo
        .add_password_slot(&store, &password, &mut Entropy::os())
        .await?;
    if out.json {
        out.emit(
            "key_added",
            KeyAddedJson {
                kind: "password",
                slot,
                recovery_key_file: None,
            },
        );
    } else {
        println!("password added");
    }
    Ok(())
}

/// The key is shown or saved before the slot exists, as at init.
async fn add_recovery(
    url: &str,
    args: &SecretArgs,
    out: Output,
    recovery_key_file: Option<&Path>,
) -> Result<()> {
    if out.json && recovery_key_file.is_none() {
        return Err(UsageError(
            "--json needs --recovery-key-file <PATH>: the key is never printed to stdout".into(),
        )
        .into());
    }
    let (store, repo) = open_repository(url, args).await?;
    let mut entropy = Entropy::os();
    let key = RecoveryKey::generate(&mut entropy);
    confirm_recovery_key(&key, recovery_key_file)?;
    let slot = repo.add_recovery_slot(&store, &key, &mut entropy).await;
    let slot = match slot {
        Ok(slot) => slot,
        Err(err) => {
            if let Some(path) = recovery_key_file {
                let _ = std::fs::remove_file(path);
            }
            return Err(err).context("cannot add the recovery key");
        }
    };
    if out.json {
        out.emit(
            "key_added",
            KeyAddedJson {
                kind: "recovery",
                slot,
                recovery_key_file: recovery_key_file.map(|p| JsonPath(p.as_os_str().as_bytes())),
            },
        );
    } else {
        println!("recovery key added");
    }
    Ok(())
}

async fn passwd(
    url: &str,
    args: &SecretArgs,
    out: Output,
    new_password_file: Option<&Path>,
) -> Result<()> {
    let (store, repo) = open_repository(url, args).await?;
    let password = resolve_new_password(new_password_file)?;
    let outcome = repo
        .change_password(&store, &password, &mut Entropy::os())
        .await?;
    if out.json {
        out.emit(
            "key_passwd",
            PasswdJson {
                new_slot: outcome.new_slot,
                old_slot_removed: outcome.old_slot_removed,
            },
        );
    } else {
        println!("password changed");
    }
    if let Some(reason) = outcome.warning {
        let message = format!("old password still works: {reason}");
        if out.json {
            out.emit("warning", WarningJson { message: &message });
        } else {
            eprintln!("warning: {message}");
        }
    }
    Ok(())
}
