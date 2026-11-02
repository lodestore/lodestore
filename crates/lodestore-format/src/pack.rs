//! Pack layout: `prefix || blob frame* || header frame || trailer`.
//!
//! The prefix carries the pack key sealed to the repository public key, so a
//! pack is self-describing for an index rebuild. Frames are XChaCha20-Poly1305
//! over `codec || payload`; the header lists every blob in fixed-width records.

use std::borrow::Cow;

use crate::crypto::{
    BLOB_TYPE_DATA, BLOB_TYPE_TREE, BlobId, Entropy, HPKE_ENC_LEN, HpkePrivateKey, HpkePublicKey,
    Key32, RepoId, SEALED_KEY_LEN, open_pack_header, open_pack_key, seal_frame, seal_pack_header,
    seal_pack_key,
};
use crate::error::{Error, Result};
use crate::index::hex32;
use crate::keys::ReadKeys;
use crate::pb;
use crate::store::BlobStore;

pub const PACK_MAGIC: &[u8; 6] = b"LODEPK";
pub const PACK_VERSION: u8 = 1;
pub const PACK_PREFIX_LEN: usize = 1175;
const _: () = assert!(PACK_PREFIX_LEN == 6 + 1 + HPKE_ENC_LEN + SEALED_KEY_LEN);

pub const ENTRY_LEN: usize = 45;
const TRAILER_LEN: usize = 4 + 6;
const HEADER_OVERHEAD: usize = crate::crypto::NONCE_LEN + crate::crypto::TAG_LEN;

pub const PACK_FIXED_LEN: usize = PACK_PREFIX_LEN + HEADER_OVERHEAD + TRAILER_LEN;

pub const PACK_TARGET_SIZE: usize = 32 << 20;
pub const PACK_MAX_SIZE: usize = 64 << 20;
pub const MAX_DATA_ULEN: usize = 4 << 20;
pub const MAX_TREE_ULEN: usize = 16 << 20;

pub const CODEC_RAW: u8 = 0;
pub const CODEC_ZSTD: u8 = 1;
const ZSTD_LEVEL: i32 = 3;

pub fn max_ulen(blob_type: u8) -> usize {
    if blob_type == BLOB_TYPE_TREE {
        MAX_TREE_ULEN
    } else {
        MAX_DATA_ULEN
    }
}

/// Whether a frame of `enc_len` bytes can hold a blob of this type: a nonce,
/// a codec byte, a payload no larger than zstd's worst case and a tag. Bounds
/// what an index entry may make a reader ask the store for.
pub fn frame_len_in_range(blob_type: u8, enc_len: u32) -> bool {
    let overhead = HEADER_OVERHEAD + 1;
    let max = overhead + zstd::zstd_safe::compress_bound(max_ulen(blob_type));
    (overhead..=max).contains(&(enc_len as usize))
}

/// Object key of a pack: `packs/<2 hex>/<hex>`.
pub fn pack_object_key(pack_id: &BlobId) -> String {
    let hex = hex32(pack_id);
    format!("packs/{}/{hex}", &hex[..2])
}

/// Stores the chunk raw when zstd saves less than 5%: already-compressed
/// data only costs CPU to decompress.
pub fn compress(data: &[u8]) -> Result<(u8, Cow<'_, [u8]>)> {
    let packed = zstd::bulk::compress(data, ZSTD_LEVEL)?;
    if packed.len() * 100 >= data.len() * 95 {
        Ok((CODEC_RAW, Cow::Borrowed(data)))
    } else {
        Ok((CODEC_ZSTD, Cow::Owned(packed)))
    }
}

/// Undoes [`compress`] with a hard output cap: the declared `ulen` is checked
/// against the per-type limit first and is the exact decompression capacity,
/// so a forged frame cannot make the reader allocate more.
pub fn decompress(codec: u8, payload: Vec<u8>, ulen: usize, blob_type: u8) -> Result<Vec<u8>> {
    if ulen > max_ulen(blob_type) {
        return Err(Error::corrupt(
            "blob",
            format!("declared size {ulen} exceeds the limit"),
        ));
    }
    let out = match codec {
        CODEC_RAW => payload,
        CODEC_ZSTD => zstd::bulk::decompress(&payload, ulen)
            .map_err(|e| Error::corrupt("blob", format!("decompression failed: {e}")))?,
        other => return Err(Error::corrupt("blob", format!("unknown codec {other}"))),
    };
    if out.len() != ulen {
        return Err(Error::corrupt(
            "blob",
            format!("size {} differs from the declared {ulen}", out.len()),
        ));
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackEntry {
    pub blob_id: BlobId,
    pub blob_type: u8,
    pub offset: u32,
    pub enc_len: u32,
    pub ulen: u32,
}

impl PackEntry {
    pub(crate) fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.blob_id);
        out.push(self.blob_type);
        out.extend_from_slice(&self.offset.to_le_bytes());
        out.extend_from_slice(&self.enc_len.to_le_bytes());
        out.extend_from_slice(&self.ulen.to_le_bytes());
    }
}

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

pub fn parse_entries(bytes: &[u8]) -> Result<Vec<PackEntry>> {
    if !bytes.len().is_multiple_of(ENTRY_LEN) {
        return Err(Error::corrupt(
            "pack entries",
            "length is not a multiple of the record size",
        ));
    }
    bytes
        .as_chunks::<ENTRY_LEN>()
        .0
        .iter()
        .map(|r| {
            let mut blob_id = [0u8; 32];
            blob_id.copy_from_slice(&r[..32]);
            let blob_type = r[32];
            if blob_type != BLOB_TYPE_DATA && blob_type != BLOB_TYPE_TREE {
                return Err(Error::corrupt("pack entries", "unknown blob type"));
            }
            Ok(PackEntry {
                blob_id,
                blob_type,
                offset: le_u32(&r[33..37]),
                enc_len: le_u32(&r[37..41]),
                ulen: le_u32(&r[41..45]),
            })
        })
        .collect()
}

/// The fixed-length start of a pack: the pack key sealed to the repository
/// public key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackPrefix {
    pub version: u8,
    pub hpke_enc: Vec<u8>,
    pub sealed_pack_key: Vec<u8>,
}

impl PackPrefix {
    pub fn open_key(&self, private_key: &HpkePrivateKey, repo_id: &RepoId) -> Result<Key32> {
        open_pack_key(private_key, repo_id, &self.hpke_enc, &self.sealed_pack_key)
            .map_err(|_| Error::corrupt("pack", "pack key does not open"))
    }
}

/// Checks magic, version and length of the first [`PACK_PREFIX_LEN`] bytes.
pub fn parse_prefix(bytes: &[u8]) -> Result<PackPrefix> {
    if bytes.len() != PACK_PREFIX_LEN || &bytes[..6] != PACK_MAGIC {
        return Err(Error::corrupt("pack", "bad prefix"));
    }
    if bytes[6] != PACK_VERSION {
        return Err(Error::UnsupportedVersion {
            what: "pack version",
            required: u32::from(bytes[6]),
            supported: u32::from(PACK_VERSION),
        });
    }
    let (enc, sealed) = bytes[7..].split_at(HPKE_ENC_LEN);
    Ok(PackPrefix {
        version: bytes[6],
        hpke_enc: enc.to_vec(),
        sealed_pack_key: sealed.to_vec(),
    })
}

/// Recovers the pack key from the first [`PACK_PREFIX_LEN`] bytes of a pack.
pub fn open_prefix(private_key: &HpkePrivateKey, repo_id: &RepoId, prefix: &[u8]) -> Result<Key32> {
    parse_prefix(prefix)?.open_key(private_key, repo_id)
}

/// Checks the last [`TRAILER_LEN`] bytes and returns the length of the sealed
/// header in front of them. A pack of `pack_size` bytes cannot hold a larger one.
pub fn parse_trailer(bytes: &[u8], pack_size: u64) -> Result<usize> {
    if bytes.len() != TRAILER_LEN || &bytes[4..] != PACK_MAGIC {
        return Err(Error::corrupt("pack", "bad trailer"));
    }
    let header_len = u64::from(le_u32(bytes));
    let room = pack_size.saturating_sub((PACK_PREFIX_LEN + TRAILER_LEN) as u64);
    if header_len < HEADER_OVERHEAD as u64 || header_len > room {
        return Err(Error::corrupt(
            "pack",
            "header length does not fit the pack",
        ));
    }
    Ok(header_len as usize)
}

/// What a pack says about itself.
#[derive(Debug)]
pub struct PackHeader {
    pub prefix: PackPrefix,
    pub entries: Vec<PackEntry>,
}

/// Reads the prefix, the trailer and the header of a pack with ranged GETs and
/// checks that every entry lies inside the frame area.
pub async fn read_pack_header<S: BlobStore>(
    store: &S,
    key: &str,
    size: u64,
    keys: &ReadKeys,
) -> Result<PackHeader> {
    let min = (PACK_PREFIX_LEN + HEADER_OVERHEAD + TRAILER_LEN) as u64;
    if size < min {
        return Err(Error::corrupt("pack", "too short"));
    }
    let prefix = parse_prefix(&store.get_range(key, 0..PACK_PREFIX_LEN as u64).await?)?;
    let trailer = store
        .get_range(key, size - TRAILER_LEN as u64..size)
        .await?;
    let header_len = parse_trailer(&trailer, size)? as u64;
    let frames_end = size - TRAILER_LEN as u64 - header_len;
    let sealed = store
        .get_range(key, frames_end..size - TRAILER_LEN as u64)
        .await?;
    if sealed.len() as u64 != header_len {
        return Err(Error::corrupt("pack", "header is shorter than declared"));
    }
    let pack_key = prefix.open_key(keys.hpke_private_key(), keys.repo_id())?;
    let plain = open_pack_header(&pack_key, keys.repo_id(), &sealed)
        .map_err(|_| Error::corrupt("pack", "header authentication failed"))?;
    let entries = parse_entries(&plain)?;
    for e in &entries {
        let end = u64::from(e.offset) + u64::from(e.enc_len);
        if (e.offset as usize) < PACK_PREFIX_LEN
            || end > frames_end
            || e.ulen as usize > max_ulen(e.blob_type)
            || !frame_len_in_range(e.blob_type, e.enc_len)
        {
            return Err(Error::corrupt("pack", "entry lies outside the frame area"));
        }
    }
    Ok(PackHeader { prefix, entries })
}

pub struct FinishedPack {
    pub bytes: Vec<u8>,
    pub pack_id: BlobId,
    pub record: pb::PackRecord,
}

/// Accumulates frames of one blob type. Data and tree blobs never share a
/// pack, and a blob is never split across packs.
pub struct PackBuilder {
    blob_type: u8,
    repo_id: RepoId,
    pack_key: Key32,
    hpke_enc: Vec<u8>,
    sealed_pack_key: Vec<u8>,
    buf: Vec<u8>,
    entries: Vec<PackEntry>,
}

impl PackBuilder {
    pub fn new(
        blob_type: u8,
        repo_id: &RepoId,
        public_key: &HpkePublicKey,
        entropy: &mut Entropy,
    ) -> Result<Self> {
        let pack_key = entropy.key32();
        let (hpke_enc, sealed_pack_key) = seal_pack_key(public_key, repo_id, &pack_key, entropy)?;
        let mut buf = Vec::with_capacity(PACK_PREFIX_LEN);
        buf.extend_from_slice(PACK_MAGIC);
        buf.push(PACK_VERSION);
        buf.extend_from_slice(&hpke_enc);
        buf.extend_from_slice(&sealed_pack_key);
        debug_assert_eq!(buf.len(), PACK_PREFIX_LEN);
        Ok(PackBuilder {
            blob_type,
            repo_id: *repo_id,
            pack_key,
            hpke_enc,
            sealed_pack_key,
            buf,
            entries: Vec::new(),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Size the finished pack would have, header and trailer included.
    pub fn size(&self) -> usize {
        self.buf.len() + self.entries.len() * ENTRY_LEN + HEADER_OVERHEAD + TRAILER_LEN
    }

    /// Adds a blob and returns the length of its frame.
    pub fn add(&mut self, blob_id: &BlobId, data: &[u8], entropy: &mut Entropy) -> Result<usize> {
        if data.len() > max_ulen(self.blob_type) {
            return Err(Error::InvalidInput(format!(
                "blob of {} bytes exceeds the limit",
                data.len()
            )));
        }
        let (codec, payload) = compress(data)?;
        let ulen = u32::try_from(data.len())
            .map_err(|_| Error::InvalidInput("blob does not fit a pack".into()))?;
        self.push_frame(blob_id, codec, &payload, ulen, entropy)
    }

    /// Adds a frame as given: any codec, any payload, any claimed size.
    /// For format conformance tests; nothing is checked or compressed.
    #[cfg(feature = "test-hooks")]
    pub fn add_raw(
        &mut self,
        blob_id: &BlobId,
        codec: u8,
        payload: &[u8],
        claimed_ulen: u32,
        entropy: &mut Entropy,
    ) -> Result<usize> {
        self.push_frame(blob_id, codec, payload, claimed_ulen, entropy)
    }

    fn push_frame(
        &mut self,
        blob_id: &BlobId,
        codec: u8,
        payload: &[u8],
        ulen: u32,
        entropy: &mut Entropy,
    ) -> Result<usize> {
        let frame = seal_frame(
            &self.pack_key,
            &self.repo_id,
            self.blob_type,
            blob_id,
            codec,
            payload,
            entropy,
        )?;
        let projected = self.size() + frame.len() + ENTRY_LEN;
        let to_u32 = |v: usize| {
            u32::try_from(v).map_err(|_| Error::InvalidInput("blob does not fit a pack".into()))
        };
        if projected > PACK_MAX_SIZE {
            return Err(Error::InvalidInput(
                "pack would exceed its maximum size".into(),
            ));
        }
        self.entries.push(PackEntry {
            blob_id: *blob_id,
            blob_type: self.blob_type,
            offset: to_u32(self.buf.len())?,
            enc_len: to_u32(frame.len())?,
            ulen,
        });
        let len = frame.len();
        self.buf.extend_from_slice(&frame);
        Ok(len)
    }

    pub fn finish(mut self, entropy: &mut Entropy) -> Result<FinishedPack> {
        self.entries.sort_by_key(|e| e.blob_id);
        let mut entries = Vec::with_capacity(self.entries.len() * ENTRY_LEN);
        for e in &self.entries {
            e.write(&mut entries);
        }
        let header = seal_pack_header(&self.pack_key, &self.repo_id, &entries, entropy)?;
        let header_len = u32::try_from(header.len())
            .map_err(|_| Error::InvalidInput("pack header too large".into()))?;
        self.buf.extend_from_slice(&header);
        self.buf.extend_from_slice(&header_len.to_le_bytes());
        self.buf.extend_from_slice(PACK_MAGIC);

        let pack_id = *blake3::hash(&self.buf).as_bytes();
        let record = pb::PackRecord {
            pack_id: pack_id.to_vec(),
            hpke_enc: self.hpke_enc,
            sealed_pack_key: self.sealed_pack_key,
            pack_size: self.buf.len() as u64,
            entries,
        };
        Ok(FinishedPack {
            bytes: self.buf,
            pack_id,
            record,
        })
    }
}

#[cfg(test)]
mod tests {
    use hpke::Kem;
    use hpke::kem::XWing;
    use rand_chacha::ChaCha20Rng;
    use rand_chacha::rand_core::SeedableRng;

    use super::*;
    use crate::crypto::{open_frame, open_pack_header};

    fn seeded(seed: u8) -> Entropy {
        Entropy::from_rng(ChaCha20Rng::from_seed([seed; 32]))
    }

    #[test]
    fn compress_incompressible_stored_raw() {
        let mut ent = seeded(1);
        let random = ent.bytes::<4096>();
        let (codec, stored) = compress(&random).unwrap();
        assert_eq!(codec, CODEC_RAW);
        assert_eq!(&*stored, &random[..]);

        let zeros = vec![0u8; 64 * 1024];
        let (codec, stored) = compress(&zeros).unwrap();
        assert_eq!(codec, CODEC_ZSTD);
        assert!(stored.len() < zeros.len() / 2);
        assert_eq!(
            decompress(codec, stored.into_owned(), zeros.len(), BLOB_TYPE_DATA).unwrap(),
            zeros
        );
    }

    #[test]
    fn compress_bomb_capped() {
        let bomb = zstd::bulk::compress(&vec![0u8; 5 << 20], 3).unwrap();
        assert!(bomb.len() < 1024);

        let corrupt = |codec, payload: Vec<u8>, ulen, ty| {
            let err = decompress(codec, payload, ulen, ty).unwrap_err();
            assert!(matches!(err, Error::Corrupt { .. }), "{err:?}");
        };
        // declared size above the per-type limit
        corrupt(CODEC_ZSTD, bomb.clone(), MAX_DATA_ULEN + 1, BLOB_TYPE_DATA);
        corrupt(CODEC_ZSTD, bomb.clone(), MAX_TREE_ULEN + 1, BLOB_TYPE_TREE);
        // forged small size on a payload that inflates past it
        corrupt(CODEC_ZSTD, bomb.clone(), 1000, BLOB_TYPE_DATA);
        corrupt(CODEC_ZSTD, bomb, MAX_DATA_ULEN, BLOB_TYPE_DATA);
        // declared size larger than what the payload really holds
        let small = zstd::bulk::compress(&vec![7u8; 1000], 3).unwrap();
        corrupt(CODEC_ZSTD, small, 2000, BLOB_TYPE_DATA);
        corrupt(CODEC_RAW, vec![1; 10], 11, BLOB_TYPE_DATA);
        corrupt(9, vec![1; 10], 10, BLOB_TYPE_DATA);
    }

    #[test]
    fn pack_builder_layout_and_readback() {
        let mut ent = seeded(2);
        let repo: RepoId = [3; 16];
        let (sk, pk) = XWing::derive_keypair(&[4u8; 32]);
        let mut b = PackBuilder::new(BLOB_TYPE_DATA, &repo, &pk, &mut ent).unwrap();
        assert!(b.is_empty());

        let blobs: Vec<(BlobId, Vec<u8>)> = (0u8..5)
            .map(|i| ([i + 1; 32], vec![i; 3000 + usize::from(i)]))
            .collect();
        for (id, data) in &blobs {
            b.add(id, data, &mut ent).unwrap();
        }
        let expected_size = b.size();
        let done = b.finish(&mut ent).unwrap();
        assert_eq!(done.bytes.len(), expected_size);
        assert_eq!(done.pack_id, *blake3::hash(&done.bytes).as_bytes());
        assert_eq!(done.record.pack_size as usize, done.bytes.len());
        assert!(done.bytes.ends_with(PACK_MAGIC));

        let key = open_prefix(&sk, &repo, &done.bytes[..PACK_PREFIX_LEN]).unwrap();
        let entries = parse_entries(&done.record.entries).unwrap();
        assert_eq!(entries.len(), blobs.len());
        for e in &entries {
            let frame = &done.bytes[e.offset as usize..(e.offset + e.enc_len) as usize];
            let (codec, payload) = open_frame(&key, &repo, e.blob_type, &e.blob_id, frame).unwrap();
            let plain = decompress(codec, payload, e.ulen as usize, e.blob_type).unwrap();
            let want = &blobs.iter().find(|(id, _)| *id == e.blob_id).unwrap().1;
            assert_eq!(&plain, want);
        }

        let n = done.bytes.len();
        let header_len = le_u32(&done.bytes[n - TRAILER_LEN..n - 6]) as usize;
        let header = &done.bytes[n - TRAILER_LEN - header_len..n - TRAILER_LEN];
        assert_eq!(
            open_pack_header(&key, &repo, header).unwrap(),
            done.record.entries
        );

        assert!(open_prefix(&sk, &[9; 16], &done.bytes[..PACK_PREFIX_LEN]).is_err());
    }

    #[test]
    fn pack_builder_refuses_past_max_size() {
        let mut ent = seeded(5);
        let repo: RepoId = [6; 16];
        let (_sk, pk) = XWing::derive_keypair(&[7u8; 32]);
        let mut b = PackBuilder::new(BLOB_TYPE_DATA, &repo, &pk, &mut ent).unwrap();

        // One reused buffer of incompressible bytes at the per-blob limit,
        // so every frame is stored raw and grows the pack by ~4 MiB.
        let mut data = vec![0u8; MAX_DATA_ULEN];
        let mut refused = None;
        for i in 0u32..64 {
            ent.fill(&mut data);
            let mut id = [0u8; 32];
            id[..4].copy_from_slice(&i.to_le_bytes());
            let before = b.size();
            match b.add(&id, &data, &mut ent) {
                Ok(_) => {
                    assert!(b.size() <= PACK_MAX_SIZE, "pack grew past the cap");
                }
                Err(e) => {
                    refused = Some((e, before));
                    break;
                }
            }
        }

        let (err, before) = refused.expect("builder never refused a frame");
        assert!(matches!(err, Error::InvalidInput(_)), "{err:?}");
        assert_eq!(b.size(), before, "refused frame must not be recorded");
        assert!(b.size() <= PACK_MAX_SIZE);

        let done = b.finish(&mut ent).unwrap();
        assert!(done.bytes.len() <= PACK_MAX_SIZE);
        assert_eq!(done.bytes.len() as u64, done.record.pack_size);
    }

    #[test]
    fn pack_object_key_uses_hex_prefix() {
        let mut id = [0u8; 32];
        id[0] = 0xab;
        id[1] = 0x01;
        id[31] = 0xff;
        let hex = format!("ab01{}ff", "0".repeat(58));
        assert_eq!(hex.len(), 64);
        assert_eq!(pack_object_key(&id), format!("packs/ab/{hex}"));
    }
}
