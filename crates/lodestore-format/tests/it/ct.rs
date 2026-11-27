use std::fs;
use std::os::fd::OwnedFd;
use std::path::Path;

use lodestore_format::Error;
use lodestore_format::crypto::{BLOB_TYPE_DATA, BlobId, Entropy};
use lodestore_format::ct::{
    CtMeta, CtMode, CtSnapshot, IdKind, IdMap, IdRange, MAX_CT_CONFIG_BYTES, META_CT_LAYOUT,
    META_CT_MODE,
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

pub(crate) async fn init_repo(dir: &Path) -> LocalFsStore {
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

pub(crate) async fn writer(store: &LocalFsStore) -> Writer<&LocalFsStore> {
    let repo = Repository::open(store, b"pw").await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    Writer::new(store, keys, repo.config()).await.unwrap()
}

pub(crate) async fn reader(store: &LocalFsStore) -> Reader<&LocalFsStore> {
    let repo = Repository::open(store, b"pw").await.unwrap();
    Reader::open(store, repo.read_keys().duplicate(), repo.config()).unwrap()
}

pub(crate) fn key() -> SourceKey {
    SourceKey::proxmox_ct("cluster", GUEST).unwrap()
}

pub(crate) fn open_dir(path: &Path) -> OwnedFd {
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

/// vzdump adds `--exclude=lost+found` only when its tar runs in the
/// container's user namespace, that is for a container with an id map.
#[tokio::test]
async fn ct_walk_skips_lost_found_and_sockets_like_vzdump() {
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    let one = |kind, host| IdRange {
        kind,
        ct: 0,
        host,
        count: 1,
    };
    let mapped = CtMeta {
        mode: CtMode::Snapshot,
        unprivileged: true,
        idmap: Some(IdMap::new(vec![one(IdKind::User, uid), one(IdKind::Group, gid)]).unwrap()),
    };
    for (meta, skipped) in [(privileged(), false), (mapped, true)] {
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
        let p = pending(&mut w, &root, backup(meta)).await;
        let files = if skipped { 3 } else { 4 };
        assert_eq!(p.stats().files_new, files, "skipped: {skipped}");
        let id = p.commit(&mut w).await.unwrap().snapshot_id;
        let mut r = reader(&store).await;
        let names = |nodes: Vec<pb::Node>| nodes.into_iter().map(|n| n.name).collect::<Vec<_>>();
        let top = names(r.list_dir(&id, b"/fs").await.unwrap());
        assert_eq!(top.contains(&b"lost+found".to_vec()), !skipped, "{top:?}");
        assert!(top.contains(&b"data".to_vec()));
        let data = names(r.list_dir(&id, b"/fs/data").await.unwrap());
        assert_eq!(data.contains(&b"lost+found".to_vec()), !skipped, "{data:?}");
        assert!(r.list_dir(&id, b"/fs/run").await.unwrap().is_empty());
        let manifest = r.snapshot(&id).await.unwrap();
        assert_eq!(
            manifest.warnings_total, 0,
            "sockets are skipped without a warning"
        );
    }
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

use lodestore_format::fs::idmap::{
    ACL_ACCESS, CAPABILITY, Direction, OVERFLOW_ID, shift_node, translate_acl, translate_caps,
    translate_id,
};
use proptest::prelude::*;

const ACL_USER_OBJ: u16 = 0x01;
const ACL_USER: u16 = 0x02;
const ACL_GROUP_OBJ: u16 = 0x04;
const ACL_GROUP: u16 = 0x08;
const ACL_MASK: u16 = 0x10;
const ACL_OTHER: u16 = 0x20;
const UNDEFINED: u32 = u32::MAX;

fn acl(entries: &[(u16, u16, u32)]) -> Vec<u8> {
    let mut out = 2u32.to_le_bytes().to_vec();
    for &(tag, perm, id) in entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&perm.to_le_bytes());
        out.extend_from_slice(&id.to_le_bytes());
    }
    out
}

fn parse_acl(blob: &[u8]) -> Vec<(u16, u16, u32)> {
    assert_eq!(blob.get(..4), Some(&2u32.to_le_bytes()[..]));
    let entries = &blob[4..];
    assert_eq!(entries.len() % 8, 0);
    entries
        .chunks(8)
        .map(|e| {
            (
                u16::from_le_bytes([e[0], e[1]]),
                u16::from_le_bytes([e[2], e[3]]),
                u32::from_le_bytes([e[4], e[5], e[6], e[7]]),
            )
        })
        .collect()
}

fn caps_v3(rootid: u32) -> Vec<u8> {
    let mut out = 0x0300_0001u32.to_le_bytes().to_vec();
    out.extend_from_slice(&(1u32 << 13).to_le_bytes());
    out.extend_from_slice(&[0; 12]);
    out.extend_from_slice(&rootid.to_le_bytes());
    out
}

/// Maps whose ranges of one kind never overlap on either side.
fn arb_map() -> impl Strategy<Value = IdMap> {
    let side = || prop::collection::vec((0u32..5_000, 0u32..5_000, 1u32..70_000), 1..4);
    (side(), side(), 0u32..1 << 30, 0u32..1 << 30).prop_map(|(u, g, ub, gb)| {
        let mut ranges = Vec::new();
        for (kind, list, base) in [(IdKind::User, u, ub), (IdKind::Group, g, gb)] {
            let (mut ct, mut host) = (0u32, base);
            for (gap_ct, gap_host, count) in list {
                ct += gap_ct;
                host += gap_host;
                ranges.push(IdRange {
                    kind,
                    ct,
                    host,
                    count,
                });
                ct += count;
                host += count;
            }
        }
        IdMap::new(ranges).unwrap()
    })
}

proptest! {
    #[test]
    fn ct_idmap_roundtrip(
        map in arb_map(),
        pick in any::<prop::sample::Index>(),
        off in any::<u32>(),
        stray in any::<u32>(),
    ) {
        let r = *pick.get(map.ranges());
        let ct = r.ct + off % r.count;
        let host = map.to_host(r.kind, ct).unwrap();
        prop_assert_eq!(host, r.host + off % r.count);
        prop_assert_eq!(map.to_ct(r.kind, host), Some(ct));
        prop_assert_eq!(
            IdMap::parse_canonical(&map.canonical().unwrap()).unwrap(),
            map.clone()
        );
        let mut unmapped = 0;
        let id = translate_id(&map, r.kind, Direction::HostToCt, stray, &mut unmapped);
        match map.to_ct(r.kind, stray) {
            Some(c) => {
                prop_assert_eq!(id, c);
                prop_assert_eq!(unmapped, 0);
            }
            None => {
                prop_assert_eq!(id, OVERFLOW_ID);
                prop_assert_eq!(unmapped, 1);
            }
        }
    }

    #[test]
    fn ct_acl_translate(
        named in prop::collection::vec((prop::bool::ANY, 0u16..8, 0u32..70_000), 0..12),
        cut in 1usize..64,
    ) {
        let map = IdMap::default_unprivileged();
        let mut entries = vec![(ACL_USER_OBJ, 6, UNDEFINED)];
        for &(user, perm, id) in &named {
            entries.push((if user { ACL_USER } else { ACL_GROUP }, perm, 100_000 + id));
        }
        entries.extend([
            (ACL_GROUP_OBJ, 4, UNDEFINED),
            (ACL_MASK, 6, UNDEFINED),
            (ACL_OTHER, 4, UNDEFINED),
        ]);
        let blob = acl(&entries);
        let mut unmapped = 0;
        let out = translate_acl(&blob, Direction::HostToCt, &map, &mut unmapped).unwrap();
        let parsed = parse_acl(&out);
        prop_assert_eq!(parsed.len(), entries.len());
        let mut expect_unmapped = 0;
        for (got, want) in parsed.iter().zip(&entries) {
            prop_assert_eq!((got.0, got.1), (want.0, want.1));
            if want.0 == ACL_USER || want.0 == ACL_GROUP {
                let id = want.2 - 100_000;
                if id < 65_536 {
                    prop_assert_eq!(got.2, id);
                } else {
                    prop_assert_eq!(got.2, OVERFLOW_ID);
                    expect_unmapped += 1;
                }
            } else {
                prop_assert_eq!(got.2, UNDEFINED);
            }
        }
        prop_assert_eq!(unmapped, expect_unmapped);
        let back = translate_acl(&out, Direction::CtToHost, &map, &mut 0).unwrap();
        if expect_unmapped == 0 {
            prop_assert_eq!(back, blob.clone());
        }
        if cut % 8 != 0 && cut < blob.len() {
            let short = &blob[..blob.len() - cut];
            prop_assert!(translate_acl(short, Direction::HostToCt, &map, &mut 0).is_err());
        }
        let mut v3 = blob.clone();
        v3[0] = 3;
        prop_assert!(translate_acl(&v3, Direction::HostToCt, &map, &mut 0).is_err());
        let header = &blob[..cut % 4];
        prop_assert!(translate_acl(header, Direction::HostToCt, &map, &mut 0).is_err());
    }
}

#[test]
fn ct_acl_entry_outside_map() {
    let map = IdMap::default_unprivileged();
    let blob = acl(&[
        (ACL_USER_OBJ, 6, UNDEFINED),
        (ACL_USER, 4, 101_000),
        (ACL_USER, 4, 300_000),
        (ACL_GROUP_OBJ, 4, UNDEFINED),
        (ACL_MASK, 4, UNDEFINED),
        (ACL_OTHER, 4, UNDEFINED),
    ]);
    let mut unmapped = 0;
    let out = translate_acl(&blob, Direction::HostToCt, &map, &mut unmapped).unwrap();
    let parsed = parse_acl(&out);
    assert_eq!(parsed[1], (ACL_USER, 4, 1000));
    assert_eq!(parsed[2], (ACL_USER, 4, OVERFLOW_ID));
    assert_eq!(parsed.len(), 6, "the blob is kept");
    assert_eq!(unmapped, 1);
}

#[test]
fn ct_caps_v3_rootid() {
    let map = IdMap::default_unprivileged();
    let mut unmapped = 0;
    let v2 = translate_caps(&caps_v3(100_000), Direction::HostToCt, &map, &mut unmapped)
        .unwrap()
        .unwrap();
    assert_eq!(v2.len(), 20);
    assert_eq!(&v2[..4], &0x0200_0001u32.to_le_bytes());
    assert_eq!(&v2[4..20], &caps_v3(100_000)[4..20]);
    let v3 = translate_caps(&caps_v3(100_005), Direction::HostToCt, &map, &mut unmapped)
        .unwrap()
        .unwrap();
    assert_eq!(v3, caps_v3(5));
    assert_eq!(unmapped, 0);
    assert_eq!(
        translate_caps(&caps_v3(7), Direction::HostToCt, &map, &mut unmapped).unwrap(),
        None
    );
    assert_eq!(unmapped, 1);
    // A version 2 set read on the node belongs to the node's root.
    assert_eq!(
        translate_caps(&v2, Direction::HostToCt, &map, &mut unmapped).unwrap(),
        None
    );
    assert_eq!(
        translate_caps(&v2, Direction::CtToHost, &map, &mut unmapped).unwrap(),
        Some(caps_v3(100_000))
    );
    assert!(translate_caps(&v2[..19], Direction::HostToCt, &map, &mut 0).is_err());
    assert!(translate_caps(&[1, 0, 0, 1], Direction::HostToCt, &map, &mut 0).is_err());
}

#[test]
fn ct_restore_shift() {
    let map = IdMap::default_unprivileged();
    let to_ct = translate_caps(&caps_v3(100_000), Direction::HostToCt, &map, &mut 0)
        .unwrap()
        .unwrap();
    let xattr = |name: &[u8], value: Vec<u8>| pb::Xattr {
        name: name.to_vec(),
        value,
    };
    let mut node = pb::Node {
        uid: 5,
        gid: 70_000,
        xattrs: vec![
            xattr(
                ACL_ACCESS,
                acl(&[(ACL_USER, 4, 1000), (ACL_GROUP, 4, 1001)]),
            ),
            xattr(CAPABILITY, to_ct),
            xattr(b"system.posix_acl_default", vec![1, 2, 3]),
            xattr(b"user.x", vec![1, 2, 3]),
        ],
        ..Default::default()
    };
    let mut unmapped = 0;
    let dropped = shift_node(&mut node, Direction::CtToHost, &map, &mut unmapped);
    assert_eq!(node.uid, 100_005);
    assert_eq!(node.gid, OVERFLOW_ID);
    assert_eq!(unmapped, 1);
    assert_eq!(
        parse_acl(&node.xattrs[0].value),
        [(ACL_USER, 4, 101_000), (ACL_GROUP, 4, 101_001)]
    );
    assert_eq!(node.xattrs[1].value, caps_v3(100_000));
    assert_eq!(node.xattrs[2].name, b"user.x");
    assert_eq!(node.xattrs.len(), 3);
    assert_eq!(dropped.len(), 1);
    assert_eq!(dropped[0].0, b"system.posix_acl_default");
}

/// Without root the walk still applies the map: the test's own ids are
/// mapped to other container ids.
#[tokio::test]
async fn ct_idmap_walk_records_container_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, root) = (tmp.path().join("repo"), tmp.path().join("ct"));
    fs::create_dir(&repo_dir).unwrap();
    container_tree(&root);
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    let one = |kind, ct, host| IdRange {
        kind,
        ct,
        host,
        count: 1,
    };
    let map = IdMap::new(vec![one(IdKind::User, 5, uid), one(IdKind::Group, 7, gid)]).unwrap();
    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;
    let meta = CtMeta {
        mode: CtMode::Stop,
        unprivileged: true,
        idmap: Some(map),
    };
    let p = pending(&mut w, &root, backup(meta.clone())).await;
    assert_eq!(p.unmapped_ids(), 0);
    let id = p.commit(&mut w).await.unwrap().snapshot_id;
    let mut r = reader(&store).await;
    assert_eq!(CtSnapshot::open(&mut r, &id).await.unwrap().meta, meta);
    for n in r.list_dir(&id, b"/fs/etc").await.unwrap() {
        assert_eq!((n.uid, n.gid), (5, 7));
    }
    let top = r.list_dir(&id, b"/").await.unwrap();
    let fs = top.iter().find(|n| n.name == b"fs").unwrap();
    assert_eq!((fs.uid, fs.gid), (5, 7));
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "needs root; run scripts/test-linux.sh --root idmap_root_"]
async fn idmap_root_backup_restore() {
    use std::os::unix::fs::MetadataExt;
    use std::process::Command;

    use lodestore_format::crypto::Key32;
    use lodestore_format::fs::restore::{Overwrite, RestoreSettings, Restorer};
    use rustix::fs::{XattrFlags, getxattr, setxattr};

    assert!(rustix::process::geteuid().is_root(), "needs root");
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, root) = (tmp.path().join("repo"), tmp.path().join("ct"));
    fs::create_dir(&repo_dir).unwrap();
    fs::create_dir_all(root.join("usr/bin")).unwrap();
    let file = root.join("usr/bin/ping");
    fs::write(&file, b"elf").unwrap();
    std::os::unix::fs::chown(&file, Some(100_005), Some(100_005)).unwrap();
    let status = Command::new("setfacl")
        .args(["-m", "u:101000:r"])
        .arg(&file)
        .status()
        .unwrap();
    assert!(status.success(), "setfacl");
    let caps = caps_v3(100_000);
    setxattr(&file, "security.capability", &caps, XattrFlags::empty()).unwrap();
    let get = |path: &Path, name: &str| {
        let mut buf = vec![0u8; 256];
        let n = getxattr(path, name, &mut buf).unwrap();
        buf.truncate(n);
        buf
    };
    assert_eq!(get(&file, "security.capability"), caps);

    let store = init_repo(&repo_dir).await;
    let mut w = writer(&store).await;
    let meta = CtMeta {
        mode: CtMode::Stop,
        unprivileged: true,
        idmap: Some(IdMap::default_unprivileged()),
    };
    let id = pending(&mut w, &root, backup(meta))
        .await
        .commit(&mut w)
        .await
        .unwrap()
        .snapshot_id;

    let mut r = reader(&store).await;
    let nodes = r.list_dir(&id, b"/fs/usr/bin").await.unwrap();
    let node = nodes.iter().find(|n| n.name == b"ping").unwrap();
    assert_eq!((node.uid, node.gid), (5, 5));
    let x = |name: &str| {
        node.xattrs
            .iter()
            .find(|x| x.name == name.as_bytes())
            .map(|x| x.value.clone())
    };
    let acl = parse_acl(&x("system.posix_acl_access").expect("ACL captured"));
    assert!(acl.contains(&(0x02, 4, 1000)), "{acl:?}");
    assert_eq!(x("security.capability").map(|c| c.len()), Some(20));

    let mut content = Vec::new();
    r.dump_file(&id, b"/fs/usr/bin/ping", &mut content)
        .await
        .unwrap();
    let target = tmp.path().join("out");
    fs::create_dir(&target).unwrap();
    let settings = RestoreSettings {
        overwrite: Overwrite::Always,
        verify: false,
        dry_run: false,
        k_id: Key32::new([0u8; 32]),
        seed: 0,
        restore_owners: true,
        keep_privileges: true,
    };
    let mut restorer = Restorer::open(&target, settings)
        .unwrap()
        .with_id_shift(IdMap::default_unprivileged());
    let mut sink = restorer.begin_file(node).unwrap().unwrap();
    sink.write(&content).unwrap();
    restorer.finish_file(sink, node).unwrap();
    assert_eq!(restorer.take_unapplied(), []);
    let out = target.join("ping");
    let st = fs::metadata(&out).unwrap();
    assert_eq!((st.uid(), st.gid()), (100_005, 100_005));
    assert_eq!(
        get(&out, "system.posix_acl_access"),
        get(&file, "system.posix_acl_access")
    );
    assert_eq!(get(&out, "security.capability"), caps);
}
