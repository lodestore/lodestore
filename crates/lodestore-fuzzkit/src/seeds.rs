//! Seed inputs for the fuzz corpora, cut from the golden fixtures. Fixture
//! files are only read.

use std::fs;
use std::path::Path;

use lodestore_format::crypto::{BLOB_TYPE_TREE, BlobId};
use lodestore_format::index::open_shard;
use lodestore_format::pack::{ENTRY_LEN, PACK_MAGIC, PACK_PREFIX_LEN, read_pack_header};
use lodestore_format::reader::Reader;
use lodestore_format::repo::Repository;
use lodestore_format::store::LocalFsStore;
use lodestore_format::tree::decode_tree;
use prost::Message;

use crate::stores;

type Seeds = Vec<(&'static str, Vec<u8>)>;

const MAX_SEED_LEN: usize = 1 << 20;

const TRAILER_LEN: usize = 4 + PACK_MAGIC.len();

pub(crate) fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            let digits = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(digits, 16).ok()
        })
        .collect()
}

fn hex_fields(text: &str, field: &str) -> Result<Vec<Vec<u8>>, String> {
    let needle = format!("\"{field}\": \"");
    text.match_indices(&needle)
        .map(|(at, _)| {
            let rest = &text[at + needle.len()..];
            let end = rest.find('"').ok_or("unterminated string")?;
            unhex(&rest[..end]).ok_or_else(|| format!("{field} is not hex"))
        })
        .collect()
}

fn vectors(fixtures: &Path, name: &str) -> Result<String, String> {
    fs::read_to_string(fixtures.join(name)).map_err(|e| format!("{name}: {e}"))
}

pub fn seeds(fixtures: &Path) -> Seeds {
    try_seeds(fixtures).unwrap_or_else(|e| panic!("seed inputs: {e}"))
}

pub fn try_seeds(fixtures: &Path) -> Result<Seeds, String> {
    let io = |what: &str, e: std::io::Error| format!("{what}: {e}");
    let password = fs::read_to_string(fixtures.join("PASSWORD")).map_err(|e| io("PASSWORD", e))?;
    let repo_dir = fixtures.join("repo");
    let store = LocalFsStore::new(repo_dir.clone());
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .map_err(|e| io("runtime", e))?;
    let repo = rt
        .block_on(Repository::open(&store, password.trim_end().as_bytes()))
        .map_err(|e| format!("open the fixture repository: {e}"))?;

    let mut out: Seeds = Vec::new();
    for shard in fs::read_dir(repo_dir.join("packs")).map_err(|e| io("packs", e))? {
        let shard = shard.map_err(|e| io("packs", e))?;
        for pack in fs::read_dir(shard.path()).map_err(|e| io("packs", e))? {
            let path = pack.map_err(|e| io("packs", e))?.path();
            let bytes = fs::read(&path).map_err(|e| io("pack", e))?;
            let size = bytes.len();
            let prefix = bytes
                .get(..PACK_PREFIX_LEN)
                .ok_or("pack shorter than a prefix")?;
            let trailer = bytes
                .get(size.saturating_sub(TRAILER_LEN)..)
                .ok_or("pack shorter than a trailer")?;
            out.push(("pack_prefix", prefix.to_vec()));
            out.push((
                "pack_trailer",
                [&(size as u64).to_le_bytes()[..], trailer].concat(),
            ));

            let key = format!(
                "packs/{}/{}",
                shard.file_name().to_string_lossy(),
                path.file_name().unwrap_or_default().to_string_lossy()
            );
            let header = rt
                .block_on(read_pack_header(
                    &store,
                    &key,
                    size as u64,
                    repo.read_keys(),
                ))
                .map_err(|e| format!("read the header of {key}: {e}"))?;
            let mut records = Vec::with_capacity(header.entries.len() * ENTRY_LEN);
            for e in &header.entries {
                records.extend_from_slice(&e.blob_id);
                records.push(e.blob_type);
                records.extend_from_slice(&e.offset.to_le_bytes());
                records.extend_from_slice(&e.enc_len.to_le_bytes());
                records.extend_from_slice(&e.ulen.to_le_bytes());
            }
            out.push(("pack_entries", records));
        }
    }

    for entry in fs::read_dir(repo_dir.join("index")).map_err(|e| io("index", e))? {
        let bytes =
            fs::read(entry.map_err(|e| io("index", e))?.path()).map_err(|e| io("shard", e))?;
        let shard_id = *blake3::hash(&bytes).as_bytes();
        let shard = open_shard(
            repo.read_keys().k_idx(),
            repo.read_keys().repo_id(),
            &shard_id,
            &bytes,
        )
        .map_err(|e| format!("open a fixture shard: {e}"))?;
        let plain = shard.encode_to_vec();
        out.push(("verify_shard", [&[1u8][..], &plain].concat()));
        out.push(("snapshot_resolves", [&[1u8, 0][..], &plain].concat()));
        out.push(("snapshot_resolves", [&[2u8, 0][..], &plain].concat()));
        out.push(("index_shard", plain));
    }
    for (at, key) in stores::shard_keys().iter().enumerate() {
        let bytes = fs::read(repo_dir.join(key)).map_err(|e| io("shard", e))?;
        out.push(("verify_shard", [&[2u8, at as u8][..], &bytes].concat()));
    }
    for (at, key) in stores::pack_keys().iter().enumerate() {
        let bytes = fs::read(repo_dir.join(key)).map_err(|e| io("pack", e))?;
        if bytes.len() > MAX_SEED_LEN {
            continue;
        }
        for target in ["verify_shard", "snapshot_resolves"] {
            out.push((target, [&[0u8, at as u8][..], &bytes].concat()));
        }
    }
    for plain in hex_fields(&vectors(fixtures, "index-vectors.json")?, "plaintext")? {
        out.push(("index_shard", plain));
    }

    let gc = vectors(fixtures, "gc-vectors.json")?;
    let (manifest, marker) = gc
        .split_once("\"writer_marker\"")
        .ok_or("gc-vectors.json has no writer_marker")?;
    for plain in hex_fields(manifest, "plaintext")? {
        out.push(("gc_manifest", [&[0u8][..], &[0x11; 16], &plain].concat()));
    }
    for plain in hex_fields(marker, "plaintext")? {
        out.push(("writer_marker", [&[0u8][..], &[0x22; 16], &plain].concat()));
    }

    let attest = vectors(fixtures, "attest-vectors.json")?;
    for body in hex_fields(&attest, "plaintext")? {
        out.push(("attest_open", [&[0x44u8; 16][..], &body].concat()));
    }
    for frame in hex_fields(&attest, "frame")? {
        out.push(("attest_peek", [&[0x44u8; 16][..], &frame].concat()));
    }
    for record in hex_fields(&attest, "record_plaintext")? {
        out.push(("provenance_open", record));
    }

    let mut reader = Reader::open(&store, repo.read_keys().duplicate(), repo.config())
        .map_err(|e| format!("open a reader: {e}"))?;
    let mut pending = Vec::new();
    for entry in fs::read_dir(repo_dir.join("snapshots")).map_err(|e| io("snapshots", e))? {
        let name = entry.map_err(|e| io("snapshots", e))?.file_name();
        let id: BlobId = unhex(&name.to_string_lossy())
            .and_then(|b| b.try_into().ok())
            .ok_or("snapshot name is not a blob id")?;
        let snapshot = rt
            .block_on(reader.snapshot(&id))
            .map_err(|e| format!("read a fixture snapshot: {e}"))?;
        pending.extend(BlobId::try_from(snapshot.root_tree.as_slice()).ok());
        out.push((
            "snapshot_resolves",
            [&[3u8][..], &snapshot.encode_to_vec()].concat(),
        ));
        let raw = fs::read(repo_dir.join(format!("snapshots/{}", name.to_string_lossy())))
            .map_err(|e| io("snapshot", e))?;
        out.push(("snapshot_resolves", [&[4u8][..], &raw].concat()));
    }
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        let bytes = rt
            .block_on(reader.read_blob(&id, BLOB_TYPE_TREE))
            .map_err(|e| format!("read a fixture tree: {e}"))?;
        let nodes = decode_tree(&bytes, None).map_err(|e| format!("decode a fixture tree: {e}"))?;
        for node in &nodes {
            pending.extend(
                node.subtrees
                    .iter()
                    .filter_map(|s| BlobId::try_from(s.as_slice()).ok()),
            );
        }
        for target in ["tree_decode", "tree_count", "tree_cursor"] {
            out.push((target, bytes.clone()));
        }
    }
    Ok(out)
}
