//! The frozen format v1 repository in `tests/fixtures/v1`. These tests read
//! what an earlier build wrote, so any change to the encoding, the chunker, the
//! ciphers or the compression that old repositories would not survive makes
//! them fail. Fixtures are append-only; a format change needs `format_version`
//! 2 and a `fixtures/v2` next to this one.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use data_encoding::HEXLOWER;
use lodestore_format::crypto::{
    BLOB_TYPE_TREE, open_index_shard, open_snapshot_body, open_snapshot_key,
};
use lodestore_format::fs::filter::PathMatcher;
use lodestore_format::index::{hex32, parse_hex32};
use lodestore_format::keys::{RecoveryKey, WriteKeys};
use lodestore_format::pack::parse_entries;
use lodestore_format::pb;
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::repo::Repository;
use lodestore_format::store::{BlobStore, LocalFsStore};
use prost::Message;
use serde_json::{Value, json};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1")
}

fn text(name: &str) -> String {
    fs::read_to_string(fixtures().join(name))
        .unwrap()
        .trim_end()
        .to_owned()
}

fn hex(bytes: &[u8]) -> String {
    HEXLOWER.encode(bytes)
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// The frozen repository, copied so that nothing can write to the fixtures.
fn repo_copy() -> (tempfile::TempDir, LocalFsStore) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    copy_dir(&fixtures().join("repo"), &repo);
    (tmp, LocalFsStore::new(repo))
}

async fn open_reader(store: &LocalFsStore) -> Reader<&LocalFsStore> {
    let repo = Repository::open(store, text("PASSWORD").as_bytes())
        .await
        .unwrap();
    Reader::open(store, repo.read_keys().duplicate(), repo.config()).unwrap()
}

fn timestamp(t: Option<&pb::Timestamp>) -> Value {
    t.map_or(Value::Null, |t| json!([t.seconds, t.nanos]))
}

/// A node as `expected.json` records it. Inode, device, ctime and owner names
/// are left out: they describe the machine the fixtures were made on.
fn node_json(path: &[u8], n: &pb::Node) -> Value {
    json!({
        "path": hex(path),
        "type": pb::NodeType::try_from(n.r#type).unwrap().as_str_name(),
        "mode": n.mode,
        "uid": n.uid,
        "gid": n.gid,
        "mtime": timestamp(n.mtime.as_ref()),
        "size": n.size,
        "link_target": hex(&n.link_target),
        "xattrs": n.xattrs.iter().map(|x| json!([hex(&x.name), hex(&x.value)])).collect::<Vec<_>>(),
        "sparse_extents": n.sparse_extents.iter().map(|e| json!([e.offset, e.length])).collect::<Vec<_>>(),
        "hardlink_group": n.hardlink_group,
    })
}

async fn snapshot_json(
    reader: &mut Reader<&LocalFsStore>,
    id: &[u8; 32],
    snapshot: &pb::Snapshot,
    hashes: Option<&BTreeMap<Vec<u8>, String>>,
) -> Value {
    let mut nodes = Vec::new();
    reader
        .walk_subtree(id, b"", &mut |path, node| {
            let mut v = node_json(path, node);
            if let Some(hashes) = hashes
                && node.r#type == pb::NodeType::File as i32
            {
                v["blake3"] = json!(hashes[path]);
            }
            nodes.push(v);
            Ok(())
        })
        .await
        .unwrap();
    json!({
        "id": hex32(id),
        "time": timestamp(snapshot.time.as_ref()),
        "hostname": hex(&snapshot.hostname),
        "tags": snapshot.tags,
        "paths": snapshot.paths.iter().map(|p| hex(p)).collect::<Vec<_>>(),
        "parent": hex(&snapshot.parent),
        "nodes": nodes,
    })
}

fn expected() -> Vec<Value> {
    let v: Value = serde_json::from_str(&text("expected.json")).unwrap();
    v["snapshots"].as_array().unwrap().clone()
}

fn without_hashes(mut snapshot: Value) -> Value {
    for node in snapshot["nodes"].as_array_mut().unwrap() {
        node.as_object_mut().unwrap().remove("blake3");
    }
    snapshot
}

async fn actual_snapshots(store: &LocalFsStore) -> Vec<Value> {
    let mut reader = open_reader(store).await;
    let mut out = Vec::new();
    for entry in reader.list_snapshots().await.unwrap().entries {
        out.push(snapshot_json(&mut reader, &entry.id, &entry.snapshot, None).await);
    }
    out
}

#[tokio::test]
async fn golden_v1_repo_reads() {
    let (_tmp, store) = repo_copy();
    let want: Vec<Value> = expected().into_iter().map(without_hashes).collect();
    assert_eq!(want.len(), 2);
    assert_eq!(actual_snapshots(&store).await, want);

    let recovery = RecoveryKey::parse(&text("RECOVERY_KEY")).unwrap();
    let repo = Repository::open_recovery(&store, &recovery).await.unwrap();
    let reader = Reader::open(&store, repo.read_keys().duplicate(), repo.config()).unwrap();
    let ids: Vec<String> = reader
        .list_snapshots()
        .await
        .unwrap()
        .entries
        .iter()
        .map(|e| hex32(&e.id))
        .collect();
    let want_ids: Vec<&str> = want.iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert_eq!(ids, want_ids);

    assert!(
        Repository::open(&store, b"not the password").await.is_err(),
        "a wrong password must not open the repository"
    );
}

#[tokio::test]
async fn golden_v1_restore_contents() {
    let (_tmp, store) = repo_copy();
    let mut reader = open_reader(&store).await;
    // APFS refuses a name that is not valid UTF-8, so a macOS host leaves that
    // directory out; Linux restores everything.
    let include = (!cfg!(target_os = "linux")).then(|| {
        PathMatcher::new(["/lodestore-golden/src/*", "!/lodestore-golden/src/weird"]).unwrap()
    });
    let weird = hex(b"/lodestore-golden/src/weird/");

    for want in expected() {
        let id = parse_hex32(want["id"].as_str().unwrap()).unwrap();
        let target = tempfile::tempdir().unwrap();
        let report = reader
            .restore(
                &id,
                target.path(),
                &RestoreOptions {
                    include: include.clone(),
                    ..Default::default()
                },
                &|_| {},
            )
            .await
            .unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);

        let mut checked = 0;
        for node in want["nodes"].as_array().unwrap() {
            let Some(expected_hash) = node["blake3"].as_str() else {
                continue;
            };
            let path_hex = node["path"].as_str().unwrap();
            if !cfg!(target_os = "linux") && path_hex.starts_with(&weird) {
                continue;
            }
            let path = HEXLOWER.decode(path_hex.as_bytes()).unwrap();
            let restored = target.path().join(OsStr::from_bytes(&path[1..]));
            let got = blake3::hash(&fs::read(&restored).unwrap());
            assert_eq!(got.to_hex().as_str(), expected_hash, "{restored:?}");
            checked += 1;
        }
        assert!(checked >= 10, "only {checked} files were checked");
    }
}

#[tokio::test]
async fn golden_v1_reencode_identical() {
    let (_tmp, store) = repo_copy();
    let password = text("PASSWORD");
    let repo = Repository::open(&store, password.as_bytes()).await.unwrap();
    let keys = repo.read_keys();

    let config = fs::read(fixtures().join("repo/config")).unwrap();
    assert_eq!(
        pb::Config::decode(&*config).unwrap().encode_to_vec(),
        config
    );

    let slots = store.list("keys/").await.unwrap();
    assert_eq!(slots.len(), 2, "one password slot and one recovery slot");
    for slot in slots {
        let bytes = store.get(&slot.key).await.unwrap();
        assert_eq!(
            pb::KeySlot::decode(&*bytes).unwrap().encode_to_vec(),
            bytes.as_ref(),
            "{}",
            slot.key
        );
    }

    let mut tree_ids = Vec::new();
    let shards = store.list("index/").await.unwrap();
    assert!(!shards.is_empty());
    for shard in shards {
        let sealed = store.get(&shard.key).await.unwrap();
        let plain = open_index_shard(keys.k_idx(), keys.repo_id(), &sealed).unwrap();
        let decoded = pb::IndexShard::decode(&*plain).unwrap();
        assert_eq!(decoded.encode_to_vec(), plain, "{}", shard.key);
        for pack in &decoded.packs {
            for entry in parse_entries(&pack.entries).unwrap() {
                if entry.blob_type == BLOB_TYPE_TREE {
                    tree_ids.push(entry.blob_id);
                }
            }
        }
    }
    assert!(tree_ids.len() >= 8, "{} tree blobs", tree_ids.len());
    let mut reader = open_reader(&store).await;
    for id in tree_ids {
        let plain = reader.read_blob(&id, BLOB_TYPE_TREE).await.unwrap();
        assert_eq!(
            pb::Tree::decode(&*plain).unwrap().encode_to_vec(),
            plain,
            "tree {}",
            hex32(&id)
        );
    }

    let snapshots = store.list("snapshots/").await.unwrap();
    assert_eq!(snapshots.len(), 2);
    for snapshot in snapshots {
        let bytes = store.get(&snapshot.key).await.unwrap();
        let envelope = pb::SnapshotEnvelope::decode(&*bytes).unwrap();
        assert_eq!(envelope.encode_to_vec(), bytes.as_ref());
        let key = open_snapshot_key(
            keys.hpke_private_key(),
            keys.repo_id(),
            &envelope.hpke_enc,
            &envelope.sealed_key,
        )
        .unwrap();
        let sealed = [envelope.nonce.as_slice(), envelope.ciphertext.as_slice()].concat();
        let body = open_snapshot_body(&key, keys.repo_id(), &sealed).unwrap();
        assert_eq!(
            pb::Snapshot::decode(&*body).unwrap().encode_to_vec(),
            body,
            "{}",
            snapshot.key
        );
    }
}

#[tokio::test]
async fn golden_v1_writer_keys_file_parses() {
    let (_tmp, store) = repo_copy();
    let file = fs::read_to_string(fixtures().join("writer-keys.txt")).unwrap();
    let keys = WriteKeys::from_file_text(&file).unwrap();
    let config = fs::read(fixtures().join("repo/config")).unwrap();
    keys.verify_config(&config).unwrap();

    let repo = Repository::open(&store, text("PASSWORD").as_bytes())
        .await
        .unwrap();
    let derived = repo.read_keys().write_keys(repo.config_hash());
    assert_eq!(derived.to_file_text().as_str(), file);
    assert_eq!(keys.cdc_seed, derived.cdc_seed);
}

#[cfg(target_os = "linux")]
mod generate {
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    use lodestore_format::crypto::Entropy;
    use lodestore_format::keys::KdfCost;
    use lodestore_format::repo::{InitOptions, PendingInit};
    use lodestore_format::snapshot::ParentTree;
    use lodestore_format::writer::{BackupOptions, Writer};
    use rand_chacha::ChaCha20Rng;
    use rand_chacha::rand_core::{Rng, SeedableRng};
    use rustix::fs::{
        AtFlags, CWD, FileType, Gid, Mode, Timespec, Timestamps, Uid, mknodat, utimensat,
    };

    use super::*;

    const BASE: &str = "/lodestore-golden";
    const MIB: usize = 1 << 20;

    fn rng(seed: u8) -> ChaCha20Rng {
        ChaCha20Rng::from_seed([seed; 32])
    }

    fn entropy(seed: u8) -> Entropy {
        Entropy::from_rng(rng(seed))
    }

    fn write(path: &Path, data: &[u8], mode: u32) {
        fs::write(path, data).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn random(seed: u8, len: usize) -> Vec<u8> {
        let mut data = vec![0u8; len];
        rng(seed).fill_bytes(&mut data);
        data
    }

    fn build_source(src: &Path) {
        let _ = fs::remove_dir_all(BASE);
        for dir in ["a/b/c", "empty", "links", "weird"] {
            fs::create_dir_all(src.join(dir)).unwrap();
        }
        write(&src.join("empty.txt"), b"", 0o644);
        write(&src.join("hello.txt"), b"hello, golden v1\n", 0o644);
        rustix::fs::setxattr(
            src.join("hello.txt"),
            "user.golden",
            b"golden-v1",
            rustix::fs::XattrFlags::empty(),
        )
        .unwrap();
        write(&src.join("a/random.bin"), &random(21, 3 * MIB), 0o600);
        write(&src.join("a/noise.bin"), &random(22, 100 << 10), 0o640);
        write(&src.join("a/b/zeros.bin"), &vec![0u8; MIB], 0o644);
        let words = b"lodestore golden repository ".repeat(8 << 10);
        write(&src.join("a/b/c/words.txt"), &words, 0o644);
        symlink("hello.txt", src.join("link-to-hello")).unwrap();
        write(&src.join("links/first"), b"one inode, two names\n", 0o644);
        fs::hard_link(src.join("links/first"), src.join("links/second")).unwrap();
        let odd = OsStr::from_bytes(b"not-utf8-\xff");
        write(&src.join("weird").join(odd), b"odd name\n", 0o644);

        let mut sparse = fs::File::create(src.join("sparse.bin")).unwrap();
        sparse.write_all(&random(23, MIB)).unwrap();
        sparse.set_len(66 * MIB as u64).unwrap();
        std::os::unix::fs::FileExt::write_all_at(&sparse, &random(24, MIB), 65 * MIB as u64)
            .unwrap();
        drop(sparse);

        mknodat(
            CWD,
            src.join("fifo"),
            FileType::Fifo,
            Mode::from_raw_mode(0o640),
            0,
        )
        .unwrap();
        write(&src.join("suid"), b"#!/bin/sh\n", 0o4755);
        write(&src.join("owned"), b"owned by 1234\n", 0o640);
        rustix::fs::chown(
            src.join("owned"),
            Some(Uid::from_raw(1234)),
            Some(Gid::from_raw(1234)),
        )
        .unwrap();
        fs::set_permissions(src.join("owned"), fs::Permissions::from_mode(0o640)).unwrap();
    }

    fn paths_below(dir: &Path, out: &mut Vec<PathBuf>) {
        out.push(dir.to_path_buf());
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        names.sort();
        for p in names {
            if fs::symlink_metadata(&p).unwrap().is_dir() {
                paths_below(&p, out);
            } else {
                out.push(p);
            }
        }
    }

    /// Every path below the source gets its own fixed times, atime after mtime.
    fn stamp(base: &Path) {
        let mut paths = Vec::new();
        paths_below(base, &mut paths);
        for (i, p) in paths.iter().enumerate() {
            let i = i as i64;
            let m = Timespec {
                tv_sec: 1_600_000_000 + i * 1000,
                tv_nsec: 100_000_000 + i * 7919,
            };
            let a = Timespec {
                tv_sec: m.tv_sec + 500,
                tv_nsec: m.tv_nsec + 1,
            };
            utimensat(
                CWD,
                p,
                &Timestamps {
                    last_access: a,
                    last_modification: m,
                },
                AtFlags::SYMLINK_NOFOLLOW,
            )
            .unwrap();
        }
    }

    fn hash_files(base: &Path) -> BTreeMap<Vec<u8>, String> {
        let mut paths = Vec::new();
        paths_below(base, &mut paths);
        paths
            .into_iter()
            .filter(|p| fs::symlink_metadata(p).unwrap().is_file())
            .map(|p| {
                let hash = blake3::hash(&fs::read(&p).unwrap()).to_hex().to_string();
                (p.as_os_str().as_bytes().to_vec(), hash)
            })
            .collect()
    }

    fn options(time: i64, parent: Option<ParentTree<'_>>) -> BackupOptions<'_> {
        BackupOptions {
            hostname: Some(b"golden-host".to_vec()),
            tags: vec!["golden".into()],
            time: Some(pb::Timestamp {
                seconds: time,
                nanos: 0,
            }),
            parent,
            ..Default::default()
        }
    }

    fn create_new(path: &Path, data: &[u8]) {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        f.write_all(data).unwrap();
    }

    fn at(nodes: &[(Vec<u8>, pb::Node)], path: &str) -> pb::Node {
        nodes
            .iter()
            .find(|(p, _)| p == path.as_bytes())
            .unwrap_or_else(|| panic!("{path} is not in the snapshot"))
            .1
            .clone()
    }

    #[tokio::test]
    #[ignore = "generates frozen fixtures; run once via LODE_GENERATE_FIXTURES=1 scripts/test-linux.sh --root golden_generate_v1"]
    async fn golden_generate_v1() {
        if std::env::var_os("LODE_GENERATE_FIXTURES").is_none() {
            eprintln!("skipped: set LODE_GENERATE_FIXTURES=1 to generate fixtures");
            return;
        }
        let root = fixtures();
        let repo_dir = root.join("repo");
        assert!(
            !repo_dir.exists(),
            "{} exists: the v1 fixtures are frozen and are never regenerated",
            repo_dir.display()
        );
        assert_eq!(fs::metadata("/proc/self").unwrap().uid(), 0, "run as root");
        let password = text("PASSWORD");

        let src = Path::new(BASE).join("src");
        build_source(&src);
        stamp(Path::new(BASE));

        fs::create_dir_all(&repo_dir).unwrap();
        let store = LocalFsStore::new(repo_dir.clone());
        let mut init_entropy = entropy(7);
        let pending = PendingInit::generate(
            &mut init_entropy,
            InitOptions {
                kdf: KdfCost {
                    memory_kib: 8192,
                    iterations: 1,
                    parallelism: 1,
                },
                program_version: "lodestore-format golden v1".into(),
                created: Some(pb::Timestamp {
                    seconds: 1_700_000_000,
                    nanos: 0,
                }),
            },
        )
        .unwrap();
        let recovery = pending.recovery_key().encode();
        pending
            .commit(&store, password.as_bytes(), &mut init_entropy)
            .await
            .unwrap();
        create_new(
            &root.join("RECOVERY_KEY"),
            format!("{}\n", *recovery).as_bytes(),
        );

        let repo = Repository::open(&store, password.as_bytes()).await.unwrap();
        let keys = repo.read_keys().write_keys(repo.config_hash());
        create_new(
            &root.join("writer-keys.txt"),
            keys.to_file_text().as_bytes(),
        );
        let mut writer = Writer::new(&store, keys, repo.config())
            .await
            .unwrap()
            .with_entropy(entropy(9));
        let reader_for =
            || Reader::open(&store, repo.read_keys().duplicate(), repo.config()).unwrap();

        let hashes1 = hash_files(Path::new(BASE));
        let first = writer
            .backup(
                std::slice::from_ref(&src),
                options(1_700_000_100, None),
                &|_| {},
            )
            .await
            .unwrap();
        assert!(!first.incomplete);

        let hello = src.join("hello.txt");
        fs::write(&hello, b"hello again, golden v1 second snapshot\n").unwrap();
        utimensat(
            CWD,
            &hello,
            &Timestamps {
                last_access: Timespec {
                    tv_sec: 1_650_000_500,
                    tv_nsec: 5,
                },
                last_modification: Timespec {
                    tv_sec: 1_650_000_000,
                    tv_nsec: 4,
                },
            },
            AtFlags::SYMLINK_NOFOLLOW,
        )
        .unwrap();
        let hashes2 = hash_files(Path::new(BASE));
        assert_ne!(hashes1, hashes2);

        let parent = ParentTree::open(reader_for(), first.snapshot_id)
            .await
            .unwrap();
        let second = writer
            .backup(
                std::slice::from_ref(&src),
                options(1_700_003_700, Some(parent)),
                &|_| {},
            )
            .await
            .unwrap();
        assert!(!second.incomplete);
        assert_ne!(first.snapshot_id, second.snapshot_id);

        let mut reader = reader_for();
        let mut snapshots = Vec::new();
        for (outcome, hashes) in [(&first, &hashes1), (&second, &hashes2)] {
            let entry = reader
                .resolve_snapshot(&hex32(&outcome.snapshot_id))
                .await
                .unwrap();
            if snapshots.is_empty() {
                let mut nodes = Vec::new();
                reader
                    .walk_subtree(&entry.id, b"", &mut |p, n| {
                        nodes.push((p.to_vec(), n.clone()));
                        Ok(())
                    })
                    .await
                    .unwrap();
                let s = "/lodestore-golden/src";
                assert_eq!(at(&nodes, &format!("{s}/suid")).mode & 0o7777, 0o4755);
                let owned = at(&nodes, &format!("{s}/owned"));
                assert_eq!((owned.uid, owned.gid), (1234, 1234));
                let sparse = at(&nodes, &format!("{s}/sparse.bin"));
                assert_eq!(
                    sparse.sparse_extents.len(),
                    2,
                    "{:?}",
                    sparse.sparse_extents
                );
                assert_eq!(
                    at(&nodes, &format!("{s}/fifo")).r#type,
                    pb::NodeType::Fifo as i32
                );
                assert_eq!(
                    at(&nodes, &format!("{s}/link-to-hello")).link_target,
                    b"hello.txt"
                );
                let (a, b) = (
                    at(&nodes, &format!("{s}/links/first")),
                    at(&nodes, &format!("{s}/links/second")),
                );
                assert!(a.hardlink_group != 0 && a.hardlink_group == b.hardlink_group);
                assert_eq!(
                    at(&nodes, &format!("{s}/hello.txt")).xattrs[0].name,
                    b"user.golden"
                );
                assert!(
                    nodes
                        .iter()
                        .any(|(p, _)| p.ends_with(b"/weird/not-utf8-\xff"))
                );
            }
            snapshots
                .push(snapshot_json(&mut reader, &entry.id, &entry.snapshot, Some(hashes)).await);
        }
        let expected = json!({ "snapshots": snapshots });
        create_new(
            &root.join("expected.json"),
            format!("{}\n", serde_json::to_string_pretty(&expected).unwrap()).as_bytes(),
        );
        fs::remove_dir_all(BASE).unwrap();
    }
}
