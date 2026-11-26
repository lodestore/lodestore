use std::fs;
use std::os::fd::OwnedFd;
use std::path::Path;

use lodestore_format::Error;
use lodestore_format::crypto::{BLOB_TYPE_DATA, BlobId, Entropy};
use lodestore_format::ct::{
    CtMeta, CtMode, CtSnapshot, IdMap, MAX_CT_CONFIG_BYTES, META_CT_LAYOUT, META_CT_MODE,
};
use lodestore_format::keys::KdfCost;
use lodestore_format::pack::CODEC_RAW;
use lodestore_format::pb;
use lodestore_format::reader::Reader;
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::source::{self, Meta, SourceKey};
use lodestore_format::store::LocalFsStore;
use lodestore_format::tree::encode_tree;
use lodestore_format::writer::{BackupOptions, CtBackup, PendingContainer, Writer};
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{Rng, SeedableRng};
use rustix::fs::{Mode, OFlags, openat};

const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

const CONFIG: &str = "arch: amd64\nhostname: dns\n";
const GUEST: &str = "0b0e6b8e-6d4f-4c1a-9a43-3f6c4b8e2a10";

async fn init_repo(dir: &Path) -> LocalFsStore {
    let store = LocalFsStore::new(dir.to_path_buf());
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

async fn writer(store: &LocalFsStore) -> Writer<&LocalFsStore> {
    let repo = Repository::open(store, b"pw").await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    Writer::new(store, keys, repo.config()).await.unwrap()
}

async fn reader(store: &LocalFsStore) -> Reader<&LocalFsStore> {
    let repo = Repository::open(store, b"pw").await.unwrap();
    Reader::open(store, repo.read_keys().duplicate(), repo.config()).unwrap()
}

fn key() -> SourceKey {
    SourceKey::proxmox_ct("cluster", GUEST).unwrap()
}

fn open_dir(path: &Path) -> OwnedFd {
    openat(
        rustix::fs::CWD,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .unwrap()
}

/// A small container root: `etc/hostname`, a larger random file and a
/// `lost+found` at the top and below.
fn container_tree(root: &Path) {
    fs::create_dir_all(root.join("etc")).unwrap();
    fs::write(root.join("etc/hostname"), b"dns\n").unwrap();
    fs::create_dir_all(root.join("var/lib")).unwrap();
    let mut data = vec![0u8; 1 << 20];
    ChaCha20Rng::from_seed([7; 32]).fill_bytes(&mut data);
    fs::write(root.join("var/lib/data.bin"), data).unwrap();
}

fn backup(meta: CtMeta) -> CtBackup {
    CtBackup {
        key: key(),
        meta,
        source_meta: Meta::from([("vmid".to_owned(), "200".to_owned())]),
        config: CONFIG.to_owned(),
        firewall: None,
        exclude: None,
    }
}

fn privileged() -> CtMeta {
    CtMeta {
        mode: CtMode::Snapshot,
        unprivileged: false,
        idmap: None,
    }
}

async fn pending(w: &mut Writer<&LocalFsStore>, root: &Path, ct: CtBackup) -> PendingContainer {
    w.backup_container(&open_dir(root), ct, BackupOptions::default(), &|_| {})
        .await
        .unwrap()
}

#[tokio::test]
async fn ct_snapshot_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, root) = (tmp.path().join("repo"), tmp.path().join("ct"));
    fs::create_dir(&repo_dir).unwrap();
    container_tree(&root);
    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;
    let mut ct = backup(privileged());
    ct.firewall = Some("[OPTIONS]\nenable: 1\n".to_owned());
    let p = pending(&mut w, &root, ct).await;
    let outcome = p.commit(&mut w).await.unwrap();

    let mut r = reader(&store).await;
    let id = outcome.snapshot_id;
    let snap = CtSnapshot::open(&mut r, &id).await.unwrap();
    assert_eq!(snap.config, CONFIG);
    assert_eq!(snap.firewall.as_deref(), Some("[OPTIONS]\nenable: 1\n"));
    assert_eq!(snap.meta, privileged());
    let manifest = r.snapshot(&id).await.unwrap();
    assert_eq!(manifest.paths, vec![b"/fs".to_vec()]);
    let (_, meta) = source::decode(manifest.source.as_ref()).unwrap();
    assert_eq!(meta.get("vmid").map(String::as_str), Some("200"));
    let etc = r.list_dir(&id, b"/fs/etc").await.unwrap();
    assert!(etc.iter().any(|n| n.name == b"hostname"));
    let config = r.list_dir(&id, b"/config").await.unwrap();
    assert!(config.iter().any(|n| n.name == b"pct.conf"));
}

#[tokio::test]
async fn ct_snapshot_dedups_with_files() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, root) = (tmp.path().join("repo"), tmp.path().join("ct"));
    fs::create_dir(&repo_dir).unwrap();
    container_tree(&root);
    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;
    let files = w
        .backup(
            std::slice::from_ref(&root),
            BackupOptions::default(),
            &|_| {},
        )
        .await
        .unwrap();
    assert!(files.stats.bytes_added > 1 << 19);
    let p = pending(&mut w, &root, backup(privileged())).await;
    // Only the configuration is new data.
    assert!(
        p.stats().bytes_added < 1024,
        "{} bytes added",
        p.stats().bytes_added
    );
}

#[tokio::test]
async fn ct_owner_names_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, root) = (tmp.path().join("repo"), tmp.path().join("ct"));
    fs::create_dir(&repo_dir).unwrap();
    container_tree(&root);
    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;
    let id = pending(&mut w, &root, backup(privileged()))
        .await
        .commit(&mut w)
        .await
        .unwrap()
        .snapshot_id;
    let mut r = reader(&store).await;
    let mut nodes = r.list_dir(&id, b"/").await.unwrap();
    nodes.extend(r.list_dir(&id, b"/fs").await.unwrap());
    nodes.extend(r.list_dir(&id, b"/fs/etc").await.unwrap());
    for n in &nodes {
        assert!(
            n.user.is_empty() && n.group.is_empty(),
            "{}",
            String::from_utf8_lossy(&n.name)
        );
    }
}

#[tokio::test]
async fn ct_walk_skips_lost_found_and_sockets_like_vzdump() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, root) = (tmp.path().join("repo"), tmp.path().join("ct"));
    fs::create_dir(&repo_dir).unwrap();
    container_tree(&root);
    fs::create_dir_all(root.join("lost+found")).unwrap();
    fs::write(root.join("lost+found/#123"), b"x").unwrap();
    fs::create_dir_all(root.join("data/lost+found")).unwrap();
    fs::create_dir_all(root.join("run")).unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(root.join("run/s")).unwrap();
    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;
    let p = pending(&mut w, &root, backup(privileged())).await;
    assert_eq!(p.stats().files_new, 3, "two files of the tree and pct.conf");
    let id = p.commit(&mut w).await.unwrap().snapshot_id;
    let mut r = reader(&store).await;
    let names = |nodes: Vec<pb::Node>| nodes.into_iter().map(|n| n.name).collect::<Vec<_>>();
    let top = names(r.list_dir(&id, b"/fs").await.unwrap());
    assert!(!top.contains(&b"lost+found".to_vec()), "{top:?}");
    assert!(top.contains(&b"data".to_vec()));
    assert!(r.list_dir(&id, b"/fs/data").await.unwrap().is_empty());
    assert!(r.list_dir(&id, b"/fs/run").await.unwrap().is_empty());
    let manifest = r.snapshot(&id).await.unwrap();
    assert_eq!(
        manifest.warnings_total, 0,
        "sockets are skipped without a warning"
    );
}

#[tokio::test]
async fn ct_excludes_are_anchored_at_the_container_root() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, root) = (tmp.path().join("repo"), tmp.path().join("ct"));
    fs::create_dir(&repo_dir).unwrap();
    container_tree(&root);
    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;
    let mut ct = backup(privileged());
    ct.exclude = Some(lodestore_format::fs::filter::PathMatcher::new(["/etc/hostname"]).unwrap());
    let id = pending(&mut w, &root, ct)
        .await
        .commit(&mut w)
        .await
        .unwrap()
        .snapshot_id;
    let mut r = reader(&store).await;
    assert!(r.list_dir(&id, b"/fs/etc").await.unwrap().is_empty());
}

#[tokio::test]
async fn ct_pending_dropped_writes_no_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, root) = (tmp.path().join("repo"), tmp.path().join("ct"));
    fs::create_dir(&repo_dir).unwrap();
    container_tree(&root);
    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;
    drop(pending(&mut w, &root, backup(privileged())).await);
    let r = reader(&store).await;
    assert!(r.list_snapshots().await.unwrap().entries.is_empty());
}

#[tokio::test]
async fn ct_refuses_bad_input() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, root) = (tmp.path().join("repo"), tmp.path().join("ct"));
    fs::create_dir(&repo_dir).unwrap();
    container_tree(&root);
    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;
    let dir = open_dir(&root);
    let mut cases = Vec::new();
    let mut c = backup(privileged());
    c.key = SourceKey::files();
    cases.push(c);
    let mut c = backup(privileged());
    c.config = "a".repeat(MAX_CT_CONFIG_BYTES + 1);
    cases.push(c);
    let mut c = backup(privileged());
    c.source_meta
        .insert(META_CT_MODE.to_owned(), "stop".to_owned());
    cases.push(c);
    cases.push(backup(CtMeta {
        mode: CtMode::Stop,
        unprivileged: true,
        idmap: None,
    }));
    for c in cases {
        let err = w
            .backup_container(&dir, c, BackupOptions::default(), &|_| {})
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)), "{err:?}");
    }
}

/// A container snapshot written with the raw hooks, so each part can be
/// made hostile on its own.
struct Hostile {
    meta: Meta,
    paths: Vec<Vec<u8>>,
    top_fs: &'static [u8],
    conf: Option<Vec<u8>>,
    /// Claimed size of pct.conf; its length when absent.
    conf_size: Option<u64>,
    extra_config: bool,
}

impl Default for Hostile {
    fn default() -> Self {
        Hostile {
            meta: privileged().to_meta().unwrap(),
            paths: vec![b"/fs".to_vec()],
            top_fs: b"fs",
            conf: Some(CONFIG.as_bytes().to_vec()),
            conf_size: None,
            extra_config: false,
        }
    }
}

fn dir(name: &[u8], subtree: &BlobId) -> pb::Node {
    pb::Node {
        name: name.to_vec(),
        r#type: pb::NodeType::Dir as i32,
        mode: 0o755,
        subtrees: vec![subtree.to_vec()],
        ..Default::default()
    }
}

fn file(name: &[u8], content: &[BlobId], size: u64) -> pb::Node {
    pb::Node {
        name: name.to_vec(),
        r#type: pb::NodeType::File as i32,
        mode: 0o600,
        size,
        content: content.iter().map(|c| c.to_vec()).collect(),
        ..Default::default()
    }
}

async fn hostile(store: &LocalFsStore, h: Hostile) -> BlobId {
    let mut w = writer(store).await;
    let empty = w.put_tree_raw(&encode_tree(Vec::new())).unwrap();
    let mut files = Vec::new();
    if let Some(conf) = &h.conf {
        let mut ids = Vec::new();
        for piece in conf.chunks(4 << 20) {
            ids.push(
                w.put_blob_raw(BLOB_TYPE_DATA, CODEC_RAW, piece, piece.len() as u32)
                    .unwrap(),
            );
        }
        files.push(file(
            b"pct.conf",
            &ids,
            h.conf_size.unwrap_or(conf.len() as u64),
        ));
    }
    if h.extra_config {
        files.push(file(b"vzdump.log", &[], 0));
    }
    let config = w.put_tree_raw(&encode_tree(files)).unwrap();
    let root = w
        .put_tree_raw(&encode_tree(vec![
            dir(b"config", &config),
            dir(h.top_fs, &empty),
        ]))
        .unwrap();
    let claim = source::encode(&key(), &h.meta).unwrap();
    w.commit_snapshot_raw_with(root, h.paths, claim)
        .await
        .unwrap()
}

#[tokio::test]
async fn ct_snapshot_refuses_hostile() {
    let tmp = tempfile::tempdir().unwrap();
    let store = init_repo(tmp.path()).await;
    let ok = hostile(&store, Hostile::default()).await;
    let mut r = reader(&store).await;
    assert_eq!(CtSnapshot::open(&mut r, &ok).await.unwrap().config, CONFIG);

    let with_meta = |key: &str, value: &str| {
        let mut meta = privileged().to_meta().unwrap();
        meta.insert(key.to_owned(), value.to_owned());
        Hostile {
            meta,
            ..Hostile::default()
        }
    };
    let cases = [
        ("layout 2", with_meta(META_CT_LAYOUT, "2")),
        ("mode fast", with_meta(META_CT_MODE, "fast")),
        (
            "unprivileged without map",
            with_meta("ct_unprivileged", "1"),
        ),
        (
            "non-canonical map",
            with_meta("ct_idmap", "g:0:100000:65536,u:0:100000:65536"),
        ),
        (
            "path /etc",
            Hostile {
                paths: vec![b"/etc".to_vec()],
                ..Hostile::default()
            },
        ),
        (
            "top /etc",
            Hostile {
                top_fs: b"etc",
                ..Hostile::default()
            },
        ),
        (
            "no pct.conf",
            Hostile {
                conf: None,
                ..Hostile::default()
            },
        ),
        (
            "pct.conf 64 KiB + 1",
            Hostile {
                conf: Some(vec![b'a'; MAX_CT_CONFIG_BYTES + 1]),
                ..Hostile::default()
            },
        ),
        (
            "pct.conf larger than claimed",
            Hostile {
                conf: Some(vec![b'a'; MAX_CT_CONFIG_BYTES + 1]),
                conf_size: Some(10),
                ..Hostile::default()
            },
        ),
        (
            "extra file in /config",
            Hostile {
                extra_config: true,
                ..Hostile::default()
            },
        ),
        (
            "pct.conf not UTF-8",
            Hostile {
                conf: Some(vec![0xff, 0xfe]),
                ..Hostile::default()
            },
        ),
    ];
    for (what, case) in cases {
        let id = hostile(&store, case).await;
        let mut r = reader(&store).await;
        let err = CtSnapshot::open(&mut r, &id).await.unwrap_err();
        assert!(
            matches!(err, Error::Corrupt { .. } | Error::InvalidInput(_)),
            "{what}: {err:?}"
        );
    }
}

#[test]
fn ct_meta_roundtrip() {
    for mode in CtMode::ALL {
        for meta in [
            CtMeta {
                mode,
                unprivileged: false,
                idmap: None,
            },
            CtMeta {
                mode,
                unprivileged: true,
                idmap: Some(IdMap::default_unprivileged()),
            },
        ] {
            assert_eq!(CtMeta::from_meta(&meta.to_meta().unwrap()).unwrap(), meta);
        }
    }
}
