//! Encode then decode is the identity for every format codec, on generated
//! inputs inside the bounds each decoder accepts.

use std::collections::BTreeMap;

use lodestore_format::attest::{
    attest_key, open_attestation, open_provenance, peek_attestation, seal_attestation,
    seal_provenance,
};
use lodestore_format::crypto::{
    BLOB_TYPE_DATA, BLOB_TYPE_TREE, BlobId, Entropy, open_frame, open_pack_header,
};
use lodestore_format::index::{open_shard, seal_shard_superseding};
use lodestore_format::keys::{
    KdfCost, MasterKey, ReadKeys, RecoveryKey, open_password_slot, open_recovery_slot,
    seal_password_slot, seal_recovery_slot,
};
use lodestore_format::locks::{
    gc_manifest_key, open_gc_manifest, open_writer_marker, seal_gc_manifest, seal_writer_marker,
    writer_marker_key,
};
use lodestore_format::pack::{
    ENTRY_LEN, PACK_PREFIX_LEN, PackBuilder, decompress, parse_entries, parse_prefix, parse_trailer,
};
use lodestore_format::pb;
use lodestore_format::snapshot::{open_snapshot, seal_snapshot};
use lodestore_format::tree::{decode_tree, encode_tree, split_nodes};
use proptest::prelude::*;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::SeedableRng;

const FAST: u32 = 256;
const SLOW: u32 = 16;
const TRAILER_LEN: usize = 10;
const CHEAP_KDF: KdfCost = KdfCost {
    memory_kib: 8,
    iterations: 1,
    parallelism: 1,
};

fn keys() -> ReadKeys {
    ReadKeys::derive(MasterKey::from_bytes([1; 32]), [2; 16])
}

fn entropy(seed: [u8; 32]) -> Entropy {
    Entropy::from_rng(ChaCha20Rng::from_seed(seed))
}

fn bytes(max: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..=max)
}

fn exactly(len: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), len)
}

fn text() -> impl Strategy<Value = String> {
    "\\PC{0,24}"
}

fn timestamp() -> impl Strategy<Value = pb::Timestamp> {
    (any::<i64>(), any::<u32>()).prop_map(|(seconds, nanos)| pb::Timestamp { seconds, nanos })
}

fn issue() -> impl Strategy<Value = pb::SnapshotIssue> {
    (bytes(40), 0..6i32, text()).prop_map(|(path, kind, message)| pb::SnapshotIssue {
        path,
        kind,
        message,
    })
}

fn stats() -> impl Strategy<Value = pb::SnapshotStats> {
    prop::array::uniform7(any::<u64>()).prop_map(|v| pb::SnapshotStats {
        files_new: v[0],
        files_changed: v[1],
        files_unchanged: v[2],
        dirs: v[3],
        bytes_total: v[4],
        bytes_read: v[5],
        bytes_added: v[6],
    })
}

fn snapshot() -> impl Strategy<Value = pb::Snapshot> {
    let head = (
        prop::option::of(timestamp()),
        bytes(40),
        prop::collection::vec(bytes(40), 0..4),
        prop::collection::vec(text(), 0..4),
        exactly(32),
        prop_oneof![Just(Vec::new()), exactly(32)],
        prop::option::of(stats()),
        any::<bool>(),
    );
    let tail = (
        prop::collection::vec(issue(), 0..3),
        prop::collection::vec(issue(), 0..3),
        any::<u64>(),
        any::<u64>(),
        prop::collection::vec(exactly(32), 0..4),
        bytes(16),
        bytes(64),
        any::<u32>(),
        text(),
    );
    (head, tail).prop_map(|(h, t)| pb::Snapshot {
        time: h.0,
        hostname: h.1,
        paths: h.2,
        tags: h.3,
        root_tree: h.4,
        parent: h.5,
        stats: h.6,
        incomplete: h.7,
        errors: t.0,
        warnings: t.1,
        errors_total: t.2,
        warnings_total: t.3,
        index_refs: t.4,
        agent_id: t.5,
        signature: t.6,
        key_epoch: t.7,
        program_version: t.8,
    })
}

fn name() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(
        any::<u8>().prop_filter("no separator or NUL", |b| *b != b'/' && *b != 0),
        1..=24,
    )
    .prop_filter("not a dot name", |n| n != b"." && n != b"..")
}

#[derive(Debug, Clone)]
struct NodeBody {
    kind: u8,
    mode: u32,
    owner: (u32, u32, Vec<u8>, Vec<u8>),
    times: (
        Option<pb::Timestamp>,
        Option<pb::Timestamp>,
        Option<pb::Timestamp>,
    ),
    size: u64,
    content: Vec<Vec<u8>>,
    extents: Vec<(u64, u64)>,
    link_target: Vec<u8>,
    rdev: u64,
    xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    ids: (u64, u64, u64, u64),
    subtrees: Vec<Vec<u8>>,
}

fn node_body() -> impl Strategy<Value = NodeBody> {
    (
        0..6u8,
        any::<u32>(),
        (any::<u32>(), any::<u32>(), bytes(8), bytes(8)),
        (
            prop::option::of(timestamp()),
            prop::option::of(timestamp()),
            prop::option::of(timestamp()),
        ),
        any::<u64>(),
        prop::collection::vec(exactly(32), 0..4),
        prop::collection::vec((any::<u64>(), any::<u64>()), 0..3),
        prop::collection::vec(any::<u8>(), 1..24),
        any::<u64>(),
        prop::collection::vec((bytes(12), bytes(24)), 0..3),
        (any::<u64>(), any::<u64>(), any::<u64>(), any::<u64>()),
        prop::collection::vec(exactly(32), 0..3),
    )
        .prop_map(
            |(
                kind,
                mode,
                owner,
                times,
                size,
                content,
                extents,
                link_target,
                rdev,
                xattrs,
                ids,
                subtrees,
            )| NodeBody {
                kind,
                mode,
                owner,
                times,
                size,
                content,
                extents,
                link_target,
                rdev,
                xattrs,
                ids,
                subtrees,
            },
        )
}

fn make_node(name: Vec<u8>, b: NodeBody) -> pb::Node {
    use pb::NodeType::*;
    let ty = [File, Dir, Symlink, Fifo, Chardev, Blockdev][b.kind as usize];
    let mut node = pb::Node {
        name,
        r#type: ty as i32,
        mode: b.mode,
        uid: b.owner.0,
        gid: b.owner.1,
        user: b.owner.2,
        group: b.owner.3,
        mtime: b.times.0,
        atime: b.times.1,
        ctime: b.times.2,
        size: b.size,
        rdev: b.rdev,
        xattrs: b
            .xattrs
            .into_iter()
            .map(|(name, value)| pb::Xattr { name, value })
            .collect(),
        inode: b.ids.0,
        dev: b.ids.1,
        nlink: b.ids.2,
        ..Default::default()
    };
    match ty {
        File => {
            node.content = b.content;
            node.sparse_extents = b
                .extents
                .into_iter()
                .map(|(offset, length)| pb::Extent { offset, length })
                .collect();
            node.hardlink_group = b.ids.3;
        }
        Dir => node.subtrees = b.subtrees,
        Symlink => node.link_target = b.link_target,
        _ => {}
    }
    node
}

fn tree() -> impl Strategy<Value = Vec<pb::Node>> {
    prop::collection::btree_set(name(), 0..12).prop_flat_map(|names| {
        let n = names.len();
        (Just(names), prop::collection::vec(node_body(), n..=n)).prop_map(|(names, bodies)| {
            names
                .into_iter()
                .zip(bodies)
                .map(|(name, body)| make_node(name, body))
                .collect()
        })
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(FAST))]

    #[test]
    fn codec_snapshot_round_trip(snap in snapshot(), seed in any::<[u8; 32]>()) {
        let keys = keys();
        let (id, sealed) =
            seal_snapshot(&snap, keys.hpke_public_key(), keys.repo_id(), &mut entropy(seed))
                .unwrap();
        prop_assert_eq!(id, *blake3::hash(&sealed).as_bytes());
        let opened = open_snapshot(keys.hpke_private_key(), keys.repo_id(), &id, &sealed).unwrap();
        prop_assert_eq!(opened, snap);
    }

    #[test]
    fn codec_tree_round_trip(nodes in tree()) {
        let encoded = encode_tree(nodes.clone());
        prop_assert_eq!(&decode_tree(&encoded, None).unwrap(), &nodes);

        let pieces = split_nodes(nodes.clone());
        let mut previous: Option<Vec<u8>> = None;
        let mut rejoined = Vec::new();
        for piece in pieces {
            let decoded = decode_tree(&encode_tree(piece.clone()), previous.as_deref()).unwrap();
            prop_assert_eq!(&decoded, &piece);
            previous = decoded.last().map(|n| n.name.clone());
            rejoined.extend(decoded);
        }
        prop_assert_eq!(rejoined, nodes);
    }

    #[test]
    fn codec_index_shard_round_trip(
        records in prop::collection::vec(
            (exactly(32), exactly(1120), exactly(48), any::<u64>(), prop::collection::vec(entry(), 0..5)),
            0..4,
        ),
        supersedes in prop::collection::vec(exactly(32), 0..5),
        seed in any::<[u8; 32]>(),
    ) {
        let keys = keys();
        let packs: Vec<pb::PackRecord> = records
            .into_iter()
            .map(|(pack_id, hpke_enc, sealed_pack_key, pack_size, entries)| pb::PackRecord {
                pack_id,
                hpke_enc,
                sealed_pack_key,
                pack_size,
                entries: entries.concat(),
            })
            .collect();
        let ids: Vec<BlobId> = supersedes
            .iter()
            .map(|s| BlobId::try_from(s.as_slice()).unwrap())
            .collect();
        let mut sorted = supersedes;
        sorted.sort();
        sorted.dedup();
        let (id, sealed) = seal_shard_superseding(
            keys.k_idx(),
            keys.repo_id(),
            packs.clone(),
            ids,
            &mut entropy(seed),
        )
        .unwrap();
        let opened = open_shard(keys.k_idx(), keys.repo_id(), &id, &sealed).unwrap();
        prop_assert_eq!(opened, pb::IndexShard { packs, supersedes: sorted });
    }

    #[test]
    fn codec_lock_and_marker_round_trip(
        gc_id in exactly(16),
        candidates in prop::collection::btree_set(exactly(32), 0..6),
        published in prop::option::of(timestamp()),
        writer_id in exactly(16),
        hostname in bytes(40),
        started in prop::option::of(timestamp()),
        seed in any::<[u8; 32]>(),
    ) {
        let keys = keys();
        let mut entropy = entropy(seed);
        let manifest = pb::GcManifest {
            version: 1,
            gc_id: gc_id.clone(),
            published,
            candidate_packs: candidates.into_iter().collect(),
        };
        let key = gc_manifest_key(&gc_id.as_slice().try_into().unwrap());
        let sealed = seal_gc_manifest(keys.k_idx(), keys.repo_id(), &manifest, &mut entropy).unwrap();
        prop_assert_eq!(
            open_gc_manifest(keys.k_idx(), keys.repo_id(), &key, &sealed).unwrap(),
            manifest
        );

        let marker = pb::WriterMarker { version: 1, writer_id: writer_id.clone(), hostname, started };
        let key = writer_marker_key(&writer_id.as_slice().try_into().unwrap());
        let sealed = seal_writer_marker(keys.k_idx(), keys.repo_id(), &marker, &mut entropy).unwrap();
        prop_assert_eq!(
            open_writer_marker(keys.k_idx(), keys.repo_id(), &key, &sealed).unwrap(),
            marker
        );
    }

    #[test]
    fn codec_attestation_round_trip(
        attest_id in exactly(16),
        created in prop::option::of(timestamp()),
        shard_ids in prop::collection::vec(exactly(32), 0..16),
        markers in prop::collection::vec(any::<u32>(), 0..5),
        sealed_provenance in prop::collection::vec(bytes(256), 0..4),
    ) {
        let keys = keys();
        let attestation = pb::Attestation {
            attest_id: attest_id.clone(),
            created,
            shard_ids,
            markers,
            sealed_provenance,
        };
        let key = attest_key(&attest_id.as_slice().try_into().unwrap());
        let sealed = seal_attestation(keys.k_srv(), keys.repo_id(), &attestation);
        prop_assert_eq!(
            &open_attestation(keys.k_srv(), keys.repo_id(), &key, &sealed).unwrap(),
            &attestation
        );
        prop_assert_eq!(peek_attestation(&key, &sealed).unwrap(), attestation);
    }

    #[test]
    fn codec_provenance_round_trip(
        snapshot_id in exactly(32),
        host_id in text(),
        hostname in text(),
        job_id in text(),
        verified_at in prop::option::of(timestamp()),
        seed in any::<[u8; 32]>(),
    ) {
        let keys = keys();
        let record = pb::ProvenanceRecord { snapshot_id, host_id, hostname, job_id, verified_at };
        let sealed = seal_provenance(&keys, &record, &mut entropy(seed)).unwrap();
        prop_assert_eq!(open_provenance(&keys, &sealed).unwrap(), record);
    }

    #[test]
    fn codec_pack_round_trip(
        tree_pack in any::<bool>(),
        blobs in prop::collection::btree_map(
            exactly(32),
            prop_oneof![bytes(300), (any::<u8>(), 0..3000usize).prop_map(|(b, n)| vec![b; n])],
            1..6,
        ),
        seed in any::<[u8; 32]>(),
    ) {
        let keys = keys();
        let mut entropy = entropy(seed);
        let blob_type = if tree_pack { BLOB_TYPE_TREE } else { BLOB_TYPE_DATA };
        let mut builder =
            PackBuilder::new(blob_type, keys.repo_id(), keys.hpke_public_key(), &mut entropy).unwrap();
        for (id, data) in &blobs {
            let id: BlobId = id.as_slice().try_into().unwrap();
            builder.add(&id, data, &mut entropy).unwrap();
        }
        let pack = builder.finish(&mut entropy).unwrap();
        let all = &pack.bytes;
        prop_assert_eq!(all.len() as u64, pack.record.pack_size);
        prop_assert_eq!(pack.pack_id, *blake3::hash(all).as_bytes());

        let prefix = parse_prefix(&all[..PACK_PREFIX_LEN]).unwrap();
        prop_assert_eq!(&prefix.hpke_enc, &pack.record.hpke_enc);
        prop_assert_eq!(&prefix.sealed_pack_key, &pack.record.sealed_pack_key);

        let header_len = parse_trailer(&all[all.len() - TRAILER_LEN..], all.len() as u64).unwrap();
        let frames_end = all.len() - TRAILER_LEN - header_len;
        let pack_key = prefix.open_key(keys.hpke_private_key(), keys.repo_id()).unwrap();
        let plain =
            open_pack_header(&pack_key, keys.repo_id(), &all[frames_end..all.len() - TRAILER_LEN])
                .unwrap();
        prop_assert_eq!(&plain, &pack.record.entries);
        prop_assert_eq!(plain.len(), blobs.len() * ENTRY_LEN);

        let entries = parse_entries(&plain).unwrap();
        prop_assert!(entries.windows(2).all(|w| w[0].blob_id < w[1].blob_id));
        let mut seen = BTreeMap::new();
        for e in &entries {
            let frame = &all[e.offset as usize..(e.offset + e.enc_len) as usize];
            prop_assert!(e.offset as usize + e.enc_len as usize <= frames_end);
            let (codec, payload) =
                open_frame(&pack_key, keys.repo_id(), e.blob_type, &e.blob_id, frame).unwrap();
            let data = decompress(codec, payload, e.ulen as usize, e.blob_type).unwrap();
            seen.insert(e.blob_id.to_vec(), data);
        }
        prop_assert_eq!(seen, blobs);
    }
}

fn entry() -> impl Strategy<Value = Vec<u8>> {
    (
        exactly(32),
        prop_oneof![Just(BLOB_TYPE_DATA), Just(BLOB_TYPE_TREE)],
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
    )
        .prop_map(|(id, ty, offset, enc_len, ulen)| {
            let mut e = id;
            e.push(ty);
            e.extend_from_slice(&offset.to_le_bytes());
            e.extend_from_slice(&enc_len.to_le_bytes());
            e.extend_from_slice(&ulen.to_le_bytes());
            e
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(SLOW))]

    #[test]
    fn codec_key_slot_round_trip(
        master in any::<[u8; 32]>(),
        password in bytes(32),
        other in bytes(32),
        created in prop::option::of(timestamp()),
        seed in any::<[u8; 32]>(),
    ) {
        prop_assume!(password != other);
        let repo_id = [2u8; 16];
        let master = MasterKey::from_bytes(master);
        let mut entropy = entropy(seed);

        let slot =
            seal_password_slot(&master, &repo_id, &password, CHEAP_KDF, created, &mut entropy).unwrap();
        let opened = open_password_slot(&slot, &repo_id, &password).unwrap().unwrap();
        prop_assert_eq!(opened.as_bytes(), master.as_bytes());
        prop_assert!(open_password_slot(&slot, &repo_id, &other).unwrap().is_none());

        let recovery = RecoveryKey::generate(&mut entropy);
        let wrong = RecoveryKey::generate(&mut entropy);
        let slot = seal_recovery_slot(&master, &repo_id, &recovery, created, &mut entropy).unwrap();
        let opened = open_recovery_slot(&slot, &repo_id, &recovery).unwrap().unwrap();
        prop_assert_eq!(opened.as_bytes(), master.as_bytes());
        prop_assert!(open_recovery_slot(&slot, &repo_id, &wrong).unwrap().is_none());
        let typed = RecoveryKey::parse(&recovery.encode()).unwrap();
        let opened = open_recovery_slot(&slot, &repo_id, &typed).unwrap().unwrap();
        prop_assert_eq!(opened.as_bytes(), master.as_bytes());
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(SLOW))]

    #[test]
    fn codec_tree_split_round_trip(count in 1100..1300usize, salt in any::<u8>()) {
        let nodes: Vec<pb::Node> = (0..count)
            .map(|i| pb::Node {
                name: format!("n{i:06}").into_bytes(),
                r#type: pb::NodeType::File as i32,
                xattrs: vec![pb::Xattr { name: b"user.pad".to_vec(), value: vec![salt; 1000] }],
                ..Default::default()
            })
            .collect();
        let pieces = split_nodes(nodes.clone());
        prop_assert!(pieces.len() > 1);
        let mut previous: Option<Vec<u8>> = None;
        let mut rejoined = Vec::new();
        for piece in pieces {
            let decoded = decode_tree(&encode_tree(piece.clone()), previous.as_deref()).unwrap();
            prop_assert!(decoded == piece);
            previous = decoded.last().map(|n| n.name.clone());
            rejoined.extend(decoded);
        }
        prop_assert!(rejoined == nodes);
    }
}
