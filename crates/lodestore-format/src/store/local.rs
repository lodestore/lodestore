use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use bytes::Bytes;
use rustix::fs::{AtFlags, CWD, Mode, OFlags};

use super::{BlobDelete, BlobError, BlobStore, ListEntry};

const TMP_PREFIX: &str = ".lode-tmp-";

/// Repository in a local folder. Create-only writes go through a temporary
/// file in the destination directory that is hard-linked into place, because
/// `link` fails when the target exists whereas `rename` silently replaces it.
#[derive(Debug, Clone)]
pub struct LocalFsStore {
    root: PathBuf,
}

impl LocalFsStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(super) async fn modified_ms(&self, key: &str) -> Result<Option<i64>, BlobError> {
        let path = self.path_of(key)?;
        tokio::task::spawn_blocking(move || std::fs::metadata(path).map(|m| mtime_ms(&m)))
            .await
            .map_err(io_err)?
            .map_err(map_read_err)
    }

    fn path_of(&self, key: &str) -> Result<PathBuf, BlobError> {
        let bad = key.is_empty()
            || key.contains('\0')
            || key
                .split('/')
                .any(|c| c.is_empty() || c == "." || c == ".." || c.starts_with(TMP_PREFIX));
        if bad {
            return Err(BlobError::backend(format!("invalid object key {key:?}")));
        }
        Ok(self.root.join(key))
    }
}

fn io_err(e: impl std::fmt::Display) -> BlobError {
    BlobError::backend(e.to_string())
}

fn map_read_err(e: std::io::Error) -> BlobError {
    if e.kind() == std::io::ErrorKind::NotFound {
        BlobError::NotFound
    } else {
        io_err(e)
    }
}

fn put_blocking(path: &Path, data: &[u8]) -> Result<(), BlobError> {
    let dir = path
        .parent()
        .ok_or_else(|| io_err("object has no parent"))?;
    std::fs::create_dir_all(dir).map_err(io_err)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io_err("object name is not valid UTF-8"))?;

    let dirfd = rustix::fs::openat(
        CWD,
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io_err)?;

    let mut rnd = [0u8; 8];
    getrandom::fill(&mut rnd).map_err(io_err)?;
    let tmp_name = format!("{TMP_PREFIX}{}", hex(&rnd));

    let tmp_fd = rustix::fs::openat(
        &dirfd,
        tmp_name.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(io_err)?;

    let result = (|| {
        let mut file = File::from(tmp_fd);
        file.write_all(data).map_err(io_err)?;
        file.sync_all().map_err(io_err)?;
        match rustix::fs::linkat(&dirfd, tmp_name.as_str(), &dirfd, name, AtFlags::empty()) {
            Ok(()) => Ok(()),
            Err(rustix::io::Errno::EXIST) => Err(BlobError::AlreadyExists),
            Err(e) => Err(io_err(e)),
        }
    })();

    let unlinked = rustix::fs::unlinkat(&dirfd, tmp_name.as_str(), AtFlags::empty());
    result?;
    unlinked.map_err(io_err)?;
    rustix::fs::fsync(&dirfd).map_err(io_err)?;
    Ok(())
}

fn delete_blocking(path: &Path) -> Result<(), BlobError> {
    let dir = path
        .parent()
        .ok_or_else(|| io_err("object has no parent"))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io_err("object name is not valid UTF-8"))?;
    let dirfd = rustix::fs::openat(
        CWD,
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| match e {
        rustix::io::Errno::NOENT => BlobError::NotFound,
        e => io_err(e),
    })?;
    match rustix::fs::unlinkat(&dirfd, name, AtFlags::empty()) {
        Ok(()) => {}
        Err(rustix::io::Errno::NOENT) => return Err(BlobError::NotFound),
        Err(e) => return Err(io_err(e)),
    }
    rustix::fs::fsync(&dirfd).map_err(io_err)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn mtime_ms(meta: &std::fs::Metadata) -> Option<i64> {
    let since = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(since.as_millis()).ok()
}

fn collect(dir: &Path, key_prefix: &str, out: &mut Vec<ListEntry>) -> std::io::Result<()> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in rd {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.starts_with(TMP_PREFIX) {
            continue;
        }
        let key = format!("{key_prefix}{name}");
        let ty = entry.file_type()?;
        if ty.is_dir() {
            collect(&entry.path(), &format!("{key}/"), out)?;
        } else if ty.is_file() {
            let meta = entry.metadata()?;
            out.push(ListEntry {
                key,
                size: meta.len(),
                modified_ms: mtime_ms(&meta),
            });
        }
    }
    Ok(())
}

impl BlobStore for LocalFsStore {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<(), BlobError> {
        let path = self.path_of(key)?;
        tokio::task::spawn_blocking(move || put_blocking(&path, &data))
            .await
            .map_err(io_err)?
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        let path = self.path_of(key)?;
        tokio::task::spawn_blocking(move || std::fs::read(path).map(Bytes::from))
            .await
            .map_err(io_err)?
            .map_err(map_read_err)
    }

    async fn get_range(&self, key: &str, range: std::ops::Range<u64>) -> Result<Bytes, BlobError> {
        let path = self.path_of(key)?;
        tokio::task::spawn_blocking(move || {
            let file = File::open(path)?;
            let len = usize::try_from(range.end.saturating_sub(range.start))
                .map_err(|_| std::io::Error::other("range too large"))?;
            let mut buf = vec![0u8; len];
            let mut filled = 0;
            while filled < len {
                let n = file.read_at(&mut buf[filled..], range.start + filled as u64)?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            buf.truncate(filled);
            Ok(Bytes::from(buf))
        })
        .await
        .map_err(io_err)?
        .map_err(map_read_err)
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        let root = self.root.clone();
        let prefix = prefix.to_owned();
        tokio::task::spawn_blocking(move || {
            let (dir_part, _) = prefix.rsplit_once('/').unwrap_or(("", &prefix));
            let (dir, key_prefix) = if dir_part.is_empty() {
                (root.clone(), String::new())
            } else {
                (root.join(dir_part), format!("{dir_part}/"))
            };
            let mut out = Vec::new();
            collect(&dir, &key_prefix, &mut out)?;
            out.retain(|e| e.key.starts_with(&prefix));
            out.sort_by(|a, b| a.key.cmp(&b.key));
            Ok(out)
        })
        .await
        .map_err(io_err)?
        .map_err(|e: std::io::Error| io_err(e))
    }

    async fn ensure_prefixes(&self, prefixes: &[&str]) -> Result<(), BlobError> {
        for p in prefixes {
            let path = self.path_of(p.trim_end_matches('/'))?;
            tokio::task::spawn_blocking(move || std::fs::create_dir_all(path))
                .await
                .map_err(io_err)?
                .map_err(io_err)?;
        }
        Ok(())
    }
}

impl BlobDelete for LocalFsStore {
    async fn delete(&self, key: &str) -> Result<(), BlobError> {
        let path = self.path_of(key)?;
        tokio::task::spawn_blocking(move || delete_blocking(&path))
            .await
            .map_err(io_err)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn store_create_only_local() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalFsStore::new(dir.path().join("repo"));

        store
            .put_if_absent("packs/ab/object", Bytes::from_static(b"first"))
            .await
            .unwrap();
        let err = store
            .put_if_absent("packs/ab/object", Bytes::from_static(b"second"))
            .await
            .unwrap_err();
        assert!(matches!(err, BlobError::AlreadyExists));
        assert_eq!(
            store.get("packs/ab/object").await.unwrap(),
            Bytes::from_static(b"first")
        );
        assert_eq!(
            store.get_range("packs/ab/object", 1..3).await.unwrap(),
            Bytes::from_static(b"ir")
        );
        assert!(matches!(
            store.get("packs/ab/missing").await,
            Err(BlobError::NotFound)
        ));

        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("repo/packs/ab"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers.len(), 1);
    }

    #[tokio::test]
    async fn store_delete_local() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalFsStore::new(dir.path().join("repo"));
        store
            .put_if_absent("keys/one", Bytes::from_static(b"1"))
            .await
            .unwrap();
        store.delete("keys/one").await.unwrap();
        assert!(matches!(
            store.get("keys/one").await,
            Err(BlobError::NotFound)
        ));
        assert!(matches!(
            store.delete("keys/one").await,
            Err(BlobError::NotFound)
        ));
        assert!(matches!(
            store.delete("nowhere/one").await,
            Err(BlobError::NotFound)
        ));
    }

    #[tokio::test]
    async fn store_list_skips_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalFsStore::new(dir.path().join("repo"));
        store
            .put_if_absent("packs/ab/one", Bytes::from_static(b"1"))
            .await
            .unwrap();
        store
            .put_if_absent("packs/cd/two", Bytes::from_static(b"22"))
            .await
            .unwrap();
        store
            .put_if_absent("index/three", Bytes::from_static(b"333"))
            .await
            .unwrap();
        std::fs::write(dir.path().join("repo/packs/ab/.lode-tmp-deadbeef"), b"x").unwrap();

        let packs = store.list("packs/").await.unwrap();
        let shape: Vec<_> = packs.iter().map(|e| (e.key.as_str(), e.size)).collect();
        assert_eq!(shape, vec![("packs/ab/one", 1), ("packs/cd/two", 2)]);
        assert_eq!(store.list("").await.unwrap().len(), 3);
        assert!(store.list("keys/").await.unwrap().is_empty());
        assert!(
            store
                .put_if_absent("../escape", Bytes::new())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn store_modified_ms_local() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalFsStore::new(dir.path().join("repo"));
        store
            .put_if_absent("snapshots/one", Bytes::from_static(b"1"))
            .await
            .unwrap();
        let when = UNIX_EPOCH + std::time::Duration::from_millis(1_767_323_045_000);
        File::options()
            .write(true)
            .open(dir.path().join("repo/snapshots/one"))
            .unwrap()
            .set_modified(when)
            .unwrap();

        let listed = store.list("snapshots/").await.unwrap();
        assert_eq!(listed[0].modified_ms, Some(1_767_323_045_000));
        assert_eq!(
            store.modified_ms("snapshots/one").await.unwrap(),
            Some(1_767_323_045_000)
        );
        assert!(matches!(
            store.modified_ms("snapshots/none").await,
            Err(BlobError::NotFound)
        ));
    }
}
