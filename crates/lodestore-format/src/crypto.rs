use std::convert::Infallible;

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{Key, KeyInit, XChaCha20Poly1305, XNonce};
use hpke::aead::ChaCha20Poly1305 as HpkeAead;
use hpke::kdf::HkdfSha256;
use hpke::kem::XWing;
use hpke::{Deserializable, Kem, OpModeR, OpModeS, Serializable};
use rand_core::{Rng, TryCryptoRng, TryRng};
use zeroize::Zeroizing;

use crate::error::{Error, Result};

pub const CTX_CHUNK_ID_KEY: &str = "lodestore v1 chunk id key";
pub const CTX_INDEX_KEY: &str = "lodestore v1 index key";
pub const CTX_HPKE_IKM: &str = "lodestore v1 hpke ikm";
pub const CTX_CDC_SEED: &str = "lodestore v1 cdc seed";
pub const CTX_PACK_KEY: &str = "lodestore v1 pack key";
pub const CTX_SNAPSHOT_KEY: &str = "lodestore v1 snapshot key";

pub const BLOB_TYPE_DATA: u8 = 1;
pub const BLOB_TYPE_TREE: u8 = 2;

pub const NONCE_LEN: usize = 24;
pub const TAG_LEN: usize = 16;
pub const HPKE_ENC_LEN: usize = 1120;
pub const SEALED_KEY_LEN: usize = 32 + TAG_LEN;

pub type RepoId = [u8; 16];
pub type BlobId = [u8; 32];
pub type Key32 = Zeroizing<[u8; 32]>;
pub type HpkePublicKey = <XWing as Kem>::PublicKey;
pub type HpkePrivateKey = <XWing as Kem>::PrivateKey;

const AAD_KEY_SLOT: &[u8] = b"key-slot";
const AAD_PACK_KEY: &[u8] = b"pack-key";
const AAD_SNAPSHOT_KEY: &[u8] = b"snapshot-key";
const AAD_PACK_HEADER: &[u8] = b"pack-header";
const AAD_INDEX: &[u8] = b"index";
const AAD_SNAPSHOT: &[u8] = b"snapshot";
const AAD_GC_MANIFEST: &[u8] = b"gc-manifest";
const AAD_WRITER_MARKER: &[u8] = b"writer-marker";

/// The only source of randomness in the format: master key, pack keys,
/// nonces, salts and HPKE encapsulation all draw from here, so a test can swap
/// in a deterministic generator.
pub struct Entropy(Source);

enum Source {
    Os,
    Custom(Box<dyn Rng + Send>),
}

impl Entropy {
    pub fn os() -> Self {
        Entropy(Source::Os)
    }

    pub fn from_rng<R: Rng + Send + 'static>(rng: R) -> Self {
        Entropy(Source::Custom(Box::new(rng)))
    }

    pub fn fill(&mut self, buf: &mut [u8]) {
        match &mut self.0 {
            Source::Os => getrandom::fill(buf).expect("operating system RNG failed"),
            Source::Custom(rng) => rng.fill_bytes(buf),
        }
    }

    pub fn bytes<const N: usize>(&mut self) -> [u8; N] {
        let mut out = [0u8; N];
        self.fill(&mut out);
        out
    }

    pub fn key32(&mut self) -> Key32 {
        Zeroizing::new(self.bytes::<32>())
    }
}

impl Default for Entropy {
    fn default() -> Self {
        Self::os()
    }
}

impl TryRng for Entropy {
    type Error = Infallible;

    fn try_next_u32(&mut self) -> std::result::Result<u32, Infallible> {
        Ok(u32::from_le_bytes(self.bytes::<4>()))
    }

    fn try_next_u64(&mut self) -> std::result::Result<u64, Infallible> {
        Ok(u64::from_le_bytes(self.bytes::<8>()))
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> std::result::Result<(), Infallible> {
        self.fill(dst);
        Ok(())
    }
}

impl TryCryptoRng for Entropy {}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// XChaCha20-Poly1305. Output is nonce (24) || ciphertext || tag (16).
pub fn aead_seal(
    key: &[u8; 32],
    aad: &[u8],
    plaintext: &[u8],
    entropy: &mut Entropy,
) -> Result<Vec<u8>> {
    let nonce = entropy.bytes::<NONCE_LEN>();
    let cipher = XChaCha20Poly1305::new(&Zeroizing::new(Key::from(*key)));
    let ct = cipher
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| Error::Crypto)?;
    Ok(cat(&[&nonce, &ct]))
}

pub fn aead_open(key: &[u8; 32], aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>> {
    if sealed.len() < NONCE_LEN + TAG_LEN {
        return Err(Error::Crypto);
    }
    let (nonce, ct) = sealed.split_at(NONCE_LEN);
    let nonce: [u8; NONCE_LEN] = nonce.try_into().map_err(|_| Error::Crypto)?;
    XChaCha20Poly1305::new(&Zeroizing::new(Key::from(*key)))
        .decrypt(&XNonce::from(nonce), Payload { msg: ct, aad })
        .map_err(|_| Error::Crypto)
}

pub fn key_slot_aad(repo_id: &RepoId) -> Vec<u8> {
    cat(&[repo_id, AAD_KEY_SLOT])
}

fn frame_aad(repo_id: &RepoId, blob_type: u8, blob_id: &BlobId) -> Vec<u8> {
    cat(&[repo_id, &[blob_type], blob_id])
}

/// Blob frame: AEAD over `codec || payload`, bound to repo, blob type and blob id.
pub fn seal_frame(
    key: &[u8; 32],
    repo_id: &RepoId,
    blob_type: u8,
    blob_id: &BlobId,
    codec: u8,
    payload: &[u8],
    entropy: &mut Entropy,
) -> Result<Vec<u8>> {
    let plain = Zeroizing::new(cat(&[&[codec], payload]));
    aead_seal(
        key,
        &frame_aad(repo_id, blob_type, blob_id),
        &plain,
        entropy,
    )
}

/// Returns `(codec, payload)`.
pub fn open_frame(
    key: &[u8; 32],
    repo_id: &RepoId,
    blob_type: u8,
    blob_id: &BlobId,
    frame: &[u8],
) -> Result<(u8, Vec<u8>)> {
    let mut plain = aead_open(key, &frame_aad(repo_id, blob_type, blob_id), frame)?;
    if plain.is_empty() {
        return Err(Error::Crypto);
    }
    let codec = plain.remove(0);
    Ok((codec, plain))
}

fn hpke_info(context: &str, repo_id: &RepoId) -> Vec<u8> {
    cat(&[context.as_bytes(), repo_id])
}

fn hpke_seal(
    context: &str,
    aad_suffix: &[u8],
    public_key: &HpkePublicKey,
    repo_id: &RepoId,
    key: &[u8; 32],
    entropy: &mut Entropy,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let (enc, sealed) = hpke::single_shot_seal_with_rng::<HpkeAead, HkdfSha256, XWing>(
        &OpModeS::Base,
        public_key,
        &hpke_info(context, repo_id),
        key,
        &cat(&[repo_id, aad_suffix]),
        entropy,
    )
    .map_err(|_| Error::Crypto)?;
    Ok((enc.to_bytes().to_vec(), sealed))
}

fn hpke_open(
    context: &str,
    aad_suffix: &[u8],
    private_key: &HpkePrivateKey,
    repo_id: &RepoId,
    enc: &[u8],
    sealed: &[u8],
) -> Result<Key32> {
    let enc = <XWing as Kem>::EncappedKey::from_bytes(enc).map_err(|_| Error::Crypto)?;
    let plain = Zeroizing::new(
        hpke::single_shot_open::<HpkeAead, HkdfSha256, XWing>(
            &OpModeR::Base,
            private_key,
            &enc,
            &hpke_info(context, repo_id),
            sealed,
            &cat(&[repo_id, aad_suffix]),
        )
        .map_err(|_| Error::Crypto)?,
    );
    let key: [u8; 32] = plain.as_slice().try_into().map_err(|_| Error::Crypto)?;
    Ok(Zeroizing::new(key))
}

/// Returns `(hpke_enc, sealed_key)`.
pub fn seal_pack_key(
    public_key: &HpkePublicKey,
    repo_id: &RepoId,
    pack_key: &[u8; 32],
    entropy: &mut Entropy,
) -> Result<(Vec<u8>, Vec<u8>)> {
    hpke_seal(
        CTX_PACK_KEY,
        AAD_PACK_KEY,
        public_key,
        repo_id,
        pack_key,
        entropy,
    )
}

pub fn open_pack_key(
    private_key: &HpkePrivateKey,
    repo_id: &RepoId,
    enc: &[u8],
    sealed: &[u8],
) -> Result<Key32> {
    hpke_open(
        CTX_PACK_KEY,
        AAD_PACK_KEY,
        private_key,
        repo_id,
        enc,
        sealed,
    )
}

/// Returns `(hpke_enc, sealed_key)`.
pub fn seal_snapshot_key(
    public_key: &HpkePublicKey,
    repo_id: &RepoId,
    snapshot_key: &[u8; 32],
    entropy: &mut Entropy,
) -> Result<(Vec<u8>, Vec<u8>)> {
    hpke_seal(
        CTX_SNAPSHOT_KEY,
        AAD_SNAPSHOT_KEY,
        public_key,
        repo_id,
        snapshot_key,
        entropy,
    )
}

pub fn open_snapshot_key(
    private_key: &HpkePrivateKey,
    repo_id: &RepoId,
    enc: &[u8],
    sealed: &[u8],
) -> Result<Key32> {
    hpke_open(
        CTX_SNAPSHOT_KEY,
        AAD_SNAPSHOT_KEY,
        private_key,
        repo_id,
        enc,
        sealed,
    )
}

pub fn seal_pack_header(
    key: &[u8; 32],
    repo_id: &RepoId,
    entries: &[u8],
    entropy: &mut Entropy,
) -> Result<Vec<u8>> {
    aead_seal(key, &cat(&[repo_id, AAD_PACK_HEADER]), entries, entropy)
}

pub fn open_pack_header(key: &[u8; 32], repo_id: &RepoId, sealed: &[u8]) -> Result<Vec<u8>> {
    aead_open(key, &cat(&[repo_id, AAD_PACK_HEADER]), sealed)
}

pub fn seal_index_shard(
    key: &[u8; 32],
    repo_id: &RepoId,
    shard: &[u8],
    entropy: &mut Entropy,
) -> Result<Vec<u8>> {
    aead_seal(key, &cat(&[repo_id, AAD_INDEX]), shard, entropy)
}

pub fn open_index_shard(key: &[u8; 32], repo_id: &RepoId, sealed: &[u8]) -> Result<Vec<u8>> {
    aead_open(key, &cat(&[repo_id, AAD_INDEX]), sealed)
}

pub fn seal_gc_manifest_body(
    key: &[u8; 32],
    repo_id: &RepoId,
    body: &[u8],
    entropy: &mut Entropy,
) -> Result<Vec<u8>> {
    aead_seal(key, &cat(&[repo_id, AAD_GC_MANIFEST]), body, entropy)
}

pub fn open_gc_manifest_body(key: &[u8; 32], repo_id: &RepoId, sealed: &[u8]) -> Result<Vec<u8>> {
    aead_open(key, &cat(&[repo_id, AAD_GC_MANIFEST]), sealed)
}

pub fn seal_writer_marker_body(
    key: &[u8; 32],
    repo_id: &RepoId,
    body: &[u8],
    entropy: &mut Entropy,
) -> Result<Vec<u8>> {
    aead_seal(key, &cat(&[repo_id, AAD_WRITER_MARKER]), body, entropy)
}

pub fn open_writer_marker_body(key: &[u8; 32], repo_id: &RepoId, sealed: &[u8]) -> Result<Vec<u8>> {
    aead_open(key, &cat(&[repo_id, AAD_WRITER_MARKER]), sealed)
}

pub fn seal_snapshot_body(
    key: &[u8; 32],
    repo_id: &RepoId,
    body: &[u8],
    entropy: &mut Entropy,
) -> Result<Vec<u8>> {
    aead_seal(key, &cat(&[repo_id, AAD_SNAPSHOT]), body, entropy)
}

pub fn open_snapshot_body(key: &[u8; 32], repo_id: &RepoId, sealed: &[u8]) -> Result<Vec<u8>> {
    aead_open(key, &cat(&[repo_id, AAD_SNAPSHOT]), sealed)
}

pub fn derive_key(context: &str, material: &[u8]) -> Key32 {
    Zeroizing::new(blake3::derive_key(context, material))
}

#[cfg(test)]
mod tests {
    use rand_chacha::ChaCha20Rng;
    use rand_chacha::rand_core::SeedableRng;

    use super::*;

    fn seeded(seed: u8) -> Entropy {
        Entropy::from_rng(ChaCha20Rng::from_seed([seed; 32]))
    }

    fn keypair(seed: u8) -> (HpkePrivateKey, HpkePublicKey) {
        XWing::derive_keypair(&[seed; 32])
    }

    #[test]
    fn crypto_aad_swap_rejected() {
        let mut ent = seeded(1);
        let key = [7u8; 32];
        let repo = [1u8; 16];
        let id = [2u8; 32];
        let frame = seal_frame(&key, &repo, BLOB_TYPE_DATA, &id, 1, b"payload", &mut ent).unwrap();

        let (codec, payload) = open_frame(&key, &repo, BLOB_TYPE_DATA, &id, &frame).unwrap();
        assert_eq!((codec, payload.as_slice()), (1, &b"payload"[..]));

        assert!(open_frame(&key, &[9u8; 16], BLOB_TYPE_DATA, &id, &frame).is_err());
        assert!(open_frame(&key, &repo, BLOB_TYPE_TREE, &id, &frame).is_err());
        assert!(open_frame(&key, &repo, BLOB_TYPE_DATA, &[3u8; 32], &frame).is_err());

        let mut tampered = frame.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(open_frame(&key, &repo, BLOB_TYPE_DATA, &id, &tampered).is_err());

        let header = seal_pack_header(&key, &repo, b"entries", &mut ent).unwrap();
        assert!(open_pack_header(&key, &repo, &header).is_ok());
        assert!(open_index_shard(&key, &repo, &header).is_err());
        assert!(open_snapshot_body(&key, &repo, &header).is_err());
    }

    #[test]
    fn crypto_hpke_xwing_roundtrip() {
        let mut ent = seeded(2);
        let repo = [1u8; 16];
        let (sk, pk) = keypair(10);
        let (other_sk, _) = keypair(11);
        let pack_key = [5u8; 32];

        let (enc, sealed) = seal_pack_key(&pk, &repo, &pack_key, &mut ent).unwrap();
        assert_eq!(enc.len(), HPKE_ENC_LEN);
        assert_eq!(sealed.len(), SEALED_KEY_LEN);

        assert_eq!(*open_pack_key(&sk, &repo, &enc, &sealed).unwrap(), pack_key);
        assert!(open_pack_key(&other_sk, &repo, &enc, &sealed).is_err());
        assert!(open_pack_key(&sk, &[2u8; 16], &enc, &sealed).is_err());
        assert!(open_snapshot_key(&sk, &repo, &enc, &sealed).is_err());

        let (enc, sealed) = seal_snapshot_key(&pk, &repo, &pack_key, &mut ent).unwrap();
        assert_eq!(enc.len(), HPKE_ENC_LEN);
        assert_eq!(sealed.len(), SEALED_KEY_LEN);
        assert_eq!(
            *open_snapshot_key(&sk, &repo, &enc, &sealed).unwrap(),
            pack_key
        );
        assert!(open_pack_key(&sk, &repo, &enc, &sealed).is_err());
    }

    #[test]
    fn crypto_frame_deterministic_with_seeded_entropy() {
        let key = [7u8; 32];
        let repo = [1u8; 16];
        let id = [2u8; 32];
        let seal = |seed| {
            seal_frame(
                &key,
                &repo,
                BLOB_TYPE_TREE,
                &id,
                0,
                b"same input",
                &mut seeded(seed),
            )
            .unwrap()
        };
        assert_eq!(seal(3), seal(3));
        assert_ne!(seal(3), seal(4));

        let (_, pk) = keypair(10);
        let hpke = |seed| seal_pack_key(&pk, &repo, &key, &mut seeded(seed)).unwrap();
        assert_eq!(hpke(3), hpke(3));
        assert_ne!(hpke(3), hpke(4));
    }
}
