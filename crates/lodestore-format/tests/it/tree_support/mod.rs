#![allow(dead_code)]

//! A directory made of tree blobs near the size limit, for tests of the
//! tree byte limits in the format and the server. Only the public API and the
//! writer's test hooks are used.

use lodestore_format::crypto::BlobId;
use lodestore_format::pack::MAX_TREE_ULEN;
use lodestore_format::pb;
use lodestore_format::store::BlobStore;
use lodestore_format::tree::{MAX_DIR_TREE_BYTES, encode_tree};
use lodestore_format::writer::Writer;
use prost::Message;

/// The number of full-size pieces that is just over `MAX_DIR_TREE_BYTES`.
pub fn wide_piece_count() -> usize {
    (MAX_DIR_TREE_BYTES / MAX_TREE_ULEN as u64) as usize + 1
}

fn piece_template() -> Vec<u8> {
    let node = |n: usize| pb::Node {
        name: b"f000000".to_vec(),
        r#type: pb::NodeType::File as i32,
        content: vec![vec![7u8; 32]; n],
        ..Default::default()
    };
    let base = node(0).encoded_len();
    // one tag byte and a length varint of at most four bytes
    let n = (MAX_TREE_ULEN - 5 - base) / 34;
    let bytes = encode_tree(vec![node(n)]);
    assert!(bytes.len() <= MAX_TREE_ULEN && bytes.len() > MAX_TREE_ULEN - 64);
    bytes
}

/// Writes `pieces` tree blobs of nearly `MAX_TREE_ULEN` bytes, each with one
/// entry named after its position, and returns a directory node over them.
pub fn write_wide_dir<S: BlobStore>(writer: &mut Writer<S>, name: &str, pieces: usize) -> pb::Node {
    let template = piece_template();
    let at = template
        .windows(7)
        .position(|w| w == b"f000000")
        .expect("name is in the template");
    let mut subtrees = Vec::with_capacity(pieces);
    for i in 0..pieces {
        let mut bytes = template.clone();
        bytes[at..at + 7].copy_from_slice(format!("f{i:06}").as_bytes());
        subtrees.push(writer.put_tree_raw(&bytes).unwrap().to_vec());
    }
    pb::Node {
        name: name.as_bytes().to_vec(),
        r#type: pb::NodeType::Dir as i32,
        mode: 0o755,
        subtrees,
        ..Default::default()
    }
}

/// A snapshot whose only entry is a directory `wide` of `pieces` pieces.
pub async fn wide_snapshot<S: BlobStore>(writer: &mut Writer<S>, pieces: usize) -> BlobId {
    let wide = write_wide_dir(writer, "wide", pieces);
    let root = writer.put_tree_raw(&encode_tree(vec![wide])).unwrap();
    writer
        .commit_snapshot_raw(root, vec![b"/".to_vec()])
        .await
        .unwrap()
}
