use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lodestore_format::crypto::RepoId;
use lodestore_format::image::{ImageState, MAX_STATE_BYTES, StateRead, instance_id};
use lodestore_format::source::SourceKey;

use crate::open::cache_dir;

pub fn state_path(repo_id: &RepoId, hostname: &[u8], key: &SourceKey) -> Option<PathBuf> {
    cache_dir(repo_id).map(|dir| {
        dir.join("images").join(format!(
            "{}.json",
            data_encoding::HEXLOWER.encode(&instance_id(hostname, key))
        ))
    })
}

pub fn load(path: &Path, repo_id: &RepoId, hostname: &[u8], key: &SourceKey) -> StateRead {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return StateRead::Missing,
        Err(_) => return StateRead::Unreadable,
    };
    let mut bytes = Vec::new();
    let limit = MAX_STATE_BYTES as u64 + 1;
    if (&mut file).take(limit).read_to_end(&mut bytes).is_err() || bytes.len() > MAX_STATE_BYTES {
        return StateRead::Unreadable;
    }
    match ImageState::parse(&bytes, repo_id, hostname, key) {
        Ok(state) => StateRead::Found(state),
        Err(_) => StateRead::Unreadable,
    }
}

pub fn consume(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| {
            format!(
                "cannot remove the image state {}; nothing was read",
                path.display()
            )
        }),
    }
}

pub fn save(
    path: &Path,
    state: &ImageState,
    repo_id: &RepoId,
    hostname: &[u8],
    key: &SourceKey,
) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(&state.encode(repo_id, hostname, key))?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)?;
    Ok(())
}

/// Held for a whole run; the lock goes with the descriptor.
pub struct StateLock {
    _file: File,
}

pub fn lock(path: &Path, key: &SourceKey) -> Result<StateLock> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("lock"))
        .context("cannot open the lock of the image state")?;
    match file.try_lock() {
        Ok(()) => Ok(StateLock { _file: file }),
        Err(TryLockError::WouldBlock) => Err(anyhow::anyhow!(
            "another backup of {key} on this machine is running"
        )),
        Err(TryLockError::Error(e)) => {
            Err(e).context("cannot lock the image state of this instance")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: RepoId = [7; 16];

    fn key() -> SourceKey {
        SourceKey::image("disk").unwrap()
    }

    fn state() -> ImageState {
        ImageState {
            parent: [3; 32],
            bitmap_gen: 4,
            disk_size: 8 << 20,
            source: [5; 32],
        }
    }

    fn path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("images").join("x.json")
    }

    #[test]
    fn state_save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(&dir);
        save(&p, &state(), &REPO, b"host", &key()).unwrap();
        assert_eq!(load(&p, &REPO, b"host", &key()), StateRead::Found(state()));
        assert_eq!(
            load(&p, &REPO, b"other", &key()),
            StateRead::Unreadable,
            "another host"
        );
    }

    #[test]
    fn state_missing_oversized_and_garbage_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(&dir);
        assert_eq!(load(&p, &REPO, b"host", &key()), StateRead::Missing);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, vec![b' '; MAX_STATE_BYTES + 1]).unwrap();
        assert_eq!(load(&p, &REPO, b"host", &key()), StateRead::Unreadable);
        std::fs::write(&p, b"not json").unwrap();
        assert_eq!(load(&p, &REPO, b"host", &key()), StateRead::Unreadable);
    }

    #[test]
    fn state_consume_removes_the_file_and_tolerates_none() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(&dir);
        consume(&p).unwrap();
        save(&p, &state(), &REPO, b"host", &key()).unwrap();
        consume(&p).unwrap();
        assert!(!p.exists());
        std::fs::create_dir(&p).unwrap();
        assert!(consume(&p).is_err(), "a directory cannot be consumed");
    }

    #[test]
    fn state_lock_is_exclusive_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(&dir);
        let first = lock(&p, &key()).unwrap();
        let err = lock(&p, &key()).err().expect("a second lock").to_string();
        assert!(err.contains("another backup"), "{err}");
        drop(first);
        assert!(lock(&p, &key()).is_ok());
    }
}
