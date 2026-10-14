use std::ops::Range;
use std::path::Path;

use bytes::Bytes;
use hpke::{Kem, Serializable, kem::XWing};
use lodestore_format::crypto::{
    BLOB_TYPE_DATA, BLOB_TYPE_TREE, Entropy, open_frame, open_pack_key,
};
use lodestore_format::index::{open_shard, parse_hex32};
use lodestore_format::keys::KdfCost;
use lodestore_format::pack::{PACK_PREFIX_LEN, pack_object_key, parse_entries};
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::snapshot::open_snapshot;
use lodestore_format::store::{BlobDelete, BlobError, BlobStore, ListEntry, LocalFsStore};
use lodestore_format::writer::{BackupOptions, Writer};

const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

/// A store whose deletes are always refused, like a write-only IAM user.
struct DenyDelete<S>(S);

impl<S: BlobStore> BlobStore for DenyDelete<S> {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<(), BlobError> {
        self.0.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        self.0.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.0.get_range(key, range).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        self.0.list(prefix).await
    }

    async fn ensure_prefixes(&self, prefixes: &[&str]) -> Result<(), BlobError> {
        self.0.ensure_prefixes(prefixes).await
    }
}

impl<S: Send + Sync> BlobDelete for DenyDelete<S> {
    async fn delete(&self, _key: &str) -> Result<(), BlobError> {
        Err(BlobError::Backend {
            message: "AccessDenied".into(),
            temporary: false,
        })
    }
}

#[tokio::test]
async fn keys_passwd_delete_denied_warns() {
    let dir = tempfile::tempdir().unwrap();
    let store = DenyDelete(LocalFsStore::new(dir.path().join("repo")));
    let mut entropy = Entropy::os();
    let options = InitOptions {
        kdf: LOW_KDF,
        ..Default::default()
    };
    PendingInit::generate(&mut entropy, options)
        .unwrap()
        .commit(&store, b"old", &mut entropy)
        .await
        .unwrap();

    let repo = Repository::open(&store, b"old").await.unwrap();
    let outcome = repo
        .change_password(&store, b"new", &mut entropy)
        .await
        .unwrap();

    assert!(!outcome.old_slot_removed);
    assert!(outcome.warning.as_deref().unwrap().contains("AccessDenied"));
    assert_eq!(store.list("keys/").await.unwrap().len(), 3);
    assert!(Repository::open(&store, b"old").await.is_ok());
    assert!(Repository::open(&store, b"new").await.is_ok());
}

/// Hides `BlobDelete` entirely: the type has no delete to call.
struct NoDelete<S>(S);

impl<S: BlobStore> BlobStore for NoDelete<S> {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<(), BlobError> {
        self.0.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        self.0.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.0.get_range(key, range).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        self.0.list(prefix).await
    }
}

async fn init_local(dir: &Path) -> LocalFsStore {
    let store = LocalFsStore::new(dir.join("repo"));
    let mut entropy = Entropy::os();
    let options = InitOptions {
        kdf: LOW_KDF,
        ..Default::default()
    };
    PendingInit::generate(&mut entropy, options)
        .unwrap()
        .commit(&store, b"pw", &mut entropy)
        .await
        .unwrap();
    store
}

fn make_src(dir: &Path) -> std::path::PathBuf {
    let src = dir.join("src");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("a.txt"), b"secret file contents ".repeat(500)).unwrap();
    std::fs::write(src.join("sub/b.txt"), b"another secret").unwrap();
    src
}

/// A Writer type that is built from `S: BlobStore` alone compiles for a store
/// without `BlobDelete`; handing the agent a delete is a type error.
#[tokio::test]
async fn write_only_writer_has_no_delete() {
    let dir = tempfile::tempdir().unwrap();
    let store = init_local(dir.path()).await;
    let src = make_src(dir.path());

    let repo = Repository::open(&store, b"pw").await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    let mut writer = Writer::new(NoDelete(&store), keys, repo.config())
        .await
        .unwrap();
    let outcome = writer
        .backup(
            std::slice::from_ref(&src),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert!(!outcome.incomplete);

    let mut reader = Reader::open(&store, repo.read_keys().duplicate(), repo.config()).unwrap();
    let out = dir.path().join("out");
    reader
        .restore(
            &outcome.snapshot_id,
            &out,
            &RestoreOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    let canonical = src.canonicalize().unwrap();
    let restored = out.join(canonical.strip_prefix("/").unwrap());
    assert_eq!(
        std::fs::read(src.join("a.txt")).unwrap(),
        std::fs::read(restored.join("a.txt")).unwrap()
    );
}

#[tokio::test]
async fn write_only_cannot_decrypt() {
    let dir = tempfile::tempdir().unwrap();
    let store = init_local(dir.path()).await;
    let src = make_src(dir.path());

    let repo = Repository::open(&store, b"pw").await.unwrap();
    let read = repo.read_keys();
    let write = read.write_keys(repo.config_hash());
    let mut writer = Writer::new(&store, read.write_keys(repo.config_hash()), repo.config())
        .await
        .unwrap();
    let outcome = writer
        .backup(&[src], BackupOptions::default(), &|_| {})
        .await
        .unwrap();
    let repo_id = *read.repo_id();

    // Every blob frame of every pack: the real pack key opens it, the agent keys never do.
    let mut data_frames = 0;
    let mut tree_frames = 0;
    for shard in store.list("index/").await.unwrap() {
        let shard_id = parse_hex32(shard.key.strip_prefix("index/").unwrap()).unwrap();
        let bytes = store.get(&shard.key).await.unwrap();
        let opened = open_shard(read.k_idx(), &repo_id, &shard_id, &bytes).unwrap();
        for pack in opened.packs {
            let pack_id = pack.pack_id.as_slice().try_into().unwrap();
            let pack_key = open_pack_key(
                read.hpke_private_key(),
                &repo_id,
                &pack.hpke_enc,
                &pack.sealed_pack_key,
            )
            .unwrap();
            let object = pack_object_key(&pack_id);
            assert!(
                store
                    .get_range(&object, 0..PACK_PREFIX_LEN as u64)
                    .await
                    .is_ok()
            );
            for e in parse_entries(&pack.entries).unwrap() {
                let end = u64::from(e.offset) + u64::from(e.enc_len);
                let frame = store
                    .get_range(&object, u64::from(e.offset)..end)
                    .await
                    .unwrap();
                assert!(open_frame(&pack_key, &repo_id, e.blob_type, &e.blob_id, &frame).is_ok());
                for wrong in [&*write.k_id, &*write.k_idx] {
                    assert!(open_frame(wrong, &repo_id, e.blob_type, &e.blob_id, &frame).is_err());
                }
                match e.blob_type {
                    BLOB_TYPE_DATA => data_frames += 1,
                    BLOB_TYPE_TREE => tree_frames += 1,
                    _ => unreachable!(),
                }
            }
        }
    }
    assert!(data_frames > 0 && tree_frames > 0);

    // The snapshot: the repository key opens it, keys derived from the agent keys do not.
    let snapshot_bytes = store
        .get(&format!(
            "snapshots/{}",
            lodestore_format::index::hex32(&outcome.snapshot_id)
        ))
        .await
        .unwrap();
    assert!(
        open_snapshot(
            read.hpke_private_key(),
            &repo_id,
            &outcome.snapshot_id,
            &snapshot_bytes
        )
        .is_ok()
    );
    let repo_public = repo.config().hpke.as_ref().unwrap().public_key.clone();
    for k in [&*write.k_id, &*write.k_idx] {
        let (private, public) = XWing::derive_keypair(k);
        assert!(open_snapshot(&private, &repo_id, &outcome.snapshot_id, &snapshot_bytes).is_err());
        assert_ne!(public.to_bytes().as_slice(), repo_public.as_slice());
    }
    assert_eq!(
        write.hpke_public.to_bytes().as_slice(),
        repo_public.as_slice()
    );
}
