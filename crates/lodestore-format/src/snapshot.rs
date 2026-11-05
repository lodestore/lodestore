//! Sealed snapshots. The body is encrypted under a fresh random key that is
//! itself sealed to the repository public key, so a writer holding only the
//! public key can create snapshots but not read them.

use std::collections::HashMap;

use prost::Message;

use crate::Fut;
use crate::crypto::{
    BlobId, Entropy, HpkePrivateKey, HpkePublicKey, NONCE_LEN, RepoId, open_snapshot_body,
    open_snapshot_key, seal_snapshot_body, seal_snapshot_key,
};
use crate::error::{Error, Result, within_size};
use crate::index::{hex32, parse_hex32};
use crate::keys::ReadKeys;
use crate::pb;
use crate::reader::Reader;
use crate::store::BlobStore;

pub(crate) const SNAPSHOT_PREFIX: &str = "snapshots/";
pub const ENVELOPE_VERSION: u32 = 1;
/// Largest snapshot object read from a listing.
pub const MAX_SNAPSHOT_SIZE: u64 = 64 << 20;

pub fn snapshot_object_key(id: &BlobId) -> String {
    format!("{SNAPSHOT_PREFIX}{}", hex32(id))
}

#[derive(Debug, Clone)]
pub struct SnapshotEntry {
    pub id: BlobId,
    pub snapshot: pb::Snapshot,
    /// When the store wrote the snapshot object, from the listing; `None` when the backend does not report it.
    pub modified_ms: Option<i64>,
}

/// Returns `(snapshot_id, envelope_bytes)`; the id is the BLAKE3 of the bytes.
pub fn seal_snapshot(
    snapshot: &pb::Snapshot,
    public_key: &HpkePublicKey,
    repo_id: &RepoId,
    entropy: &mut Entropy,
) -> Result<(BlobId, Vec<u8>)> {
    let key = entropy.key32();
    let (hpke_enc, sealed_key) = seal_snapshot_key(public_key, repo_id, &key, entropy)?;
    let sealed = seal_snapshot_body(&key, repo_id, &snapshot.encode_to_vec(), entropy)?;
    let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
    let envelope = pb::SnapshotEnvelope {
        version: ENVELOPE_VERSION,
        hpke_enc,
        sealed_key,
        nonce: nonce.to_vec(),
        ciphertext: ciphertext.to_vec(),
    };
    let bytes = envelope.encode_to_vec();
    Ok((*blake3::hash(&bytes).as_bytes(), bytes))
}

/// Opens an envelope after checking that its name is the hash of its bytes.
pub fn open_snapshot(
    private_key: &HpkePrivateKey,
    repo_id: &RepoId,
    id: &BlobId,
    bytes: &[u8],
) -> Result<pb::Snapshot> {
    let name = snapshot_object_key(id);
    if blake3::hash(bytes).as_bytes() != id {
        return Err(Error::corrupt(
            name,
            "object name does not match its content",
        ));
    }
    let envelope = pb::SnapshotEnvelope::decode(bytes)
        .map_err(|e| Error::corrupt(name.clone(), format!("undecodable: {e}")))?;
    if envelope.version > ENVELOPE_VERSION {
        return Err(Error::UnsupportedVersion {
            what: "snapshot version",
            required: envelope.version,
            supported: ENVELOPE_VERSION,
        });
    }
    let key = open_snapshot_key(
        private_key,
        repo_id,
        &envelope.hpke_enc,
        &envelope.sealed_key,
    )
    .map_err(|_| Error::corrupt(name.clone(), "snapshot key does not open"))?;
    let sealed = [envelope.nonce.as_slice(), envelope.ciphertext.as_slice()].concat();
    let plain = open_snapshot_body(&key, repo_id, &sealed)
        .map_err(|_| Error::corrupt(name.clone(), "authentication failed"))?;
    pb::Snapshot::decode(plain.as_slice())
        .map_err(|e| Error::corrupt(name, format!("undecodable body: {e}")))
}

fn sort_key(e: &SnapshotEntry) -> (i64, u32, BlobId) {
    let t = e.snapshot.time.unwrap_or_default();
    (t.seconds, t.nanos, e.id)
}

/// What a listing of `snapshots/` found.
#[derive(Debug, Default)]
pub struct SnapshotList {
    /// Oldest first; equal times are ordered by id.
    pub entries: Vec<SnapshotEntry>,
    /// `(object key, reason)` of every object that is not a snapshot this
    /// build can open. Any credential that can create objects can plant one,
    /// so they must not hide the real snapshots.
    pub skipped: Vec<(String, String)>,
}

/// All snapshots that open; the others are reported in `skipped`.
pub async fn list_snapshots<S: BlobStore>(store: &S, keys: &ReadKeys) -> Result<SnapshotList> {
    let mut out = SnapshotList::default();
    for entry in store.list(SNAPSHOT_PREFIX).await? {
        let Some(id) = entry
            .key
            .strip_prefix(SNAPSHOT_PREFIX)
            .and_then(parse_hex32)
        else {
            continue;
        };
        if let Err(e) = within_size(&entry.key, entry.size, MAX_SNAPSHOT_SIZE, "snapshot") {
            out.skipped.push((entry.key, e.to_string()));
            continue;
        }
        let bytes = store.get(&entry.key).await?;
        match open_snapshot(keys.hpke_private_key(), keys.repo_id(), &id, &bytes) {
            Ok(snapshot) => out.entries.push(SnapshotEntry {
                id,
                snapshot,
                modified_ms: entry.modified_ms,
            }),
            Err(e @ (Error::Corrupt { .. } | Error::UnsupportedVersion { .. })) => {
                out.skipped.push((entry.key, e.to_string()));
            }
            Err(e) => return Err(e),
        }
    }
    out.entries.sort_by_key(sort_key);
    Ok(out)
}

/// The latest snapshot of this host that covered exactly these paths.
/// `entries` is oldest first, as [`list_snapshots`] returns it; `paths` are
/// the canonical roots as written to the manifest.
pub fn find_parent<'a>(
    entries: &'a [SnapshotEntry],
    hostname: &[u8],
    paths: &[Vec<u8>],
) -> Option<&'a SnapshotEntry> {
    let sorted = |p: &[Vec<u8>]| {
        let mut p = p.to_vec();
        p.sort();
        p
    };
    let want = sorted(paths);
    entries
        .iter()
        .rev()
        .find(|e| e.snapshot.hostname == hostname && sorted(&e.snapshot.paths) == want)
}

/// What [`ParentTree`] needs from a reader; implemented by `Reader` and by
/// the agent's local tree cache.
pub trait TreeSource {
    fn snapshot_root<'a>(&'a mut self, id: &'a BlobId) -> Fut<'a, (pb::Snapshot, Vec<pb::Node>)>;
    fn tree<'a>(&'a mut self, ids: &'a [Vec<u8>]) -> Fut<'a, Vec<pb::Node>>;
}

/// A parent snapshot's tree, loaded one directory at a time as the backup
/// walk asks for paths. Only the chain of directories leading to the last
/// path asked for stays in memory. The writer itself holds no read key, so
/// the parent is read through a `Reader` handed in by the caller (D-22).
pub struct ParentTree<'a> {
    id: BlobId,
    time: pb::Timestamp,
    source: Box<dyn TreeSource + 'a>,
    dirs: HashMap<Vec<u8>, Vec<pb::Node>>,
}

impl<'a> ParentTree<'a> {
    pub async fn open<S: BlobStore + 'a>(reader: Reader<S>, id: BlobId) -> Result<Self> {
        Self::from_source(Box::new(reader), id).await
    }

    pub async fn from_source(mut source: Box<dyn TreeSource + 'a>, id: BlobId) -> Result<Self> {
        let (snapshot, root) = source.snapshot_root(&id).await?;
        Ok(ParentTree {
            id,
            time: snapshot.time.unwrap_or_default(),
            source,
            dirs: HashMap::from([(Vec::new(), root)]),
        })
    }

    pub fn id(&self) -> BlobId {
        self.id
    }

    /// When the parent backup started.
    pub fn time(&self) -> &pb::Timestamp {
        &self.time
    }

    /// The node at an absolute path (`/a/b/c`) as the parent recorded it.
    pub async fn lookup(&mut self, path: &[u8]) -> Result<Option<pb::Node>> {
        let Some(cut) = path.iter().rposition(|&b| b == b'/') else {
            return Ok(None);
        };
        let (dir, name) = (&path[..cut], &path[cut + 1..]);
        let mut current = Vec::new();
        for part in dir.split(|&b| b == b'/').filter(|c| !c.is_empty()) {
            let mut next = current.clone();
            next.push(b'/');
            next.extend_from_slice(part);
            if !self.dirs.contains_key(&next) {
                let Some(node) = find_node(&self.dirs[&current], part) else {
                    return Ok(None);
                };
                if node.r#type != pb::NodeType::Dir as i32 {
                    return Ok(None);
                }
                let subtrees = node.subtrees.clone();
                let nodes = self.source.tree(&subtrees).await?;
                self.dirs.insert(next.clone(), nodes);
            }
            current = next;
        }
        self.dirs.retain(|k, _| {
            k.is_empty() || dir == k || (dir.starts_with(k) && dir.get(k.len()) == Some(&b'/'))
        });
        Ok(find_node(&self.dirs[&current], name).cloned())
    }
}

fn find_node<'n>(nodes: &'n [pb::Node], name: &[u8]) -> Option<&'n pb::Node> {
    nodes
        .binary_search_by(|n| n.name.as_slice().cmp(name))
        .ok()
        .map(|i| &nodes[i])
}

#[cfg(test)]
mod tests {
    use hpke::Kem;
    use hpke::kem::XWing;
    use rand_chacha::ChaCha20Rng;
    use rand_chacha::rand_core::SeedableRng;

    use super::*;

    #[test]
    fn snapshot_seal_roundtrip_and_name_check() {
        let mut ent = Entropy::from_rng(ChaCha20Rng::from_seed([1; 32]));
        let repo = [2u8; 16];
        let (sk, pk) = XWing::derive_keypair(&[3u8; 32]);
        let (other_sk, _) = XWing::derive_keypair(&[4u8; 32]);
        let snap = pb::Snapshot {
            hostname: b"host".to_vec(),
            root_tree: vec![5; 32],
            ..Default::default()
        };
        let (id, bytes) = seal_snapshot(&snap, &pk, &repo, &mut ent).unwrap();

        assert_eq!(open_snapshot(&sk, &repo, &id, &bytes).unwrap(), snap);
        assert!(matches!(
            open_snapshot(&sk, &repo, &[0; 32], &bytes),
            Err(Error::Corrupt { .. })
        ));
        assert!(open_snapshot(&other_sk, &repo, &id, &bytes).is_err());
        assert!(open_snapshot(&sk, &[9; 16], &id, &bytes).is_err());
    }
}
