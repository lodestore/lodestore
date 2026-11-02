use std::path::Path;

use anyhow::{Context, Result};
use lodestore_format::Error;
use lodestore_format::crypto::Entropy;
use lodestore_format::keys::WriteKeys;
use lodestore_format::locks::{
    LockId, UNMARKED_WRITER_LIMIT, close_writer_marker, put_writer_marker, writer_marker_key,
};
use lodestore_format::reader::Reader;
use lodestore_format::repo::Repository;
use lodestore_format::store::{BlobError, BlobStore, Store, open_url};
use lodestore_format::writer::{Deadline, Writer};

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

/// The writer marker this run holds in `locks/`: while it is open a cleanup
/// cycle deletes nothing. Without one, the run gets the fallback time limit,
/// counted from before the marker was attempted: from then on the writer may
/// read `locks/` and the index, which is when a cleanup cycle starts to count it.
pub enum Marker {
    Written(Store, LockId),
    Denied(Deadline),
}

impl Marker {
    pub fn unmarked_deadline(&self) -> Option<Deadline> {
        match self {
            Marker::Denied(deadline) => Some(*deadline),
            Marker::Written(..) => None,
        }
    }

    /// Closes the marker; a failure only delays cleanup, so it is a warning.
    pub async fn close(self) {
        let Marker::Written(store, id) = self else {
            return;
        };
        if let Err(e) = close_writer_marker(&store, &id).await {
            eprintln!(
                "warning: the cleanup marker {} stays open; cleanup waits for it until an admin runs lode gc unlock ({e})",
                writer_marker_key(&id)
            );
        }
    }
}

pub struct Opened {
    pub writer: Writer<Store>,
    /// `Some` when the repository was opened with a secret, so the caller can
    /// read the parent snapshot; with writer keys it is `None`.
    pub reader: Option<Reader<Store>>,
    pub marker: Marker,
}

async fn put_marker(url: &str, keys: &WriteKeys, hostname: &[u8]) -> Result<Marker> {
    let started = Deadline::starting_now(UNMARKED_WRITER_LIMIT);
    let store = open_store(url)?;
    match put_writer_marker(
        &store,
        &keys.k_idx,
        &keys.repo_id,
        hostname,
        &mut Entropy::os(),
    )
    .await
    {
        Ok(id) => Ok(Marker::Written(store, id)),
        Err(Error::Store(BlobError::Denied { .. })) => Ok(Marker::Denied(started)),
        Err(e) => Err(e).with_context(|| format!("cannot write the cleanup marker in {url}")),
    }
}

fn check_locks_readable(writer: &Writer<Store>, no_gc_check: bool) -> Result<()> {
    if !writer.gc_locks_denied() {
        return Ok(());
    }
    if no_gc_check {
        eprintln!(
            "warning: cannot read locks/ in the repository (access denied); not checking for cleanup cycles (--no-gc-check)"
        );
        return Ok(());
    }
    anyhow::bail!(
        "cannot read locks/ in the repository (access denied); a cleanup cycle could delete data this backup reuses. Update the key policy (see https://lodestore.github.io/lodestore/guides/gc/) or pass --no-gc-check if no server cleans up this repository."
    )
}

/// Writes the writer marker before anything is read from the repository, so a
/// manifest published after that is seen by the writer or held back by the
/// marker. Whatever fails afterwards closes the marker again.
pub async fn open_for_write(
    url: &str,
    args: &SecretArgs,
    hostname: &[u8],
    no_gc_check: bool,
) -> Result<Opened> {
    if let Some(path) = &args.writer_keys {
        return open_with_writer_keys(url, path, hostname, no_gc_check).await;
    }
    let (store, repo) = open_repository(url, args).await?;
    let keys = repo.read_keys().write_keys(repo.config_hash());
    let marker = put_marker(url, &keys, hostname).await?;
    let opened = async {
        let mut writer = Writer::new(store, keys, repo.config()).await?;
        check_locks_readable(&writer, no_gc_check)?;
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
        anyhow::Ok((writer, Some(reader)))
    }
    .await;
    match opened {
        Ok((writer, reader)) => Ok(Opened {
            writer,
            reader,
            marker,
        }),
        Err(e) => {
            marker.close().await;
            Err(e)
        }
    }
}

async fn open_with_writer_keys(
    url: &str,
    path: &Path,
    hostname: &[u8],
    no_gc_check: bool,
) -> Result<Opened> {
    let text = Zeroizing::new(
        std::fs::read_to_string(path)
            .with_context(|| format!("cannot read writer keys {}", path.display()))?,
    );
    let keys = WriteKeys::from_file_text(&text)
        .with_context(|| format!("cannot use writer keys {}", path.display()))?;
    let store = open_store(url)?;
    let config = match store.get("config").await {
        Ok(bytes) => bytes,
        Err(BlobError::NotFound) => return Err(Error::NotInitialized.into()),
        Err(e) => return Err(e.into()),
    };
    keys.verify_config(&config)
        .with_context(|| format!("cannot back up to {url}"))?;
    let marker = put_marker(url, &keys, hostname).await?;
    let opened = async {
        let writer = Writer::connect(store, keys)
            .await
            .with_context(|| format!("cannot back up to {url}"))?;
        check_locks_readable(&writer, no_gc_check)?;
        warn_skipped(writer.skipped_index_shards());
        anyhow::Ok(writer)
    }
    .await;
    match opened {
        Ok(writer) => Ok(Opened {
            writer,
            reader: None,
            marker,
        }),
        Err(e) => {
            marker.close().await;
            Err(e)
        }
    }
}
