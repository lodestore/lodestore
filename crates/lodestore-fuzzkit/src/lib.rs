#![cfg(feature = "test-hooks")]
#![forbid(unsafe_code)]
//! Every function here is the body of one fuzz target. None of them panics on
//! any input; equality assertions exist only where they are written down.

pub mod restore_plan;
pub mod seeds;
pub mod stores;

use std::collections::HashSet;
use std::pin::Pin;
use std::sync::OnceLock;
use std::task::{Context, Poll};

use lodestore_agent::update::minisign::{PublicKey, Signature, TrustedRelease};
use lodestore_format::attest::{
    ATTEST_FRAME_V1, MAX_ATTEST_SIZE, attest_key, open_attestation, open_provenance,
    peek_attestation,
};
use lodestore_format::crypto::{
    self, Entropy, NONCE_LEN, RepoId, attest_mac, open_gc_manifest_body, open_index_shard,
    open_provenance_body, open_writer_marker_body, seal_gc_manifest_body, seal_index_shard,
    seal_provenance_body, seal_snapshot_body, seal_snapshot_key, seal_writer_marker_body,
};
use lodestore_format::ct::{self, CtMeta, IdMap};
use lodestore_format::error::Result;
use lodestore_format::fs::idmap::{Direction, translate_acl, translate_caps};
use lodestore_format::fs::restore::check_extents;
use lodestore_format::image::{
    BitmapBinding, BitmapNames, FullReason, IMAGE_CHUNK_SIZE, ImageExtent, ImageMeta, ImageSource,
    ImageState, SEGMENT_CHUNKS, merge_head, merge_segments, parse_layout, segment_nodes,
};
use lodestore_format::index::{Index, SupersedesTrust, open_shard};
use lodestore_format::keys::{
    MAX_SLOT_SIZE, MasterKey, ReadKeys, RecoveryKey, WriteKeys, looks_like_recovery_key,
    open_password_slot, open_recovery_slot, slot_kdf_work,
};
use lodestore_format::locks::{
    LockId, MAX_GC_MANIFEST_SIZE, MAX_WRITER_MARKER_SIZE, gc_manifest_key, open_gc_manifest,
    open_writer_marker, writer_marker_key,
};
use lodestore_format::pack::{PACK_PREFIX_LEN, parse_entries, parse_prefix, parse_trailer};
use lodestore_format::pb;
use lodestore_format::snapshot::{ENVELOPE_VERSION, open_snapshot};
use lodestore_format::source::{self, SourceKey};
use lodestore_format::store::sftp::{RawEntry, RawKind, check_listing};
use lodestore_format::tree::{decode_tree, hooks};
use lodestore_nbd::{Client, Limits as NbdLimits, MemExport, NbdSource, serve};
use prost::Message;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::SeedableRng;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

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

pub fn source_decode(data: &[u8]) {
    if let Ok(claimed) = pb::Source::decode(data)
        && let Ok((key, meta)) = source::decode(Some(&claimed))
    {
        let again = source::encode(&key, &meta).expect("a decoded source encodes");
        assert_eq!(
            source::decode(again.as_ref()).ok(),
            Some((key, meta)),
            "an encoded source must decode to what was encoded"
        );
    }
    if let Ok(key) = SourceKey::parse(&String::from_utf8_lossy(data)) {
        assert_eq!(
            SourceKey::parse(key.as_str()).ok(),
            Some(key),
            "a parsed key must parse again to itself"
        );
    }
}

/// A hostile container manifest, top tree, `/config` listing, id map,
/// `pct.conf` or ACL/capability xattr is refused or checks out; a parsed
/// map is canonical, a parsed meta encodes back to itself, and a translated
/// xattr translates again.
pub fn ct_layout(data: &[u8]) {
    if let Ok(snapshot) = pb::Snapshot::decode(data) {
        let _ = ct::check_manifest(&snapshot);
    }
    if let Ok(claimed) = pb::Source::decode(data)
        && let Ok((_, meta)) = source::decode(Some(&claimed))
        && let Ok(parsed) = CtMeta::from_meta(&meta)
    {
        let again = parsed.to_meta().expect("a parsed container meta encodes");
        assert_eq!(
            CtMeta::from_meta(&again).ok(),
            Some(parsed),
            "a container meta must decode to what was encoded"
        );
    }
    if let Ok(tree) = pb::Tree::decode(data) {
        let _ = ct::check_top(&tree.nodes);
        if let Ok((conf, fw)) = ct::check_config_dir(&tree.nodes) {
            assert!(conf.size <= ct::MAX_CT_CONFIG_BYTES as u64);
            assert!(fw.is_none_or(|n| n.size <= ct::MAX_CT_CONFIG_BYTES as u64));
        }
    }
    if let Ok(text) = std::str::from_utf8(data) {
        if let Ok(map) = IdMap::parse_canonical(text) {
            assert_eq!(
                map.canonical().ok().as_deref(),
                Some(text),
                "a parsed id map must be canonical"
            );
        }
        let _ = ct::config_lines(text).count();
        let _ = IdMap::from_config(text);
    }
    let map = IdMap::default_unprivileged();
    for dir in [Direction::HostToCt, Direction::CtToHost] {
        if let Ok(out) = translate_acl(data, dir, &map, &mut 0) {
            assert_eq!(out.len(), data.len(), "an ACL keeps its entries");
            assert!(translate_acl(&out, dir, &map, &mut 0).is_ok());
        }
        if let Ok(Some(out)) = translate_caps(data, dir, &map, &mut 0) {
            assert!(translate_caps(&out, dir, &map, &mut 0).is_ok());
        }
    }
}

/// A hostile segment list is either refused or one the writer would have
/// written, and the merged file node it makes is a valid sparse file.
pub fn image_layout(data: &[u8]) {
    if let Ok(tree) = pb::Tree::decode(data)
        && let Ok(layout) = parse_layout(&tree.nodes)
    {
        let merged = merge_segments(&layout, b"x.img");
        assert!(
            check_extents(&merged).is_ok(),
            "a merged image must have valid extents"
        );
        let mut head = merged.clone();
        head.content.clear();
        assert_eq!(
            merge_head(&layout, b"x.img"),
            head,
            "the head is the node without its ids"
        );
        // The chunk table is one entry per chunk; only a one-segment image is built here.
        if layout.size <= SEGMENT_CHUNKS * IMAGE_CHUNK_SIZE {
            let table = layout
                .chunk_table()
                .expect("a parsed layout has a chunk table");
            assert_eq!(table.len() as u64, layout.size.div_ceil(IMAGE_CHUNK_SIZE));
            let per_segment = layout.segment_size.div_ceil(IMAGE_CHUNK_SIZE).max(1);
            let again = segment_nodes(layout.size, &table, per_segment)
                .expect("a parsed layout is one the writer can write");
            assert_eq!(again.len(), layout.segments.len());
            for (a, b) in again.iter().zip(&layout.segments) {
                assert_eq!(
                    (&a.name, a.size, &a.content, &a.sparse_extents),
                    (&b.name, b.size, &b.content, &b.sparse_extents),
                    "a parsed segment must be the canonical one"
                );
            }
        }
    }
    if let Ok(claimed) = pb::Source::decode(data)
        && let Ok((key, meta)) = source::decode(Some(&claimed))
        && let Some(name) = source::label(&key, &meta)
    {
        let _ = BitmapBinding::from_meta(&meta, &name);
        let _ = FullReason::from_meta(&meta, &name);
        if let Ok(parsed) = ImageMeta::from_meta(&meta, &name) {
            assert_eq!(
                ImageMeta::from_meta(&parsed.to_meta(&name), &name).ok(),
                Some(parsed),
                "image meta must read back what it writes"
            );
        }
    }
}

/// A state file is refused or one `encode` would have written, and a bitmap
/// name is refused or the one `name` builds for its generation.
pub fn image_state(data: &[u8]) {
    let repo: RepoId = [0x11; 16];
    let Ok(key) = SourceKey::image("disk") else {
        return;
    };
    if let Ok(state) = ImageState::parse(data, &repo, b"host-a", &key) {
        assert_eq!(
            ImageState::parse(
                &state.encode(&repo, b"host-a", &key),
                &repo,
                b"host-a",
                &key
            )
            .ok(),
            Some(state),
            "an encoded state must parse to what was encoded"
        );
    }
    let names = BitmapNames::new(&repo, b"host-a", &key);
    if let Some(generation) = names.parse(&String::from_utf8_lossy(data)) {
        assert_eq!(
            names.name(generation).as_bytes(),
            data,
            "a parsed bitmap name is the one built for its generation"
        );
    }
}

const PASSWORD_WORK_BUDGET: u64 = 1 << 10;

fn fixed_recovery_key() -> &'static RecoveryKey {
    static KEY: OnceLock<RecoveryKey> = OnceLock::new();
    KEY.get_or_init(|| {
        let text = include_str!("../../lodestore-format/tests/fixtures/v1/RECOVERY_KEY");
        RecoveryKey::parse(text.trim()).expect("the fixture recovery key parses")
    })
}

pub fn key_slot(data: &[u8]) {
    if data.len() as u64 > MAX_SLOT_SIZE {
        return;
    }
    let repo_id = keys().repo_id();
    let _ = slot_kdf_work(data, pb::SlotKind::Recovery);
    let _ = open_recovery_slot(data, repo_id, fixed_recovery_key());
    if slot_kdf_work(data, pb::SlotKind::Password).is_some_and(|work| work <= PASSWORD_WORK_BUDGET)
    {
        let _ = open_password_slot(data, repo_id, b"fuzz");
    }
}

pub fn writer_keys(data: &[u8]) {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = WriteKeys::from_file_text(text);
    }
}

pub fn recovery_key(data: &[u8]) {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = RecoveryKey::parse(text);
        let _ = looks_like_recovery_key(text);
    }
}

pub fn minisign_signature(data: &[u8]) {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = Signature::parse(text);
        let _ = PublicKey::parse(text);
        let _ = TrustedRelease::parse(text);
    }
}

pub fn snapshot_open(data: &[u8]) {
    let keys = keys();
    let (repo_id, private_key) = (keys.repo_id(), keys.hpke_private_key());
    let mut entropy = entropy();
    let snapshot_key = entropy.key32();
    if let Ok((hpke_enc, sealed_key)) =
        seal_snapshot_key(keys.hpke_public_key(), repo_id, &snapshot_key, &mut entropy)
        && let Ok(sealed) = seal_snapshot_body(&snapshot_key, repo_id, data, &mut entropy)
        && sealed.len() >= NONCE_LEN
    {
        let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
        let bytes = pb::SnapshotEnvelope {
            version: ENVELOPE_VERSION,
            hpke_enc,
            sealed_key,
            nonce: nonce.to_vec(),
            ciphertext: ciphertext.to_vec(),
        }
        .encode_to_vec();
        let id = *blake3::hash(&bytes).as_bytes();
        assert_eq!(
            open_snapshot(private_key, repo_id, &id, &bytes).ok(),
            pb::Snapshot::decode(data).ok(),
            "a sealed snapshot must open to what it was sealed from"
        );
    }
    let _ = open_snapshot(private_key, repo_id, blake3::hash(data).as_bytes(), data);
}

pub(crate) const NBD_BITMAPS: [&str; 2] = ["b0", "b1"];
pub(crate) const NBD_READ_LEN: u64 = 64 << 10;
pub(crate) const NBD_STATUS_LEN: u64 = 1 << 20;
pub(crate) const NBD_EXTENTS_SHIFT: u64 = 1;

/// The extents of the image source over the first status window, shifted so
/// that the first and the last piece are cut by the window.
pub(crate) async fn nbd_extents<S>(client: Client<S>) -> (Client<S>, Result<Vec<ImageExtent>>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let window = NBD_STATUS_LEN
        .min(client.size())
        .saturating_sub(NBD_EXTENTS_SHIFT);
    let mut source = NbdSource::new(client);
    let found = source.extents(NBD_EXTENTS_SHIFT, window).await;
    (source.into_client(), found)
}

pub(crate) fn nbd_limits() -> NbdLimits {
    NbdLimits {
        max_export_size: 1 << 40,
        ..NbdLimits::default()
    }
}

pub(crate) fn nbd_export() -> MemExport {
    static DATA: OnceLock<Vec<u8>> = OnceLock::new();
    let data = DATA.get_or_init(|| {
        let mut data: Vec<u8> = (0..NBD_STATUS_LEN)
            .map(|i| ((i * 31 + (i >> 12)) % 251 + 1) as u8)
            .collect();
        data[(64 << 10)..(512 << 10)].fill(0);
        data
    });
    MemExport::new(data.clone())
        .with_bitmap("b0", vec![4096..8192, (256 << 10)..(320 << 10)])
        .with_bitmap(
            "b1",
            vec![(768 << 10)..(769 << 10), (900 << 10)..(901 << 10)],
        )
}

pub(crate) fn nbd_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("a current-thread runtime builds")
}

struct Scripted<'a> {
    input: &'a [u8],
    pos: usize,
}

impl AsyncRead for Scripted<'_> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let rest = self.input.get(self.pos..).unwrap_or_default();
        let n = rest.len().min(buf.remaining());
        buf.put_slice(rest.get(..n).unwrap_or_default());
        self.pos += n;
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for Scripted<'_> {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// The bytes a hostile server sends: the client either refuses them or reads
/// what they say, and never holds more than the replies it may accept. The
/// extents of the image source, which join the allocation and dirty replies,
/// are driven on top of the client too.
pub fn nbd_client(data: &[u8]) {
    nbd_runtime().block_on(async {
        let stream = Scripted {
            input: data,
            pos: 0,
        };
        let Ok(mut client) = Client::handshake(stream, "", &NBD_BITMAPS, nbd_limits()).await else {
            return;
        };
        let size = client.size();
        if size > 0 {
            let len = NBD_READ_LEN.min(size).min(u64::from(client.max_read()));
            let _ = client.read(0, len as u32).await;
            let _ = client
                .block_status(0, NBD_STATUS_LEN.min(size) as u32)
                .await;
            client = nbd_extents(client).await.0;
        }
        let _ = client.disconnect().await;
    });
}

/// The bytes a hostile client sends: the server answers or hangs up, and
/// never writes to the export.
pub fn nbd_server(data: &[u8]) {
    nbd_runtime().block_on(async {
        let stream = Scripted {
            input: data,
            pos: 0,
        };
        let _ = serve(stream, &mut nbd_export(), &NbdLimits::default()).await;
    });
}

fn take<'a>(rest: &mut &'a [u8], len: usize) -> Option<&'a [u8]> {
    let (head, tail) = rest.split_at_checked(len)?;
    *rest = tail;
    Some(head)
}

fn take_text(rest: &mut &[u8]) -> Option<String> {
    let len = usize::from(take(rest, 1)?[0]);
    let bytes = take(rest, len.min(rest.len()))?;
    Some(String::from_utf8_lossy(bytes).into_owned())
}

/// Input: prefix and directory (a length byte, then that many bytes each),
/// then entries of a flags byte (bits 0-1 the kind: 0 file, 1 directory,
/// otherwise other; bit 2 a size follows; bit 3 a modification time
/// follows), the size as 8 little-endian bytes, the time as 4, and the name
/// like the prefix.
fn take_raw_entry(rest: &mut &[u8]) -> Option<RawEntry> {
    let flags = take(rest, 1)?[0];
    let size = if flags & 4 != 0 {
        Some(u64::from_le_bytes(take(rest, 8)?.try_into().ok()?))
    } else {
        None
    };
    let mtime_s = if flags & 8 != 0 {
        Some(u32::from_le_bytes(take(rest, 4)?.try_into().ok()?))
    } else {
        None
    };
    Some(RawEntry {
        name: take_text(rest)?,
        size,
        mtime_s,
        kind: match flags & 3 {
            0 => RawKind::File,
            1 => RawKind::Dir,
            _ => RawKind::Other,
        },
    })
}

/// A directory listing from a hostile SFTP server: every key that comes out
/// is a sound key under the directory.
pub fn sftp_listing(data: &[u8]) {
    let mut rest = data;
    let (Some(prefix), Some(dir)) = (take_text(&mut rest), take_text(&mut rest)) else {
        return;
    };
    let mut raw = Vec::new();
    while raw.len() < 256 {
        let Some(entry) = take_raw_entry(&mut rest) else {
            break;
        };
        raw.push(entry);
    }
    if let Err(violation) = check_listing(&prefix, &dir, &raw) {
        panic!("{violation}");
    }
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
    ("source_decode", source_decode),
    ("image_layout", image_layout),
    ("ct_layout", ct_layout),
    ("image_state", image_state),
    ("nbd_client", nbd_client),
    ("nbd_server", nbd_server),
    ("verify_shard", verify_shard),
    ("snapshot_resolves", snapshot_resolves),
    ("restore_plan", restore_plan),
    ("key_slot", key_slot),
    ("writer_keys", writer_keys),
    ("recovery_key", recovery_key),
    ("minisign_signature", minisign_signature),
    ("snapshot_open", snapshot_open),
    ("sftp_listing", sftp_listing),
];
