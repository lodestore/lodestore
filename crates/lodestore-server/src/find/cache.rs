//! Encrypted path lists of snapshots, kept in `data_dir` so a repeat search needs no bucket.
//!
//! A file is `LSFC`, a version byte and frames of `u32 BE length ‖ nonce ‖ ciphertext`. A frame
//! holds up to 256 KiB of records, zstd-compressed and sealed under the unlock key; the
//! associated data binds it to the snapshot, its position and the end of the file. Anything
//! that does not read back cleanly is a miss and the file is deleted.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, bail};
use lodestore_format::crypto::NONCE_LEN;

use super::walk::Entry;
use crate::browse::tree::EntryKind;
use crate::secrets::Sealer;
use crate::state::AppState;

const MAGIC: &[u8] = b"LSFC";
const VERSION: u8 = 1;
const KIND: &str = "find-cache";
const FRAME_PLAIN: usize = 256 * 1024;
const MAX_RECORD: usize = 64 * 1024;
const MAX_PLAIN: usize = FRAME_PLAIN + MAX_RECORD;
const FRAME_LIMIT: usize = 1 << 20;
const ZSTD_LEVEL: i32 = 3;
const STALE_TMP: Duration = Duration::from_secs(3600);

pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("find-cache").join("v1")
}

fn file_path(dir: &Path, snapshot_id: &str) -> Option<PathBuf> {
    let valid = !snapshot_id.is_empty() && snapshot_id.bytes().all(|b| b.is_ascii_hexdigit());
    valid.then(|| dir.join(format!("{snapshot_id}.bin")))
}

fn frame_id(snapshot_id: &str, index: usize, last: bool) -> String {
    format!("{snapshot_id}:{index}:{}", u8::from(last))
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value & 0x7f) as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.0.split_at_checked(n)?;
        self.0 = tail;
        Some(head)
    }

    fn byte(&mut self) -> Option<u8> {
        self.take(1)?.first().copied()
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn varint(&mut self) -> Option<u64> {
        let mut value = 0u64;
        for shift in (0u32..64).step_by(7) {
            let byte = self.byte()?;
            let bits = u64::from(byte & 0x7f);
            if shift == 63 && bits > 1 {
                return None;
            }
            value |= bits << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }
}

fn encode_record(out: &mut Vec<u8>, entry: &Entry) -> anyhow::Result<()> {
    let kind = match entry.kind {
        EntryKind::File => 0u8,
        EntryKind::Symlink => 1,
        _ => bail!("only files and symlinks are cached"),
    };
    let start = out.len();
    put_varint(out, entry.path.len() as u64);
    out.extend_from_slice(&entry.path);
    out.push(kind);
    put_varint(out, entry.size);
    put_varint(out, zigzag(entry.mtime_ms));
    match entry.content_id {
        Some(id) => {
            out.push(1);
            out.extend_from_slice(&id);
        }
        None => out.push(0),
    }
    if out.len() - start > MAX_RECORD {
        out.truncate(start);
        bail!("a path is too long to cache");
    }
    Ok(())
}

pub fn encode_records(entries: &[Entry]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    for entry in entries {
        encode_record(&mut out, entry)?;
    }
    Ok(out)
}

pub fn decode_records(bytes: &[u8]) -> Option<Vec<Entry>> {
    let mut cursor = Cursor(bytes);
    let mut entries = Vec::new();
    while !cursor.is_empty() {
        let len = usize::try_from(cursor.varint()?).ok()?;
        let path = cursor.take(len)?.to_vec();
        let kind = match cursor.byte()? {
            0 => EntryKind::File,
            1 => EntryKind::Symlink,
            _ => return None,
        };
        let size = cursor.varint()?;
        let mtime_ms = unzigzag(cursor.varint()?);
        let content_id = match cursor.byte()? {
            0 => None,
            1 => Some(<[u8; 16]>::try_from(cursor.take(16)?).ok()?),
            _ => return None,
        };
        entries.push(Entry {
            path,
            kind,
            size,
            mtime_ms,
            content_id,
        });
    }
    Some(entries)
}

fn write_frame(
    out: &mut impl Write,
    sealer: &Sealer,
    snapshot_id: &str,
    index: usize,
    last: bool,
    plain: &[u8],
) -> anyhow::Result<()> {
    let packed = zstd::bulk::compress(plain, ZSTD_LEVEL)?;
    let (nonce, ciphertext) = sealer.seal(KIND, &frame_id(snapshot_id, index, last), &packed)?;
    let len = nonce.len() + ciphertext.len();
    if len > FRAME_LIMIT {
        bail!("a cache frame is too large");
    }
    out.write_all(&u32::try_from(len)?.to_be_bytes())?;
    out.write_all(&nonce)?;
    out.write_all(&ciphertext)?;
    Ok(())
}

fn write_file(
    out: &mut impl Write,
    sealer: &Sealer,
    snapshot_id: &str,
    entries: &[Entry],
) -> anyhow::Result<()> {
    out.write_all(MAGIC)?;
    out.write_all(&[VERSION])?;
    let mut pending: Option<Vec<u8>> = None;
    let mut index = 0usize;
    let mut buf = Vec::new();
    for entry in entries {
        encode_record(&mut buf, entry)?;
        if buf.len() >= FRAME_PLAIN
            && let Some(previous) = pending.replace(std::mem::take(&mut buf))
        {
            write_frame(out, sealer, snapshot_id, index, false, &previous)?;
            index += 1;
        }
    }
    if (!buf.is_empty() || pending.is_none())
        && let Some(previous) = pending.replace(buf)
    {
        write_frame(out, sealer, snapshot_id, index, false, &previous)?;
        index += 1;
    }
    if let Some(last) = pending {
        write_frame(out, sealer, snapshot_id, index, true, &last)?;
    }
    Ok(())
}

/// Publishes the list under its final name only once it is complete; returns its size.
pub fn store(
    sealer: &Sealer,
    dir: &Path,
    snapshot_id: &str,
    entries: &[Entry],
) -> anyhow::Result<u64> {
    let path = file_path(dir, snapshot_id).context("the snapshot id is not valid")?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let tmp = dir.join(format!(
        "{snapshot_id}.bin.tmp-{}",
        uuid::Uuid::now_v7().simple()
    ));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    let published = (|| -> anyhow::Result<u64> {
        let mut out = BufWriter::new(file);
        write_file(&mut out, sealer, snapshot_id, entries)?;
        out.flush()?;
        out.get_ref().sync_all()?;
        let len = out.get_ref().metadata()?.len();
        fs::rename(&tmp, &path)?;
        File::open(dir)?.sync_all()?;
        Ok(len)
    })();
    if published.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    published
}

fn unpack(packed: &[u8]) -> Option<Vec<u8>> {
    let mut plain = Vec::new();
    zstd::stream::read::Decoder::new(packed)
        .ok()?
        .take(MAX_PLAIN as u64 + 1)
        .read_to_end(&mut plain)
        .ok()?;
    (plain.len() <= MAX_PLAIN).then_some(plain)
}

fn read_file(sealer: &Sealer, snapshot_id: &str, bytes: &[u8]) -> Option<Vec<Entry>> {
    let mut cursor = Cursor(bytes);
    if cursor.take(MAGIC.len())? != MAGIC || cursor.byte()? != VERSION {
        return None;
    }
    let mut entries = Vec::new();
    let mut index = 0usize;
    loop {
        let len = <[u8; 4]>::try_from(cursor.take(4)?).ok()?;
        let len = usize::try_from(u32::from_be_bytes(len)).ok()?;
        if len > FRAME_LIMIT {
            return None;
        }
        let (nonce, ciphertext) = cursor.take(len)?.split_at_checked(NONCE_LEN)?;
        let last = cursor.is_empty();
        let id = frame_id(snapshot_id, index, last);
        let packed = sealer.open(KIND, &id, nonce, ciphertext).ok()?;
        entries.extend(decode_records(&unpack(&packed)?)?);
        if last {
            return Some(entries);
        }
        index = index.checked_add(1)?;
    }
}

/// The cached list, or `None` when there is none, it is larger than `max_bytes` or it cannot be
/// trusted, in which case the file is removed.
pub fn load(sealer: &Sealer, dir: &Path, snapshot_id: &str, max_bytes: u64) -> Option<Vec<Entry>> {
    let path = file_path(dir, snapshot_id)?;
    let mut file = File::open(&path).ok()?;
    let fits = file.metadata().is_ok_and(|meta| meta.len() <= max_bytes);
    let mut bytes = Vec::new();
    let entries = fits
        .then(|| file.read_to_end(&mut bytes).ok())
        .flatten()
        .and_then(|_| read_file(sealer, snapshot_id, &bytes));
    match entries {
        Some(entries) => {
            let _ = file.set_modified(SystemTime::now());
            Some(entries)
        }
        None => {
            let _ = fs::remove_file(&path);
            None
        }
    }
}

pub fn remove(dir: &Path, snapshot_id: &str) {
    if let Some(path) = file_path(dir, snapshot_id) {
        let _ = fs::remove_file(path);
    }
}

fn files(dir: &Path) -> Vec<(PathBuf, String, fs::Metadata)> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    read.filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            Some((e.path(), name, e.metadata().ok()?))
        })
        .collect()
}

/// Deletes the files used longest ago until the lists fit in `max_bytes`.
pub fn evict(dir: &Path, max_bytes: u64) {
    evict_with(dir, max_bytes, |path| fs::remove_file(path));
}

fn evict_with(dir: &Path, max_bytes: u64, remove: impl Fn(&Path) -> std::io::Result<()>) {
    let mut lists: Vec<_> = files(dir)
        .into_iter()
        .filter(|(_, name, _)| name.ends_with(".bin"))
        .map(|(path, _, meta)| (meta.modified().ok(), meta.len(), path))
        .collect();
    let mut total: u64 = lists.iter().map(|(_, len, _)| len).sum();
    lists.sort_by_key(|list| list.0);
    for (_, len, path) in lists {
        if total <= max_bytes {
            break;
        }
        // A list another eviction already removed no longer counts against the budget.
        match remove(&path) {
            Ok(()) => total = total.saturating_sub(len),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                total = total.saturating_sub(len);
            }
            Err(_) => {}
        }
    }
}

fn sweep_dir(dir: &Path, snapshots: &HashSet<String>, now: SystemTime) {
    for (path, name, meta) in files(dir) {
        let stale = if let Some(id) = name.strip_suffix(".bin") {
            !snapshots.contains(id)
        } else if name.contains(".bin.tmp-") {
            meta.modified()
                .ok()
                .and_then(|at| now.duration_since(at).ok())
                .is_some_and(|age| age > STALE_TMP)
        } else {
            false
        };
        if stale {
            let _ = fs::remove_file(path);
        }
    }
}

/// Drops the lists of snapshots that are no longer in the catalog and abandoned temp files.
pub async fn sweep_orphans(state: &AppState) -> anyhow::Result<()> {
    let ids: HashSet<String> = sqlx::query_scalar("SELECT id FROM snapshots")
        .fetch_all(&state.db.pool)
        .await?
        .into_iter()
        .collect();
    let dir = dir(&state.config.data_dir);
    tokio::task::spawn_blocking(move || sweep_dir(&dir, &ids, SystemTime::now())).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Secret};
    use crate::db::Db;

    const ID: &str = "ab12";

    fn entry(path: &[u8], content: bool) -> Entry {
        Entry {
            path: path.to_vec(),
            kind: EntryKind::File,
            size: 1234,
            mtime_ms: -5,
            content_id: content.then_some([7; 16]),
        }
    }

    fn same(a: &[Entry], b: &[Entry]) -> bool {
        a.len() == b.len()
            && a.iter().zip(b).all(|(x, y)| {
                (&x.path, x.kind, x.size, x.mtime_ms, x.content_id)
                    == (&y.path, y.kind, y.size, y.mtime_ms, y.content_id)
            })
    }

    async fn sealer(dir: &Path, key: &str) -> Sealer {
        let config = Config {
            data_dir: dir.to_path_buf(),
            database_url: format!("sqlite://{}/seal.db?mode=rwc", dir.display()),
            unlock_key: Some(Secret::new(key.repeat(32))),
            ..Config::default()
        };
        let db = Db::connect(&config.database_url).await.unwrap();
        Sealer::load(&db, &config).await.unwrap()
    }

    #[test]
    fn records_roundtrip() {
        let mut link = entry(b"/a/\xff/link", false);
        link.kind = EntryKind::Symlink;
        link.size = u64::MAX;
        link.mtime_ms = i64::MIN;
        for entries in [
            Vec::new(),
            vec![entry(b"/etc/nginx/nginx.conf", true), link],
        ] {
            let bytes = encode_records(&entries).unwrap();
            assert!(same(&decode_records(&bytes).unwrap(), &entries));
        }
        let mut dir = entry(b"/d", false);
        dir.kind = EntryKind::Dir;
        assert!(encode_records(&[dir]).is_err());
        assert!(encode_records(&[entry(&vec![b'a'; MAX_RECORD], false)]).is_err());
    }

    #[tokio::test]
    async fn every_truncation_and_byte_flip_is_a_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let sealer = sealer(tmp.path(), "07").await;
        let cache = tmp.path().join("cache");
        let entries = vec![
            entry(b"/etc/nginx/nginx.conf", true),
            entry(b"/etc/hosts", false),
        ];
        let len = store(&sealer, &cache, ID, &entries).unwrap();
        let path = file_path(&cache, ID).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(len, bytes.len() as u64);
        assert!(load(&sealer, &cache, ID, len - 1).is_none());
        assert!(!path.exists());
        fs::write(&path, &bytes).unwrap();
        assert!(same(&load(&sealer, &cache, ID, len).unwrap(), &entries));

        for len in 0..bytes.len() {
            fs::write(&path, bytes.get(..len).unwrap()).unwrap();
            assert!(
                load(&sealer, &cache, ID, u64::MAX).is_none(),
                "truncated to {len}"
            );
            assert!(!path.exists());
        }
        for at in 0..bytes.len() {
            let mut flipped = bytes.clone();
            *flipped.get_mut(at).unwrap() ^= 1;
            fs::write(&path, &flipped).unwrap();
            assert!(load(&sealer, &cache, ID, u64::MAX).is_none(), "byte {at}");
            assert!(!path.exists());
        }

        let other = tempfile::tempdir().unwrap();
        let wrong_key = self::sealer(other.path(), "08").await;
        fs::write(&path, &bytes).unwrap();
        assert!(load(&wrong_key, &cache, ID, u64::MAX).is_none());
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn frames_are_bound_to_order_and_end() {
        let tmp = tempfile::tempdir().unwrap();
        let sealer = sealer(tmp.path(), "07").await;
        let entries: Vec<Entry> = (0..30_000)
            .map(|n| entry(format!("/data/dir/file-{n:06}.conf").as_bytes(), n % 2 == 0))
            .collect();
        store(&sealer, tmp.path(), ID, &entries).unwrap();
        let path = file_path(tmp.path(), ID).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert!(same(
            &load(&sealer, tmp.path(), ID, u64::MAX).unwrap(),
            &entries
        ));

        let mut frames = Vec::new();
        let mut cursor = Cursor(bytes.get(MAGIC.len() + 1..).unwrap());
        while !cursor.is_empty() {
            let len = u32::from_be_bytes(cursor.take(4).unwrap().try_into().unwrap());
            let body = cursor.take(len as usize).unwrap();
            frames.push([&len.to_be_bytes()[..], body].concat());
        }
        assert!(frames.len() >= 3, "{} frames", frames.len());
        let header = bytes.get(..MAGIC.len() + 1).unwrap();
        let rebuild = |order: &[usize]| {
            let mut out = header.to_vec();
            for &i in order {
                out.extend_from_slice(frames.get(i).unwrap());
            }
            out
        };
        let last = frames.len() - 1;
        let dropped: Vec<usize> = (0..last).collect();
        let mut swapped: Vec<usize> = (0..=last).collect();
        swapped.swap(0, 1);
        for order in [dropped, swapped] {
            fs::write(&path, rebuild(&order)).unwrap();
            assert!(load(&sealer, tmp.path(), ID, u64::MAX).is_none());
        }
    }

    #[tokio::test]
    async fn no_plaintext_paths_in_file() {
        let tmp = tempfile::tempdir().unwrap();
        let sealer = sealer(tmp.path(), "07").await;
        let entries = vec![
            entry(b"/etc/nginx/nginx.conf", true),
            entry(b"/etc/nginx/nginx.conf.bak", false),
        ];
        store(&sealer, tmp.path(), ID, &entries).unwrap();
        let bytes = fs::read(file_path(tmp.path(), ID).unwrap()).unwrap();
        assert!(bytes.starts_with(b"LSFC\x01"));
        assert!(!bytes.windows(10).any(|w| w == b"nginx.conf"));
        assert!(!bytes.windows(4).any(|w| w == b"/etc"));
        let temp = fs::read_dir(tmp.path()).unwrap().filter_map(Result::ok);
        assert_eq!(
            temp.filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
                .count(),
            0
        );
    }

    #[test]
    fn evict_drops_the_least_recently_used() {
        let tmp = tempfile::tempdir().unwrap();
        let epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        for (name, age) in [("a", 30), ("b", 10), ("c", 20)] {
            let file = File::create(tmp.path().join(format!("{name}.bin"))).unwrap();
            file.set_len(100).unwrap();
            file.set_modified(epoch + Duration::from_secs(age)).unwrap();
        }
        evict(tmp.path(), 200);
        assert!(!tmp.path().join("b.bin").exists());
        assert!(tmp.path().join("a.bin").exists() && tmp.path().join("c.bin").exists());
        evict(tmp.path(), 0);
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn evict_keeps_the_newest_list_when_another_eviction_took_the_oldest() {
        let tmp = tempfile::tempdir().unwrap();
        let epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        for (name, age) in [("old", 10), ("new", 20)] {
            let file = File::create(tmp.path().join(format!("{name}.bin"))).unwrap();
            file.set_len(100).unwrap();
            file.set_modified(epoch + Duration::from_secs(age)).unwrap();
        }
        evict_with(tmp.path(), 150, |path| {
            fs::remove_file(path)?;
            if path.ends_with("old.bin") {
                return Err(std::io::ErrorKind::NotFound.into());
            }
            Ok(())
        });
        assert!(!tmp.path().join("old.bin").exists());
        assert!(tmp.path().join("new.bin").exists());
    }

    #[test]
    fn sweep_drops_orphans_and_stale_temp_files() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ["ab12.bin", "cd34.bin", "ab12.bin.tmp-x", "note.txt"] {
            File::create(tmp.path().join(name)).unwrap();
        }
        let live = HashSet::from(["ab12".to_string()]);
        sweep_dir(tmp.path(), &live, SystemTime::now());
        assert!(!tmp.path().join("cd34.bin").exists());
        assert!(tmp.path().join("ab12.bin.tmp-x").exists());
        sweep_dir(tmp.path(), &live, SystemTime::now() + 2 * STALE_TMP);
        assert!(!tmp.path().join("ab12.bin.tmp-x").exists());
        assert!(tmp.path().join("ab12.bin").exists() && tmp.path().join("note.txt").exists());
    }
}
