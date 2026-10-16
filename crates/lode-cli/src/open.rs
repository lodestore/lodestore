use std::path::Path;

use anyhow::{Context, Result};
use lodestore_format::keys::WriteKeys;
use lodestore_format::reader::Reader;
use lodestore_format::repo::Repository;
use lodestore_format::store::{Store, open_url};
use lodestore_format::writer::Writer;

use crate::cli::{SecretArgs, UnlockError, UsageError};
use crate::output::warn_skipped;
use crate::profile::Profile;
use crate::secrets::{Purpose, Secret, resolve_secret};
use zeroize::Zeroizing;

/// `-r/--repo`, then `LODE_REPOSITORY`, then the profile's `repository`.
pub fn resolve_repository(flag: Option<String>, profile: Option<&Profile>) -> Result<String> {
    flag.or_else(|| {
        std::env::var("LODE_REPOSITORY")
            .ok()
            .filter(|v| !v.is_empty())
    })
    .or_else(|| profile.and_then(|p| p.repository.clone()))
    .ok_or_else(|| {
        UsageError(
            "no repository: pass -r/--repo <URL>, set LODE_REPOSITORY, or select a profile with a repository using --profile <NAME>"
                .into(),
        )
        .into()
    })
}

pub fn open_store(url: &str) -> Result<Store> {
    open_url(url).with_context(|| format!("cannot open repository {url}"))
}

/// Opens with a password or recovery key. Every command that reads goes
/// through here, so writer keys are refused for all of them.
pub(crate) async fn open_repository(url: &str, args: &SecretArgs) -> Result<(Store, Repository)> {
    if args.writer_keys.is_some() {
        return Err(UnlockError(
            "writer keys can only back up; reading needs a password or recovery key".into(),
        )
        .into());
    }
    let secret = resolve_secret(args, Purpose::Open)?;
    let store = open_store(url)?;
    let repo = match &secret {
        Secret::Password(password) => Repository::open(&store, password).await,
        Secret::Recovery(key) => Repository::open_recovery(&store, key).await,
    }
    .with_context(|| format!("cannot open repository {url}"))?;
    warn_skipped(repo.skipped_slots());
    Ok((store, repo))
}

pub async fn open_for_read(url: &str, args: &SecretArgs) -> Result<Reader<Store>> {
    let (store, repo) = open_repository(url, args).await?;
    Ok(Reader::open(
        store,
        repo.read_keys().duplicate(),
        repo.config(),
    )?)
}

/// The reader is `Some` when the repository was opened with a secret, so the
/// caller can read the parent snapshot; with writer keys it is `None`.
pub async fn open_for_write(
    url: &str,
    args: &SecretArgs,
) -> Result<(Writer<Store>, Option<Reader<Store>>)> {
    if let Some(path) = &args.writer_keys {
        return open_with_writer_keys(url, path).await;
    }
    let (store, repo) = open_repository(url, args).await?;
    let keys = repo.read_keys().write_keys(repo.config_hash());
    let mut writer = Writer::new(store, keys, repo.config()).await?;
    warn_skipped(writer.skipped_index_shards());
    let dropped = writer.drop_unbacked_packs().await?;
    if dropped > 0 {
        eprintln!(
            "warning: {dropped} packs named by the index are missing or have the wrong size; their blobs are uploaded again"
        );
    }
    let reader = Reader::open(
        open_store(url)?,
        repo.read_keys().duplicate(),
        repo.config(),
    )?;
    Ok((writer, Some(reader)))
}

async fn open_with_writer_keys(
    url: &str,
    path: &Path,
) -> Result<(Writer<Store>, Option<Reader<Store>>)> {
    let text = Zeroizing::new(
        std::fs::read_to_string(path)
            .with_context(|| format!("cannot read writer keys {}", path.display()))?,
    );
    let keys = WriteKeys::from_file_text(&text)
        .with_context(|| format!("cannot use writer keys {}", path.display()))?;
    let store = open_store(url)?;
    let writer = Writer::connect(store, keys)
        .await
        .with_context(|| format!("cannot back up to {url}"))?;
    warn_skipped(writer.skipped_index_shards());
    Ok((writer, None))
}
