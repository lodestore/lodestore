use std::io::{BufRead, IsTerminal, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use data_encoding::HEXLOWER;
use lodestore_format::Error;
use lodestore_format::crypto::Entropy;
use lodestore_format::index::rebuild_index;
use lodestore_format::keys::RecoveryKey;
use lodestore_format::repo::{InitOptions, PendingInit};
use lodestore_format::store::{BlobError, BlobStore};
use serde::Serialize;

use crate::cli::{RepoCommand, SecretArgs, UsageError};
use crate::cmd::write_new_file;
use crate::open::{open_repository, open_store};
use crate::output::{JsonPath, Output, spinner, warn_skipped};
use crate::secrets::{Purpose, Secret, resolve_secret};

#[derive(Serialize)]
struct InitJson<'a> {
    repository_id: String,
    recovery_key_file: Option<JsonPath<'a>>,
}

#[derive(Serialize)]
struct SkippedJson<'a> {
    key: &'a str,
    reason: &'a str,
}

#[derive(Serialize)]
struct RebuildJson<'a> {
    packs_scanned: u64,
    packs_indexed: u64,
    shards_written: u64,
    packs_skipped: Vec<SkippedJson<'a>>,
}

/// Returns the process exit code: 1 when a rebuild had to skip packs.
pub async fn run(url: &str, args: &SecretArgs, out: Output, command: RepoCommand) -> Result<u8> {
    match command {
        RepoCommand::Init { recovery_key_file } => {
            init(url, args, out, recovery_key_file.as_deref())
                .await
                .map(|()| 0)
        }
        RepoCommand::RebuildIndex => rebuild(url, args, out).await,
    }
}

async fn rebuild(url: &str, args: &SecretArgs, out: Output) -> Result<u8> {
    let (store, repo) = open_repository(url, args).await?;
    let bar = if out.json { None } else { spinner() };
    let report = rebuild_index(&store, repo.read_keys(), &|done, total| {
        if let Some(bar) = &bar {
            bar.set_message(format!("reading pack headers: {done}/{total}"));
        }
    })
    .await;
    if let Some(bar) = &bar {
        bar.finish_and_clear();
    }
    let report = report.with_context(|| format!("cannot rebuild the index of {url}"))?;
    warn_skipped(&report.shards_skipped);
    warn_skipped(&report.packs_skipped);
    if out.json {
        out.emit(
            "rebuild_index",
            RebuildJson {
                packs_scanned: report.packs_scanned,
                packs_indexed: report.packs_indexed,
                shards_written: report.shards_written,
                packs_skipped: report
                    .packs_skipped
                    .iter()
                    .map(|(key, reason)| SkippedJson { key, reason })
                    .collect(),
            },
        );
    } else {
        println!(
            "scanned {} packs, indexed {}, wrote {} index shards, skipped {}",
            report.packs_scanned,
            report.packs_indexed,
            report.shards_written,
            report.packs_skipped.len()
        );
    }
    Ok(u8::from(!report.packs_skipped.is_empty()))
}

/// Nothing is written to the repository until the recovery key is safe: the
/// key is confirmed first, `config` is written last.
async fn init(
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
    let Secret::Password(password) = resolve_secret(args, Purpose::Init)? else {
        bail!("a recovery key cannot be the password of a new repository");
    };
    let store = open_store(url)?;
    PendingInit::ensure_uninitialized(&store)
        .await
        .with_context(|| format!("cannot initialize repository {url}"))?;

    let mut entropy = Entropy::os();
    let pending = PendingInit::generate(&mut entropy, InitOptions::default())?;
    confirm_recovery_key(pending.recovery_key(), recovery_key_file)?;
    let repository_id = HEXLOWER.encode(pending.read_keys().repo_id());

    if let Err(err) = pending.commit(&store, &password, &mut entropy).await {
        // The key is the only copy: keep it when `config` may be ours, as
        // after a PUT whose answer was lost.
        let config_unknown = !matches!(err, Error::AlreadyInitialized)
            && !matches!(store.get("config").await, Err(BlobError::NotFound));
        if let Some(path) = recovery_key_file.filter(|_| !config_unknown) {
            let _ = std::fs::remove_file(path);
        }
        return Err(err).with_context(|| format!("cannot initialize repository {url}"));
    }
    if out.json {
        out.emit(
            "init",
            InitJson {
                repository_id,
                recovery_key_file: recovery_key_file.map(|p| JsonPath(p.as_os_str().as_bytes())),
            },
        );
        return Ok(());
    }
    println!("repository initialized at {url}");
    if let Some(path) = recovery_key_file {
        eprintln!("recovery key saved to {}", path.display());
    }
    Ok(())
}

/// Returns only once the key is saved or acknowledged. The key goes to the new
/// 0600 file or to the terminal on stderr, never to stdout. Also used by
/// `lode key add --recovery`.
pub(crate) fn confirm_recovery_key(
    key: &RecoveryKey,
    recovery_key_file: Option<&Path>,
) -> Result<()> {
    if let Some(path) = recovery_key_file {
        return write_key_file(path, key);
    }
    if !std::io::stdin().is_terminal() {
        bail!(
            "cannot show the recovery key without a terminal: pass --recovery-key-file <PATH> to save it to a new file"
        );
    }
    let shown = key.encode();
    let mut err = std::io::stderr().lock();
    writeln!(
        err,
        "Recovery key. Save it now: without it and without the password the data cannot be restored.\n\n    {}\n",
        *shown
    )?;
    let last_group = shown.rsplit('-').next().unwrap_or_default();
    check_last_group(last_group, &mut std::io::stdin().lock(), &mut err)
}

const CONFIRM_ATTEMPTS: u32 = 3;

/// Proof that the user looked at the key: the last group, typed back.
fn check_last_group(expected: &str, input: &mut impl BufRead, out: &mut impl Write) -> Result<()> {
    for attempt in 1..=CONFIRM_ATTEMPTS {
        write!(
            out,
            "Type the last group of the key to confirm ({attempt}/{CONFIRM_ATTEMPTS}): "
        )?;
        out.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            bail!("the recovery key was not confirmed; nothing was written to the repository");
        }
        if line.trim().eq_ignore_ascii_case(expected) {
            return Ok(());
        }
        writeln!(out, "That is not the last group of the key.")?;
    }
    bail!("the recovery key was not confirmed; nothing was written to the repository")
}

fn write_key_file(path: &Path, key: &RecoveryKey) -> Result<()> {
    let mut text = key.encode().to_string();
    text.push('\n');
    write_new_file(path, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(expected: &str, typed: &str) -> Result<()> {
        check_last_group(expected, &mut typed.as_bytes(), &mut Vec::new())
    }

    #[test]
    fn gate_accepts_last_group_case_insensitively() {
        assert!(check("AB7Q", "ab7q\n").is_ok());
        assert!(check("AB7Q", "nope\n  Ab7q  \n").is_ok());
    }

    #[test]
    fn gate_gives_up_after_three_attempts() {
        assert!(check("AB7Q", "a\nb\nc\nAB7Q\n").is_err());
        assert!(check("AB7Q", "").is_err());
    }
}
