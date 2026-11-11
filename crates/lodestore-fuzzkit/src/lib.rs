#![cfg(feature = "test-hooks")]
#![forbid(unsafe_code)]
//! Every function here is the body of one fuzz target. None of them panics on
//! any input; equality assertions exist only where they are written down.

pub mod restore_plan;
pub mod seeds;
pub mod stores;

use std::collections::HashSet;
use std::sync::OnceLock;

use lodestore_format::attest::{
    ATTEST_FRAME_V1, MAX_ATTEST_SIZE, attest_key, open_attestation, open_provenance,
    peek_attestation,
};
use lodestore_format::crypto::{
    self, Entropy, RepoId, attest_mac, open_gc_manifest_body, open_index_shard,
    open_provenance_body, open_writer_marker_body, seal_gc_manifest_body, seal_index_shard,
    seal_provenance_body, seal_writer_marker_body,
};
use lodestore_format::error::Result;
use lodestore_format::index::{Index, SupersedesTrust, open_shard};
use lodestore_format::keys::{MasterKey, ReadKeys};
use lodestore_format::locks::{
    LockId, MAX_GC_MANIFEST_SIZE, MAX_WRITER_MARKER_SIZE, gc_manifest_key, open_gc_manifest,
    open_writer_marker, writer_marker_key,
};
use lodestore_format::pack::{PACK_PREFIX_LEN, parse_entries, parse_prefix, parse_trailer};
use lodestore_format::pb;
use lodestore_format::tree::{decode_tree, hooks};
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::SeedableRng;

pub fn pack_prefix(data: &[u8]) {
    let _ = parse_prefix(data);
}

pub fn pack_trailer(data: &[u8]) {
    let Some((size, rest)) = data.split_first_chunk::<8>() else {
        return;
    };
    let _ = parse_trailer(rest, u64::from_le_bytes(*size));
}

pub fn pack_entries(data: &[u8]) {
    let _ = parse_entries(data);
}

pub fn tree_decode(data: &[u8]) {
    let _ = decode_tree(data, None);
    if let Some((&first, rest)) = data.split_first()
        && first < 128
        && !rest.is_empty()
    {
        let _ = decode_tree(rest, Some(&data[1..data.len().min(33)]));
    }
}

pub fn tree_count(data: &[u8]) {
    let counted = hooks::count_nodes(data);
    if let Ok(nodes) = decode_tree(data, None) {
        assert_eq!(
            counted.ok(),
            Some(nodes.len() as u64),
            "a tree that decodes must count, and to the same number"
        );
    }
}

pub fn tree_cursor(data: &[u8]) {
    if let Ok(nodes) = decode_tree(data, None) {
        let len = nodes.len();
        let paths = hooks::cursor_paths(nodes, data.len() as u64, b"/base".to_vec());
        assert_eq!(paths.len(), len, "the cursor must hand out every node once");
    }
}

pub fn attest_peek(data: &[u8]) {
    if let Some((id, rest)) = data.split_first_chunk::<16>() {
        let _ = peek_attestation(&attest_key(id), rest);
    }
    if let Some(end) = data.iter().position(|&b| b == b'\n')
        && let Ok(key) = std::str::from_utf8(&data[..end])
    {
        let _ = peek_attestation(key, &data[end + 1..]);
    }
}

fn keys() -> &'static ReadKeys {
    static KEYS: OnceLock<ReadKeys> = OnceLock::new();
    KEYS.get_or_init(|| ReadKeys::derive(MasterKey::from_bytes([1u8; 32]), [2u8; 16]))
}

pub(crate) fn entropy() -> Entropy {
    Entropy::from_rng(ChaCha20Rng::from_seed([7u8; 32]))
}

fn fixed_shard() -> pb::IndexShard {
    let mut entry = vec![4u8; 32];
    entry.push(crypto::BLOB_TYPE_DATA);
    entry.extend_from_slice(&(PACK_PREFIX_LEN as u32).to_le_bytes());
    entry.extend_from_slice(&100u32.to_le_bytes());
    entry.extend_from_slice(&10u32.to_le_bytes());
    pb::IndexShard {
        packs: vec![pb::PackRecord {
            pack_id: vec![3u8; 32],
            hpke_enc: vec![0u8; crypto::HPKE_ENC_LEN],
            sealed_pack_key: vec![0u8; crypto::SEALED_KEY_LEN],
            pack_size: 4096,
            entries: entry,
        }],
        supersedes: Vec::new(),
    }
}

pub fn index_shard(data: &[u8]) {
    let keys = keys();
    let (k_idx, repo_id) = (keys.k_idx(), keys.repo_id());
    let mut indexes = [Index::default(), Index::default()];
    indexes[1].set_supersedes_trust(SupersedesTrust::Only(HashSet::new()));
    let fixed = fixed_shard();
    for index in &mut indexes {
        let _ = index.add_shard([9u8; 32], &fixed);
    }

    if let Ok(sealed) = seal_index_shard(k_idx, repo_id, data, &mut entropy()) {
        assert_eq!(
            open_index_shard(k_idx, repo_id, &sealed).ok().as_deref(),
            Some(data),
            "an index shard must open to what was sealed"
        );
        let shard_id = *blake3::hash(&sealed).as_bytes();
        if let Ok(shard) = open_shard(k_idx, repo_id, &shard_id, &sealed) {
            for index in &mut indexes {
                let _ = index.add_shard(shard_id, &shard);
            }
        }
    }
    let _ = open_shard(k_idx, repo_id, blake3::hash(data).as_bytes(), data);
}

type Seal = fn(&[u8; 32], &RepoId, &[u8], &mut Entropy) -> Result<Vec<u8>>;
type OpenBody = fn(&[u8; 32], &RepoId, &[u8]) -> Result<Vec<u8>>;

fn lock_object(
    data: &[u8],
    max: u64,
    key_of: fn(&LockId) -> String,
    seal: Seal,
    open_body: OpenBody,
    open: impl Fn(&[u8; 32], &RepoId, &str, &[u8]),
) {
    if data.len() as u64 > max {
        return;
    }
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    let keys = keys();
    let (k_idx, repo_id) = (keys.k_idx(), keys.repo_id());
    let sealed_open = |key: &str, plain: &[u8]| {
        if let Ok(sealed) = seal(k_idx, repo_id, plain, &mut entropy()) {
            assert_eq!(
                open_body(k_idx, repo_id, &sealed).ok().as_deref(),
                Some(plain),
                "a lock body must open to what was sealed"
            );
            open(k_idx, repo_id, key, &sealed);
        }
    };
    match mode {
        0 => {
            if let Some((id, plain)) = rest.split_first_chunk::<16>() {
                sealed_open(&key_of(id), plain);
            }
        }
        1 => {
            if let Some(end) = rest.iter().position(|&b| b == b'\n')
                && let Ok(key) = std::str::from_utf8(&rest[..end])
            {
                sealed_open(key, &rest[end + 1..]);
            }
        }
        _ => {
            if let Some((id, bytes)) = rest.split_first_chunk::<16>() {
                open(k_idx, repo_id, &key_of(id), bytes);
            }
        }
    }
}

pub fn gc_manifest(data: &[u8]) {
    lock_object(
        data,
        MAX_GC_MANIFEST_SIZE,
        gc_manifest_key,
        seal_gc_manifest_body,
        open_gc_manifest_body,
        |k, r, key, bytes| {
            let _ = open_gc_manifest(k, r, key, bytes);
        },
    );
}

pub fn writer_marker(data: &[u8]) {
    lock_object(
        data,
        MAX_WRITER_MARKER_SIZE,
        writer_marker_key,
        seal_writer_marker_body,
        open_writer_marker_body,
        |k, r, key, bytes| {
            let _ = open_writer_marker(k, r, key, bytes);
        },
    );
}

pub fn attest_open(data: &[u8]) {
    if data.len() as u64 > MAX_ATTEST_SIZE {
        return;
    }
    let Some((id, body)) = data.split_first_chunk::<16>() else {
        return;
    };
    let keys = keys();
    let (k_srv, repo_id) = (keys.k_srv(), keys.repo_id());
    let key = attest_key(id);

    let mut framed = vec![ATTEST_FRAME_V1];
    framed.extend_from_slice(&attest_mac(k_srv, repo_id, ATTEST_FRAME_V1, body));
    framed.extend_from_slice(body);
    if let Ok(opened) = open_attestation(k_srv, repo_id, &key, &framed) {
        assert_eq!(
            peek_attestation(&key, &framed).ok(),
            Some(opened),
            "peek and open must agree on an authentic attestation"
        );
    }
    let _ = open_attestation(k_srv, repo_id, &key, body);
}

pub fn provenance_open(data: &[u8]) {
    let keys = keys();
    if let Ok(sealed) = seal_provenance_body(keys.k_srv(), keys.repo_id(), data, &mut entropy()) {
        assert_eq!(
            open_provenance_body(keys.k_srv(), keys.repo_id(), &sealed)
                .ok()
                .as_deref(),
            Some(data),
            "a provenance body must open to what was sealed"
        );
        let _ = open_provenance(keys, &sealed);
    }
    let _ = open_provenance(keys, data);
}

pub use restore_plan::restore_plan;
pub use stores::{snapshot_resolves, verify_shard};

pub type Target = fn(&[u8]);

pub const TARGETS: &[(&str, Target)] = &[
    ("pack_prefix", pack_prefix),
    ("pack_trailer", pack_trailer),
    ("pack_entries", pack_entries),
    ("tree_decode", tree_decode),
    ("tree_count", tree_count),
    ("tree_cursor", tree_cursor),
    ("attest_peek", attest_peek),
    ("index_shard", index_shard),
    ("gc_manifest", gc_manifest),
    ("writer_marker", writer_marker),
    ("attest_open", attest_open),
    ("provenance_open", provenance_open),
    ("verify_shard", verify_shard),
    ("snapshot_resolves", snapshot_resolves),
    ("restore_plan", restore_plan),
];
