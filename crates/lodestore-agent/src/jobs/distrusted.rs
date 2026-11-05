use std::collections::HashSet;
use std::sync::Mutex;

use lodestore_format::crypto::BlobId;
use lodestore_format::index::{hex32, parse_hex32};

use crate::Paths;
use crate::identity::write_private_file;

pub(crate) const MAX_DISTRUSTED_SHARDS: usize = 65_536;

static RECORD_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn load(paths: &Paths) -> HashSet<BlobId> {
    let Ok(text) = std::fs::read_to_string(paths.distrusted_shards()) else {
        return HashSet::new();
    };
    text.lines()
        .filter_map(|line| parse_hex32(line.trim()))
        .collect()
}

/// The server is the source of truth: the file always holds its latest list.
pub(crate) fn record(paths: &Paths, ids: &[Vec<u8>]) -> std::io::Result<()> {
    let _guard = RECORD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut text = String::new();
    for id in ids
        .iter()
        .filter_map(|raw| BlobId::try_from(raw.as_slice()).ok())
        .take(MAX_DISTRUSTED_SHARDS)
    {
        text.push_str(&hex32(&id));
        text.push('\n');
    }
    write_private_file(&paths.distrusted_shards(), text.as_bytes())
}

/// A lost update only costs deduplication safety until the next list arrives,
/// so a failed write never fails the job or the session.
pub(crate) fn record_or_warn(paths: &Paths, ids: &[Vec<u8>]) {
    if let Err(e) = record(paths, ids) {
        tracing::warn!(error = %e, "cannot save the list of distrusted index shards");
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

    fn raw(ids: &[BlobId]) -> Vec<Vec<u8>> {
        ids.iter().map(|id| id.to_vec()).collect()
    }

    #[test]
    fn distrusted_round_trip() {
        let (_dir, paths) = paths();
        record(&paths, &raw(&[id(1), id(2)])).unwrap();
        assert_eq!(load(&paths), HashSet::from([id(1), id(2)]));
    }

    #[test]
    fn distrusted_last_list_wins() {
        let (_dir, paths) = paths();
        record(&paths, &raw(&[id(1), id(2)])).unwrap();
        record(&paths, &raw(&[id(3)])).unwrap();
        assert_eq!(load(&paths), HashSet::from([id(3)]));
        record(&paths, &[]).unwrap();
        assert!(load(&paths).is_empty());
    }

    #[test]
    fn distrusted_drops_malformed_ids() {
        let (_dir, paths) = paths();
        let ids = vec![vec![], vec![7; 31], id(5).to_vec(), vec![7; 33]];
        record(&paths, &ids).unwrap();
        assert_eq!(load(&paths), HashSet::from([id(5)]));
    }

    #[test]
    fn distrusted_caps_size() {
        let (_dir, paths) = paths();
        let ids: Vec<BlobId> = (0..MAX_DISTRUSTED_SHARDS as u32 + 10).map(id).collect();
        record(&paths, &raw(&ids)).unwrap();
        let loaded = load(&paths);
        assert_eq!(loaded.len(), MAX_DISTRUSTED_SHARDS);
        assert!(loaded.contains(&id(0)));
        assert!(!loaded.contains(&id(MAX_DISTRUSTED_SHARDS as u32)));
    }

    #[test]
    fn distrusted_missing_file_is_empty() {
        let (_dir, paths) = paths();
        assert!(load(&paths).is_empty());
    }

    #[test]
    fn distrusted_recorded_from_hello() {
        use lodestore_proto::agent::v1::HelloAck;
        let (_dir, paths) = paths();
        let ack = HelloAck {
            distrusted_shards: raw(&[id(9)]),
            ..Default::default()
        };
        record_or_warn(&paths, &ack.distrusted_shards);
        assert_eq!(load(&paths), HashSet::from([id(9)]));
        record_or_warn(&paths, &HelloAck::default().distrusted_shards);
        assert!(load(&paths).is_empty());
    }

    #[test]
    fn distrusted_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, paths) = paths();
        record(&paths, &raw(&[id(1)])).unwrap();
        let mode = std::fs::metadata(paths.distrusted_shards())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
