use std::fmt;

use argon2::{Algorithm, Argon2, Params, Version};
use data_encoding::{BASE32_NOPAD, BASE64};
use hpke::{Deserializable, Kem, Serializable};
use prost::Message;
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::config::{self, AEAD_ID_CHACHA20POLY1305, KDF_ID_HKDF_SHA256, KEM_ID_XWING};
use crate::crypto::{
    CTX_CDC_SEED, CTX_CHUNK_ID_KEY, CTX_HPKE_IKM, CTX_INDEX_KEY, CTX_SERVER_MAC, Entropy,
    HpkePrivateKey, HpkePublicKey, Key32, NONCE_LEN, RepoId, aead_open, aead_seal, derive_key,
    key_slot_aad,
};
use crate::error::{Error, Result};
use crate::pb;

pub const KDF_ARGON2ID: u32 = 1;
pub const KDF_BLAKE3: u32 = 2;
pub const SLOT_VERSION: u32 = 1;

pub const ARGON2_MEMORY_KIB: u32 = 65536;
pub const ARGON2_ITERATIONS: u32 = 3;
pub const ARGON2_PARALLELISM: u32 = 4;

pub const MAX_ARGON2_MEMORY_KIB: u32 = 1 << 20;
pub const MAX_ARGON2_ITERATIONS: u32 = 16;
pub const MAX_ARGON2_PARALLELISM: u32 = 16;

/// The largest key slot object a reader downloads.
pub const MAX_SLOT_SIZE: u64 = 64 << 10;

/// The most Argon2 work (memory in KiB times passes) one opening spends across
/// all slots it tries.
pub const MAX_ARGON2_TOTAL_WORK: u64 = 1 << 24;

const SALT_LEN: usize = 16;

const WRITER_KEYS_HEADER: &str = "LODE-WRITER-KEYS v1";
const WRITER_KEYS_VERSION: u32 = 1;

const CTX_RECOVERY_KEK: &str = "lodestore v1 recovery kek";
const CTX_RECOVERY_CHECKSUM: &str = "lodestore v1 recovery checksum";
const RECOVERY_PREFIX: &str = "lode-rk1-";
const RECOVERY_CHECKSUM_LEN: usize = 3;
const RECOVERY_RAW_LEN: usize = 32 + RECOVERY_CHECKSUM_LEN;

/// Cost parameters of the password KDF. They are written into the slot, so a
/// repository keeps working after the defaults are raised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KdfCost {
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

impl Default for KdfCost {
    fn default() -> Self {
        KdfCost {
            memory_kib: ARGON2_MEMORY_KIB,
            iterations: ARGON2_ITERATIONS,
            parallelism: ARGON2_PARALLELISM,
        }
    }
}

#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct MasterKey([u8; 32]);

impl MasterKey {
    pub fn generate(entropy: &mut Entropy) -> Self {
        MasterKey(entropy.bytes::<32>())
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        MasterKey(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MasterKey(<redacted>)")
    }
}

/// Seed of the content-defined chunker. Derived from the chunk-id key, so
/// every writer holding that key chunks identically; forced non-zero because a
/// zero seed selects the public gear table.
pub fn cdc_seed(k_id: &[u8; 32]) -> u64 {
    let derived = derive_key(CTX_CDC_SEED, k_id);
    let mut head = [0u8; 8];
    head.copy_from_slice(&derived[..8]);
    u64::from_le_bytes(head) | 1
}

/// Everything derived from the master key. Can read and write the repository.
pub struct ReadKeys {
    master: MasterKey,
    repo_id: RepoId,
    k_id: Key32,
    k_idx: Key32,
    k_srv: Key32,
    hpke_private: HpkePrivateKey,
    hpke_public: HpkePublicKey,
    cdc_seed: u64,
}

impl ReadKeys {
    pub fn derive(master: MasterKey, repo_id: RepoId) -> Self {
        let k_id = derive_key(CTX_CHUNK_ID_KEY, master.as_bytes());
        let k_idx = derive_key(CTX_INDEX_KEY, master.as_bytes());
        let k_srv = derive_key(CTX_SERVER_MAC, master.as_bytes());
        let ikm = derive_key(CTX_HPKE_IKM, master.as_bytes());
        let (hpke_private, hpke_public) = hpke::kem::XWing::derive_keypair(ikm.as_slice());
        let cdc_seed = cdc_seed(&k_id);
        ReadKeys {
            master,
            repo_id,
            k_id,
            k_idx,
            k_srv,
            hpke_private,
            hpke_public,
            cdc_seed,
        }
    }

    pub fn master(&self) -> &MasterKey {
        &self.master
    }

    /// A second handle on the same keys, for a reader alongside a writer.
    pub fn duplicate(&self) -> ReadKeys {
        ReadKeys::derive(self.master.clone(), self.repo_id)
    }

    pub fn repo_id(&self) -> &RepoId {
        &self.repo_id
    }

    pub fn k_id(&self) -> &[u8; 32] {
        &self.k_id
    }

    pub fn k_idx(&self) -> &[u8; 32] {
        &self.k_idx
    }

    pub fn k_srv(&self) -> &[u8; 32] {
        &self.k_srv
    }

    pub fn hpke_private_key(&self) -> &HpkePrivateKey {
        &self.hpke_private
    }

    pub fn hpke_public_key(&self) -> &HpkePublicKey {
        &self.hpke_public
    }

    pub fn public_key_bytes(&self) -> Vec<u8> {
        self.hpke_public.to_bytes().to_vec()
    }

    pub fn cdc_seed(&self) -> u64 {
        self.cdc_seed
    }

    pub fn write_keys(&self, config_hash: [u8; 32]) -> WriteKeys {
        WriteKeys {
            repo_id: self.repo_id,
            config_hash,
            k_id: self.k_id.clone(),
            k_idx: self.k_idx.clone(),
            hpke_public: self.hpke_public.clone(),
            cdc_seed: self.cdc_seed,
        }
    }
}

impl fmt::Debug for ReadKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadKeys")
            .field("repo_id", &self.repo_id)
            .field("keys", &"<redacted>")
            .finish()
    }
}

/// The agent key set: enough to chunk, deduplicate and write, nothing that
/// opens a pack, a tree or a snapshot. It holds no master key and no X-Wing
/// private key, so it cannot be used to open a reader:
///
/// ```compile_fail
/// use lodestore_format::keys::WriteKeys;
/// use lodestore_format::reader::Reader;
/// use lodestore_format::store::LocalFsStore;
///
/// fn open(store: LocalFsStore, keys: WriteKeys, config: &lodestore_format::pb::Config) {
///     let _ = Reader::open(store, keys, config);
/// }
/// ```
pub struct WriteKeys {
    pub repo_id: RepoId,
    pub config_hash: [u8; 32],
    pub k_id: Key32,
    pub k_idx: Key32,
    pub hpke_public: HpkePublicKey,
    pub cdc_seed: u64,
}

impl WriteKeys {
    /// The writer-keys file: a header line, then the base64 of a `WriterKeys`
    /// message, then a newline. This is the contract with the agent.
    pub fn to_file_text(&self) -> Zeroizing<String> {
        let mut message = pb::WriterKeys {
            version: WRITER_KEYS_VERSION,
            repo_id: self.repo_id.to_vec(),
            config_hash: self.config_hash.to_vec(),
            k_id: self.k_id.to_vec(),
            k_idx: self.k_idx.to_vec(),
            hpke: Some(pb::HpkeSuite {
                kem_id: KEM_ID_XWING,
                kdf_id: KDF_ID_HKDF_SHA256,
                aead_id: AEAD_ID_CHACHA20POLY1305,
                public_key: self.hpke_public.to_bytes().to_vec(),
            }),
        };
        let encoded = Zeroizing::new(message.encode_to_vec());
        message.k_id.zeroize();
        message.k_idx.zeroize();
        Zeroizing::new(format!(
            "{WRITER_KEYS_HEADER}\n{}\n",
            BASE64.encode(&encoded)
        ))
    }

    pub fn from_file_text(text: &str) -> Result<Self> {
        let bad = |reason: &str| Error::InvalidInput(format!("not a writer keys file: {reason}"));
        let (header, body) = text.split_once('\n').ok_or_else(|| bad("no header line"))?;
        if header.trim_end() != WRITER_KEYS_HEADER {
            return Err(bad(&format!("expected the header {WRITER_KEYS_HEADER}")));
        }
        let compact: Zeroizing<String> =
            Zeroizing::new(body.chars().filter(|c| !c.is_whitespace()).collect());
        let raw = Zeroizing::new(
            BASE64
                .decode(compact.as_bytes())
                .map_err(|_| bad("the body is not base64"))?,
        );
        let mut msg = pb::WriterKeys::decode(raw.as_slice()).map_err(|_| bad("undecodable"))?;
        let msg_k_id = Zeroizing::new(std::mem::take(&mut msg.k_id));
        let msg_k_idx = Zeroizing::new(std::mem::take(&mut msg.k_idx));
        if msg.version != WRITER_KEYS_VERSION {
            return Err(Error::InvalidInput(format!(
                "writer keys version {} is not supported by this build",
                msg.version
            )));
        }
        let fixed = |what: &str, bytes: &[u8]| -> Result<[u8; 32]> {
            bytes
                .try_into()
                .map_err(|_| bad(&format!("{what} must be 32 bytes")))
        };
        let repo_id: RepoId = msg
            .repo_id
            .as_slice()
            .try_into()
            .map_err(|_| bad("repo_id must be 16 bytes"))?;
        let config_hash = fixed("config_hash", &msg.config_hash)?;
        let k_id = Zeroizing::new(fixed("k_id", &msg_k_id)?);
        let k_idx = Zeroizing::new(fixed("k_idx", &msg_k_idx)?);
        let suite = msg.hpke.ok_or_else(|| bad("the hpke suite is missing"))?;
        if suite.kem_id != KEM_ID_XWING
            || suite.kdf_id != KDF_ID_HKDF_SHA256
            || suite.aead_id != AEAD_ID_CHACHA20POLY1305
        {
            return Err(bad("unsupported cipher suite"));
        }
        let hpke_public =
            HpkePublicKey::from_bytes(&suite.public_key).map_err(|_| bad("bad public key"))?;
        let seed = cdc_seed(&k_id);
        Ok(WriteKeys {
            repo_id,
            config_hash,
            k_id,
            k_idx,
            hpke_public,
            cdc_seed: seed,
        })
    }

    /// Fails unless `config_bytes` are the exact bytes the keys were exported
    /// for, so a swapped `config` with another public key is never written to.
    pub fn verify_config(&self, config_bytes: &[u8]) -> Result<()> {
        if bool::from(config::config_hash(config_bytes).ct_eq(&self.config_hash)) {
            Ok(())
        } else {
            Err(Error::corrupt(
                "config",
                "repository config changed since writer keys were exported",
            ))
        }
    }
}

impl fmt::Debug for WriteKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WriteKeys")
            .field("repo_id", &self.repo_id)
            .field("keys", &"<redacted>")
            .finish()
    }
}

/// What trying a slot costs an opening with a secret of `kind`: Argon2 work
/// (memory times passes) for a password slot, 0 for a recovery slot, a slot
/// that does not decode, or one whose parameters are out of range (refused
/// before any hashing). `None`: this secret does not try the slot at all.
pub fn slot_kdf_work(slot_bytes: &[u8], kind: pb::SlotKind) -> Option<u64> {
    let Ok(slot) = pb::KeySlot::decode(slot_bytes) else {
        return Some(0);
    };
    let algorithm = match kind {
        pb::SlotKind::Password => KDF_ARGON2ID,
        pb::SlotKind::Recovery => KDF_BLAKE3,
        pb::SlotKind::Unspecified | pb::SlotKind::Server => return None,
    };
    let kdf = slot.kdf.as_ref();
    if slot.version > SLOT_VERSION
        || slot.kind != kind as i32
        || kdf.map_or(0, |k| k.algorithm) != algorithm
    {
        return None;
    }
    let Some(k) = kdf.filter(|_| kind == pb::SlotKind::Password) else {
        return Some(0);
    };
    if k.memory_kib > MAX_ARGON2_MEMORY_KIB
        || k.iterations > MAX_ARGON2_ITERATIONS
        || k.parallelism > MAX_ARGON2_PARALLELISM
    {
        return Some(0);
    }
    Some(u64::from(k.memory_kib) * u64::from(k.iterations))
}

pub fn slot_object_name(slot_bytes: &[u8]) -> String {
    format!("keys/{}", blake3::hash(slot_bytes).to_hex())
}

fn argon2_kek(password: &[u8], kdf: &pb::KdfParams) -> Result<Key32> {
    if kdf.memory_kib > MAX_ARGON2_MEMORY_KIB
        || kdf.iterations > MAX_ARGON2_ITERATIONS
        || kdf.parallelism > MAX_ARGON2_PARALLELISM
    {
        return Err(Error::corrupt(
            "key slot",
            format!(
                "KDF parameters out of range (m={} KiB, t={}, p={})",
                kdf.memory_kib, kdf.iterations, kdf.parallelism
            ),
        ));
    }
    let params = Params::new(kdf.memory_kib, kdf.iterations, kdf.parallelism, Some(32))
        .map_err(|e| Error::corrupt("key slot", format!("invalid KDF parameters: {e}")))?;
    let mut kek = Zeroizing::new([0u8; 32]);
    off_async_workers(|| {
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params).hash_password_into(
            password,
            &kdf.salt,
            kek.as_mut_slice(),
        )
    })
    .map_err(|e| Error::corrupt("key slot", format!("KDF failed: {e}")))?;
    Ok(kek)
}

/// Runs CPU-heavy work so it does not stall a multi-threaded tokio runtime's
/// other tasks; elsewhere it simply runs inline.
fn off_async_workers<T>(f: impl FnOnce() -> T) -> T {
    use tokio::runtime::{Handle, RuntimeFlavor};
    match Handle::try_current() {
        Ok(h) if h.runtime_flavor() == RuntimeFlavor::MultiThread => tokio::task::block_in_place(f),
        _ => f(),
    }
}

fn seal_slot(
    kind: pb::SlotKind,
    kdf: pb::KdfParams,
    kek: &Key32,
    master: &MasterKey,
    repo_id: &RepoId,
    created: Option<pb::Timestamp>,
    entropy: &mut Entropy,
) -> Result<Vec<u8>> {
    let sealed = aead_seal(kek, &key_slot_aad(repo_id), master.as_bytes(), entropy)?;
    let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
    let slot = pb::KeySlot {
        version: SLOT_VERSION,
        kind: kind as i32,
        kdf: Some(kdf),
        nonce: nonce.to_vec(),
        ciphertext: ciphertext.to_vec(),
        created,
    };
    Ok(slot.encode_to_vec())
}

/// Decodes a slot and returns it only when this build understands it and it
/// is of the wanted kind and KDF.
fn decode_slot(
    slot_bytes: &[u8],
    kind: pb::SlotKind,
    algorithm: u32,
) -> Result<Option<pb::KeySlot>> {
    let slot = pb::KeySlot::decode(slot_bytes)
        .map_err(|e| Error::corrupt("key slot", format!("undecodable: {e}")))?;
    let kdf_algorithm = slot.kdf.as_ref().map_or(0, |k| k.algorithm);
    if slot.version > SLOT_VERSION || slot.kind != kind as i32 || kdf_algorithm != algorithm {
        return Ok(None);
    }
    if slot.nonce.len() != NONCE_LEN {
        return Err(Error::corrupt("key slot", "bad nonce length"));
    }
    Ok(Some(slot))
}

fn unwrap_master(slot: &pb::KeySlot, kek: &Key32, repo_id: &RepoId) -> Result<Option<MasterKey>> {
    let sealed = [slot.nonce.as_slice(), slot.ciphertext.as_slice()].concat();
    let Ok(plain) = aead_open(kek, &key_slot_aad(repo_id), &sealed) else {
        return Ok(None);
    };
    let plain = Zeroizing::new(plain);
    let bytes: [u8; 32] = plain
        .as_slice()
        .try_into()
        .map_err(|_| Error::corrupt("key slot", "master key has wrong length"))?;
    Ok(Some(MasterKey::from_bytes(bytes)))
}

/// Builds the encoded password slot that wraps the master key.
pub fn seal_password_slot(
    master: &MasterKey,
    repo_id: &RepoId,
    password: &[u8],
    cost: KdfCost,
    created: Option<pb::Timestamp>,
    entropy: &mut Entropy,
) -> Result<Vec<u8>> {
    let kdf = pb::KdfParams {
        algorithm: KDF_ARGON2ID,
        memory_kib: cost.memory_kib,
        iterations: cost.iterations,
        parallelism: cost.parallelism,
        salt: entropy.bytes::<SALT_LEN>().to_vec(),
    };
    let kek = argon2_kek(password, &kdf)?;
    seal_slot(
        pb::SlotKind::Password,
        kdf,
        &kek,
        master,
        repo_id,
        created,
        entropy,
    )
}

/// Tries to open one slot with a password. `Ok(None)` means the slot is not a
/// password slot this build understands or the password does not open it.
pub fn open_password_slot(
    slot_bytes: &[u8],
    repo_id: &RepoId,
    password: &[u8],
) -> Result<Option<MasterKey>> {
    let Some(slot) = decode_slot(slot_bytes, pb::SlotKind::Password, KDF_ARGON2ID)? else {
        return Ok(None);
    };
    let kek = argon2_kek(password, &slot.kdf.clone().unwrap_or_default())?;
    unwrap_master(&slot, &kek, repo_id)
}

/// A second, random secret that opens the repository without the password. It
/// already holds 256 bits of entropy, so no password KDF is applied.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct RecoveryKey([u8; 32]);

impl RecoveryKey {
    pub fn generate(entropy: &mut Entropy) -> Self {
        RecoveryKey(entropy.bytes::<32>())
    }

    fn checksum(&self) -> [u8; RECOVERY_CHECKSUM_LEN] {
        let digest = derive_key(CTX_RECOVERY_CHECKSUM, &self.0);
        let mut out = [0u8; RECOVERY_CHECKSUM_LEN];
        out.copy_from_slice(&digest[..RECOVERY_CHECKSUM_LEN]);
        out
    }

    fn kek(&self) -> Key32 {
        derive_key(CTX_RECOVERY_KEK, &self.0)
    }

    /// `lode-rk1-` followed by 14 groups of 4 base32 characters.
    pub fn encode(&self) -> Zeroizing<String> {
        let mut raw = Zeroizing::new([0u8; RECOVERY_RAW_LEN]);
        raw[..32].copy_from_slice(&self.0);
        raw[32..].copy_from_slice(&self.checksum());
        let b32 = Zeroizing::new(BASE32_NOPAD.encode(raw.as_slice()));
        let mut out = Zeroizing::new(String::with_capacity(
            RECOVERY_PREFIX.len() + b32.len() + b32.len() / 4 - 1,
        ));
        out.push_str(RECOVERY_PREFIX);
        for (i, group) in b32.as_bytes().chunks(4).enumerate() {
            if i > 0 {
                out.push('-');
            }
            out.extend(group.iter().map(|&b| char::from(b)));
        }
        out
    }

    /// Accepts any letter case and `-` or whitespace anywhere after the
    /// prefix; the checksum is verified before the key is used.
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let rest = match text.get(..RECOVERY_PREFIX.len()) {
            Some(head) if head.eq_ignore_ascii_case(RECOVERY_PREFIX) => {
                &text[RECOVERY_PREFIX.len()..]
            }
            _ => {
                return Err(Error::InvalidInput(format!(
                    "not a recovery key: it must start with {RECOVERY_PREFIX}"
                )));
            }
        };
        let compact: Zeroizing<String> = Zeroizing::new(
            rest.chars()
                .filter(|c| *c != '-' && !c.is_whitespace())
                .map(|c| c.to_ascii_uppercase())
                .collect(),
        );
        let raw = Zeroizing::new(BASE32_NOPAD.decode(compact.as_bytes()).map_err(|_| {
            Error::InvalidInput(
                "recovery key must use the letters A-Z and the digits 2-7 only, in 14 groups of 4"
                    .into(),
            )
        })?);
        if raw.len() != RECOVERY_RAW_LEN {
            return Err(Error::InvalidInput(
                "recovery key has the wrong length".into(),
            ));
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&raw[..32]);
        let key = RecoveryKey(key);
        if !bool::from(key.checksum().ct_eq(&raw[32..])) {
            return Err(Error::InvalidInput(
                "recovery key checksum mismatch: check the key for typos".into(),
            ));
        }
        Ok(key)
    }
}

impl fmt::Debug for RecoveryKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RecoveryKey(<redacted>)")
    }
}

/// True when a secret is meant as a recovery key rather than a password.
pub fn looks_like_recovery_key(text: &str) -> bool {
    text.get(..RECOVERY_PREFIX.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(RECOVERY_PREFIX))
}

pub fn seal_recovery_slot(
    master: &MasterKey,
    repo_id: &RepoId,
    key: &RecoveryKey,
    created: Option<pb::Timestamp>,
    entropy: &mut Entropy,
) -> Result<Vec<u8>> {
    let kdf = pb::KdfParams {
        algorithm: KDF_BLAKE3,
        ..Default::default()
    };
    seal_slot(
        pb::SlotKind::Recovery,
        kdf,
        &key.kek(),
        master,
        repo_id,
        created,
        entropy,
    )
}

/// `Ok(None)` means the slot is not a recovery slot or the key does not open it.
pub fn open_recovery_slot(
    slot_bytes: &[u8],
    repo_id: &RepoId,
    key: &RecoveryKey,
) -> Result<Option<MasterKey>> {
    let Some(slot) = decode_slot(slot_bytes, pb::SlotKind::Recovery, KDF_BLAKE3)? else {
        return Ok(None);
    };
    unwrap_master(&slot, &key.kek(), repo_id)
}

#[cfg(test)]
mod tests {
    use rand_chacha::ChaCha20Rng;
    use rand_chacha::rand_core::SeedableRng;

    use super::*;

    fn seeded(seed: u8) -> Entropy {
        Entropy::from_rng(ChaCha20Rng::from_seed([seed; 32]))
    }

    const LOW: KdfCost = KdfCost {
        memory_kib: 64,
        iterations: 1,
        parallelism: 1,
    };

    #[test]
    fn cdc_seed_nonzero_and_deterministic() {
        let mut ent = seeded(1);
        for _ in 0..64 {
            let k_id = ent.bytes::<32>();
            let seed = cdc_seed(&k_id);
            assert_ne!(seed, 0);
            assert_eq!(seed % 2, 1);
            assert_eq!(seed, cdc_seed(&k_id));
        }
        assert_ne!(cdc_seed(&[1; 32]), cdc_seed(&[2; 32]));

        let keys = ReadKeys::derive(MasterKey::from_bytes([9; 32]), [3; 16]);
        assert_eq!(keys.cdc_seed(), cdc_seed(keys.k_id()));
        assert_eq!(keys.write_keys([0; 32]).cdc_seed, keys.cdc_seed());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slot_opens_inside_a_multi_thread_runtime() {
        let mut ent = seeded(2);
        let master = MasterKey::generate(&mut ent);
        let repo = [4u8; 16];
        let slot = seal_password_slot(&master, &repo, b"pw", LOW, None, &mut ent).unwrap();
        let opened = open_password_slot(&slot, &repo, b"pw").unwrap().unwrap();
        assert_eq!(opened.as_bytes(), master.as_bytes());
    }

    #[test]
    fn slot_roundtrip_and_wrong_password() {
        let mut ent = seeded(2);
        let master = MasterKey::generate(&mut ent);
        let repo = [4u8; 16];
        let slot = seal_password_slot(&master, &repo, b"pw", LOW, None, &mut ent).unwrap();

        let opened = open_password_slot(&slot, &repo, b"pw").unwrap().unwrap();
        assert_eq!(opened.as_bytes(), master.as_bytes());
        assert!(open_password_slot(&slot, &repo, b"nope").unwrap().is_none());
        assert!(
            open_password_slot(&slot, &[5u8; 16], b"pw")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn slot_rejects_excessive_kdf_params() {
        let mut ent = seeded(3);
        let master = MasterKey::generate(&mut ent);
        let repo = [4u8; 16];
        let good = seal_password_slot(&master, &repo, b"pw", LOW, None, &mut ent).unwrap();
        let base = pb::KeySlot::decode(good.as_slice()).unwrap();

        let with = |edit: fn(&mut pb::KdfParams)| {
            let mut slot = base.clone();
            edit(slot.kdf.as_mut().unwrap());
            slot.encode_to_vec()
        };
        let evil = [
            with(|k| k.memory_kib = MAX_ARGON2_MEMORY_KIB + 1),
            with(|k| k.memory_kib = u32::MAX),
            with(|k| k.iterations = MAX_ARGON2_ITERATIONS + 1),
            with(|k| k.parallelism = MAX_ARGON2_PARALLELISM + 1),
        ];
        for slot in evil {
            let err = open_password_slot(&slot, &repo, b"pw").unwrap_err();
            assert!(matches!(err, Error::Corrupt { .. }), "{err:?}");
        }
        assert!(open_password_slot(&good, &repo, b"pw").unwrap().is_some());
    }

    #[test]
    fn key_types_do_not_print_secrets() {
        let keys = ReadKeys::derive(MasterKey::from_bytes([0xAB; 32]), [3; 16]);
        let shown = format!("{keys:?} {:?}", keys.master());
        assert!(shown.contains("redacted"));
        assert!(!shown.contains("171"));
        let shown = format!("{:?}", keys.write_keys([0; 32]));
        assert!(shown.contains("redacted"));
    }

    #[test]
    fn recovery_encode_roundtrip() {
        let mut ent = seeded(7);
        for _ in 0..16 {
            let key = RecoveryKey::generate(&mut ent);
            let parsed = RecoveryKey::parse(&key.encode()).unwrap();
            assert_eq!(parsed.0, key.0);
        }
    }

    #[test]
    fn recovery_checksum_rejects_typo() {
        let key = RecoveryKey::generate(&mut seeded(8));
        let good = key.encode().to_string();
        for pos in [RECOVERY_PREFIX.len(), good.len() / 2, good.len() - 1] {
            if good.as_bytes()[pos] == b'-' {
                continue;
            }
            let mut bad = good.clone().into_bytes();
            bad[pos] = if bad[pos] == b'A' { b'B' } else { b'A' };
            let err = RecoveryKey::parse(&String::from_utf8(bad).unwrap()).unwrap_err();
            assert!(err.to_string().contains("checksum"), "{err}");
        }
        assert!(RecoveryKey::parse("lode-rk1-AAAA").is_err());
        assert!(RecoveryKey::parse("hunter2").is_err());
    }

    #[test]
    fn recovery_case_and_separator_insensitive() {
        let key = RecoveryKey::generate(&mut seeded(9));
        let canonical = key.encode().to_string();
        let lower = canonical.to_lowercase();
        let spaced = format!("lode-rk1-{}", canonical[9..].replace('-', " "));
        let squashed = format!("  lode-rk1-{}\n", canonical[9..].replace('-', ""));
        for variant in [lower, spaced, squashed] {
            assert_eq!(RecoveryKey::parse(&variant).unwrap().0, key.0, "{variant}");
        }
        assert!(looks_like_recovery_key("LODE-RK1-xxxx"));
        assert!(!looks_like_recovery_key("correct horse"));
    }

    #[test]
    fn recovery_format_shape() {
        let key = RecoveryKey::generate(&mut seeded(10)).encode();
        let body = key.strip_prefix("lode-rk1-").unwrap();
        let groups: Vec<&str> = body.split('-').collect();
        assert_eq!(groups.len(), 14);
        for g in groups {
            assert_eq!(g.len(), 4);
            assert!(
                g.bytes().all(|b| matches!(b, b'A'..=b'Z' | b'2'..=b'7')),
                "{g}"
            );
        }
    }

    #[test]
    fn recovery_slot_roundtrip_and_isolation() {
        let mut ent = seeded(11);
        let master = MasterKey::generate(&mut ent);
        let repo = [4u8; 16];
        let key = RecoveryKey::generate(&mut ent);
        let slot = seal_recovery_slot(&master, &repo, &key, None, &mut ent).unwrap();

        let opened = open_recovery_slot(&slot, &repo, &key).unwrap().unwrap();
        assert_eq!(opened.as_bytes(), master.as_bytes());
        let other = RecoveryKey::generate(&mut ent);
        assert!(open_recovery_slot(&slot, &repo, &other).unwrap().is_none());
        assert!(open_password_slot(&slot, &repo, b"pw").unwrap().is_none());

        let password = seal_password_slot(&master, &repo, b"pw", LOW, None, &mut ent).unwrap();
        assert!(
            open_recovery_slot(&password, &repo, &key)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn writer_keys_file_roundtrip_and_pin() {
        let keys = ReadKeys::derive(MasterKey::from_bytes([7; 32]), [3; 16]);
        let config = b"config bytes";
        let write = keys.write_keys(config::config_hash(config));
        let text = write.to_file_text();
        assert!(text.starts_with("LODE-WRITER-KEYS v1\n") && text.ends_with('\n'));

        let back = WriteKeys::from_file_text(&text).unwrap();
        assert_eq!(back.repo_id, write.repo_id);
        assert_eq!(*back.k_id, *write.k_id);
        assert_eq!(*back.k_idx, *write.k_idx);
        assert_eq!(back.cdc_seed, keys.cdc_seed());
        assert_eq!(
            back.hpke_public.to_bytes().as_slice(),
            keys.public_key_bytes().as_slice()
        );
        assert!(back.verify_config(config).is_ok());
        assert!(back.verify_config(b"config bytez").is_err());

        for bad in [
            "",
            "LODE-WRITER-KEYS v2\nAAAA\n",
            "LODE-WRITER-KEYS v1\n!!!\n",
        ] {
            assert!(WriteKeys::from_file_text(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn recovery_key_debug_is_redacted() {
        let key = RecoveryKey::generate(&mut seeded(12));
        assert_eq!(format!("{key:?}"), "RecoveryKey(<redacted>)");
    }
}
