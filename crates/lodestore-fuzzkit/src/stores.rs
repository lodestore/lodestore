//! Harnesses that need a repository to read from: the golden fixture, shared
//! read-only by the whole process, with the objects a call wants to lie about
//! laid over it in memory.

use std::collections::HashSet;
use std::ops::Range;
use std::path::Path;
use std::sync::OnceLock;

use bytes::Bytes;
use lodestore_format::crypto::{BlobId, seal_index_shard};
use lodestore_format::index::index_object_key;
use lodestore_format::keys::ReadKeys;
use lodestore_format::pb;
use lodestore_format::repo::Repository;
use lodestore_format::snapshot::{open_snapshot, seal_snapshot, snapshot_object_key};
use lodestore_format::store::{BlobError, BlobStore, ListEntry, LocalFsStore};
use lodestore_format::verify_snapshot::{check_snapshot_resolves, verify_shard as verify};
use prost::Message;
use tokio::runtime::{Builder, Runtime};

use crate::entropy;

struct Golden {
    rt: Runtime,
    store: LocalFsStore,
    repo: Repository,
    shards: Vec<BlobId>,
    shard_keys: Vec<String>,
    packs: Vec<String>,
    snapshots: Vec<(BlobId, pb::Snapshot)>,
}

fn golden() -> &'static Golden {
    static GOLDEN: OnceLock<Golden> = OnceLock::new();
    GOLDEN.get_or_init(|| {
        let fixtures =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../lodestore-format/tests/fixtures/v1");
        let password =
            std::fs::read_to_string(fixtures.join("PASSWORD")).expect("PASSWORD fixture");
        let store = LocalFsStore::new(fixtures.join("repo"));
        let rt = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a current-thread runtime");
        let repo = rt
            .block_on(Repository::open(&store, password.trim_end().as_bytes()))
            .expect("open the golden repository");
        let keys_of = |prefix: &str| {
            let mut keys: Vec<String> = rt
                .block_on(store.list(prefix))
                .expect("list the golden repository")
                .into_iter()
                .map(|e| e.key)
                .collect();
            keys.sort();
            keys
        };
        let id_of = |key: &str| -> BlobId {
            let hex = key.rsplit('/').next().unwrap_or_default();
            crate::seeds::unhex(hex)
                .and_then(|b| b.try_into().ok())
                .expect("a golden object is named by its id")
        };
        let shard_keys = keys_of("index/");
        let snapshots = keys_of("snapshots/")
            .iter()
            .map(|key| {
                let id = id_of(key);
                let bytes = rt.block_on(store.get(key)).expect("read a golden snapshot");
                let keys = repo.read_keys();
                let snapshot = open_snapshot(keys.hpke_private_key(), keys.repo_id(), &id, &bytes)
                    .expect("open a golden snapshot");
                (id, snapshot)
            })
            .collect();
        Golden {
            shards: shard_keys.iter().map(|k| id_of(k)).collect(),
            packs: keys_of("packs/"),
            shard_keys,
            snapshots,
            rt,
            store,
            repo,
        }
    })
}

pub fn pack_keys() -> &'static [String] {
    &golden().packs
}

pub fn shard_keys() -> &'static [String] {
    &golden().shard_keys
}

struct Overlay<'a> {
    base: &'a LocalFsStore,
    laid: Vec<(String, Bytes)>,
}

impl<'a> Overlay<'a> {
    fn new(base: &'a LocalFsStore) -> Self {
        Overlay {
            base,
            laid: Vec::new(),
        }
    }

    fn with(mut self, key: String, bytes: Vec<u8>) -> Self {
        self.laid.retain(|(k, _)| *k != key);
        self.laid.push((key, bytes.into()));
        self
    }

    fn laid(&self, key: &str) -> Option<&Bytes> {
        self.laid.iter().find(|(k, _)| k == key).map(|(_, b)| b)
    }
}

impl BlobStore for Overlay<'_> {
    async fn put_if_absent(&self, _key: &str, _data: Bytes) -> Result<(), BlobError> {
        Err(BlobError::Denied {
            message: "the overlay is read-only".into(),
        })
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        match self.laid(key) {
            Some(bytes) => Ok(bytes.clone()),
            None => self.base.get(key).await,
        }
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, BlobError> {
        let Some(bytes) = self.laid(key) else {
            return self.base.get_range(key, range).await;
        };
        let len = bytes.len() as u64;
        let start = range.start.min(len);
        let end = range.end.min(len).max(start);
        Ok(bytes.slice(start as usize..end as usize))
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        let mut out = self.base.list(prefix).await?;
        for (key, bytes) in self.laid.iter().filter(|(k, _)| k.starts_with(prefix)) {
            out.retain(|e| e.key != *key);
            out.push(ListEntry {
                key: key.clone(),
                size: bytes.len() as u64,
                modified_ms: None,
            });
        }
        Ok(out)
    }
}

fn pick<T>(items: &[T], byte: u8) -> Option<&T> {
    items.get(usize::from(byte).checked_rem(items.len())?)
}

fn seal_shard(keys: &ReadKeys, plain: &[u8]) -> Option<(BlobId, Vec<u8>)> {
    let sealed = seal_index_shard(keys.k_idx(), keys.repo_id(), plain, &mut entropy()).ok()?;
    Some((*blake3::hash(&sealed).as_bytes(), sealed))
}

fn sealed_snapshot(keys: &ReadKeys, snapshot: &pb::Snapshot) -> Option<(BlobId, Vec<u8>)> {
    seal_snapshot(
        snapshot,
        keys.hpke_public_key(),
        keys.repo_id(),
        &mut entropy(),
    )
    .ok()
}

pub fn verify_shard(data: &[u8]) {
    let g = golden();
    let keys = g.repo.read_keys();
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    match mode % 3 {
        0 => {
            let Some((&at, body)) = rest.split_first() else {
                return;
            };
            let Some(pack) = pick(&g.packs, at) else {
                return;
            };
            let store = Overlay::new(&g.store).with(pack.clone(), body.to_vec());
            for shard in &g.shards {
                let _ = g.rt.block_on(verify(&store, keys, shard));
            }
        }
        1 => {
            let Some((id, sealed)) = seal_shard(keys, rest) else {
                return;
            };
            let store = Overlay::new(&g.store).with(index_object_key(&id), sealed);
            let _ = g.rt.block_on(verify(&store, keys, &id));
        }
        _ => {
            let Some((&at, body)) = rest.split_first() else {
                return;
            };
            let (Some(key), Some(id)) = (pick(&g.shard_keys, at), pick(&g.shards, at)) else {
                return;
            };
            let store = Overlay::new(&g.store).with(key.clone(), body.to_vec());
            let _ = g.rt.block_on(verify(&store, keys, id));
        }
    }
}

pub fn snapshot_resolves(data: &[u8]) {
    let g = golden();
    let keys = g.repo.read_keys();
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    let mut verified: HashSet<BlobId> = g.shards.iter().copied().collect();
    let mut attested = HashSet::new();
    let mut store = Overlay::new(&g.store);
    let mut subjects: Vec<BlobId> = Vec::new();
    match mode % 5 {
        0 => {
            let Some((&at, body)) = rest.split_first() else {
                return;
            };
            let Some(pack) = pick(&g.packs, at) else {
                return;
            };
            store = store.with(pack.clone(), body.to_vec());
            subjects.extend(g.snapshots.iter().map(|(id, _)| *id));
        }
        1 | 2 => {
            let Some((&at, body)) = rest.split_first() else {
                return;
            };
            let Some((shard_id, sealed)) = seal_shard(keys, body) else {
                return;
            };
            let Some((_, base)) = pick(&g.snapshots, at) else {
                return;
            };
            let mut snapshot = base.clone();
            snapshot.index_refs.push(shard_id.to_vec());
            let Some((id, bytes)) = sealed_snapshot(keys, &snapshot) else {
                return;
            };
            store = store
                .with(index_object_key(&shard_id), sealed)
                .with(snapshot_object_key(&id), bytes);
            if mode % 5 == 1 {
                verified.insert(shard_id);
            } else {
                attested.insert(shard_id);
            }
            subjects.push(id);
        }
        3 => {
            let Ok(snapshot) = pb::Snapshot::decode(rest) else {
                return;
            };
            let Some((id, bytes)) = sealed_snapshot(keys, &snapshot) else {
                return;
            };
            store = store.with(snapshot_object_key(&id), bytes);
            subjects.push(id);
        }
        _ => {
            let id = *blake3::hash(rest).as_bytes();
            store = store.with(snapshot_object_key(&id), rest.to_vec());
            subjects.push(id);
        }
    }
    for id in &subjects {
        let _ = g.rt.block_on(check_snapshot_resolves(
            &store, keys, id, &verified, &attested,
        ));
    }
}
