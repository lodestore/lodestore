use std::collections::HashSet;
use std::sync::Mutex;

use lodestore_format::crypto::BlobId;
use lodestore_format::index::{hex32, parse_hex32};
use lodestore_format::store::BlobStore;
use lodestore_format::writer::Writer;

use crate::Paths;
use crate::identity::write_private_file;

pub(crate) const MAX_OWN_SHARDS: usize = 65_536;

static RECORD_LOCK: Mutex<()> = Mutex::new(());

fn read(paths: &Paths) -> Vec<BlobId> {
    let Ok(text) = std::fs::read_to_string(paths.own_shards()) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| parse_hex32(line.trim()))
        .collect()
}

pub(crate) fn load(paths: &Paths) -> HashSet<BlobId> {
    read(paths).into_iter().collect()
}

pub(crate) fn record(
    paths: &Paths,
    written: &[BlobId],
    attested: &HashSet<BlobId>,
) -> std::io::Result<()> {
    let _guard = RECORD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut seen = HashSet::new();
    let mut ids: Vec<BlobId> = read(paths)
        .into_iter()
        .chain(written.iter().copied())
        .filter(|id| !attested.contains(id) && seen.insert(*id))
        .collect();
    if ids.len() > MAX_OWN_SHARDS {
        ids.drain(..ids.len() - MAX_OWN_SHARDS);
    }
    let mut text = String::with_capacity(ids.len() * 65);
    for id in &ids {
        text.push_str(&hex32(id));
        text.push('\n');
    }
    write_private_file(&paths.own_shards(), text.as_bytes())
}

/// Shards are in the bucket whether or not the backup succeeded; losing the
/// file only costs deduplication, so a failure here never fails the job.
pub(crate) fn record_written<S: BlobStore>(paths: &Paths, writer: &Writer<S>) {
    if let Err(e) = record(paths, writer.written_shards(), writer.attested_shards()) {
        tracing::warn!(error = %e, "cannot save the list of own index shards");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("etc"),
            state_dir: dir.path().join("var"),
        };
        (dir, paths)
    }

    fn id(n: u32) -> BlobId {
        let mut out = [0u8; 32];
        out[..4].copy_from_slice(&n.to_le_bytes());
        out
    }

    #[test]
    fn own_shards_round_trip() {
        let (_dir, paths) = paths();
        record(&paths, &[id(1), id(2)], &HashSet::new()).unwrap();
        record(&paths, &[id(2), id(3)], &HashSet::new()).unwrap();
        assert_eq!(load(&paths), HashSet::from([id(1), id(2), id(3)]));
    }

    #[test]
    fn own_shards_prunes_attested() {
        let (_dir, paths) = paths();
        record(&paths, &[id(1), id(2), id(3)], &HashSet::new()).unwrap();
        record(&paths, &[id(4)], &HashSet::from([id(1), id(3)])).unwrap();
        assert_eq!(load(&paths), HashSet::from([id(2), id(4)]));
    }

    #[test]
    fn own_shards_caps_size() {
        let (_dir, paths) = paths();
        let written: Vec<BlobId> = (0..MAX_OWN_SHARDS as u32 + 10).map(id).collect();
        record(&paths, &written, &HashSet::new()).unwrap();
        let loaded = load(&paths);
        assert_eq!(loaded.len(), MAX_OWN_SHARDS);
        assert!(!loaded.contains(&id(9)));
        assert!(loaded.contains(&id(10)));
        assert!(loaded.contains(&id(MAX_OWN_SHARDS as u32 + 9)));
    }

    #[test]
    fn own_shards_ignores_garbage() {
        let (_dir, paths) = paths();
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        let good = hex32(&id(7));
        let long = "ab".repeat(33);
        let text = format!(
            "not hex\n{}\n\n{good}\n{long}\n{}zz\n\u{0}\u{ff}\n",
            &good[..10],
            "ab".repeat(31)
        );
        std::fs::write(paths.own_shards(), text).unwrap();
        assert_eq!(load(&paths), HashSet::from([id(7)]));
    }

    #[test]
    fn own_shards_missing_file_is_empty() {
        let (_dir, paths) = paths();
        assert!(load(&paths).is_empty());
    }
}
