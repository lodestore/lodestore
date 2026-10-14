//! Frozen outputs of the format's primitives for fixed inputs. A dependency
//! bump that changes any of them would make existing repositories unreadable
//! or silently break deduplication. zstd output is not part of the vectors
//! (it is not guaranteed between versions); decompressing the committed frames
//! is checked through the golden repository.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use argon2::{Algorithm, Argon2, Params, Version};
use data_encoding::HEXLOWER;
use hpke::kem::XWing;
use hpke::{Kem, Serializable};
use lodestore_format::chunker::{Chunker, blob_id};
use lodestore_format::crypto::{
    BLOB_TYPE_DATA, CTX_CDC_SEED, CTX_CHUNK_ID_KEY, CTX_HPKE_IKM, CTX_INDEX_KEY, CTX_PACK_KEY,
    CTX_SNAPSHOT_KEY, Entropy, derive_key, open_frame, open_pack_header, open_pack_key,
    open_snapshot_key, seal_frame, seal_index_shard, seal_pack_header, seal_pack_key,
    seal_snapshot_body, seal_snapshot_key,
};
use lodestore_format::keys::{
    KdfCost, MasterKey, RecoveryKey, cdc_seed, open_password_slot, open_recovery_slot,
    seal_password_slot, seal_recovery_slot,
};
use lodestore_format::pack::{CODEC_RAW, PackBuilder, compress};
use lodestore_format::pb;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{Rng, SeedableRng};
use serde_json::{Value, json};

const KDF: KdfCost = KdfCost {
    memory_kib: 8192,
    iterations: 1,
    parallelism: 1,
};

fn path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1/vectors.json")
}

fn hex(bytes: &[u8]) -> String {
    HEXLOWER.encode(bytes)
}

fn rng(seed: u8) -> ChaCha20Rng {
    ChaCha20Rng::from_seed([seed; 32])
}

fn entropy(seed: u8) -> Entropy {
    Entropy::from_rng(rng(seed))
}

fn random(seed: u8, len: usize) -> Vec<u8> {
    let mut data = vec![0u8; len];
    rng(seed).fill_bytes(&mut data);
    data
}

fn fastcdc() -> Value {
    let k_id = [3u8; 32];
    let seed = cdc_seed(&k_id);
    let input = random(31, 12 << 20);
    let mut offset = 0usize;
    let chunks: Vec<Value> = Chunker::new(Cursor::new(&input), &k_id, seed)
        .map(|c| {
            let c = c.unwrap();
            let out = json!({ "offset": offset, "length": c.data.len(), "id": hex(&c.id) });
            offset += c.data.len();
            out
        })
        .collect();
    assert_eq!(offset, input.len());
    json!({
        "input": { "chacha20_seed": 31, "length": input.len() },
        "k_id": hex(&k_id),
        "seed": seed.to_string(),
        "keyed_blake3_of_empty": hex(&blob_id(&k_id, b"")),
        "chunks": chunks,
    })
}

fn derive() -> Value {
    let master = [1u8; 32];
    let contexts: serde_json::Map<String, Value> = [
        CTX_CHUNK_ID_KEY,
        CTX_INDEX_KEY,
        CTX_HPKE_IKM,
        CTX_CDC_SEED,
        CTX_PACK_KEY,
        CTX_SNAPSHOT_KEY,
    ]
    .into_iter()
    .map(|ctx| (ctx.to_owned(), json!(hex(&*derive_key(ctx, &master)))))
    .collect();
    json!({ "master": hex(&master), "contexts": contexts })
}

fn recovery_and_slots() -> Value {
    let password = b"vector password";
    let salt = [9u8; 16];
    let mut kek = [0u8; 32];
    Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        Params::new(KDF.memory_kib, KDF.iterations, KDF.parallelism, Some(32)).unwrap(),
    )
    .hash_password_into(password, &salt, &mut kek)
    .unwrap();

    let master = MasterKey::from_bytes([2u8; 32]);
    let repo_id = [4u8; 16];
    let created = Some(pb::Timestamp {
        seconds: 1_700_000_000,
        nanos: 0,
    });
    let recovery = RecoveryKey::generate(&mut entropy(5));
    let password_slot =
        seal_password_slot(&master, &repo_id, password, KDF, created, &mut entropy(6)).unwrap();
    let recovery_slot =
        seal_recovery_slot(&master, &repo_id, &recovery, created, &mut entropy(7)).unwrap();
    let opened = open_password_slot(&password_slot, &repo_id, password)
        .unwrap()
        .unwrap();
    assert_eq!(opened.as_bytes(), master.as_bytes());
    let opened = open_recovery_slot(&recovery_slot, &repo_id, &recovery)
        .unwrap()
        .unwrap();
    assert_eq!(opened.as_bytes(), master.as_bytes());

    json!({
        "recovery_key": { "chacha20_seed": 5, "encoded": recovery.encode().as_str() },
        "argon2id": {
            "password": "vector password",
            "salt": hex(&salt),
            "memory_kib": KDF.memory_kib,
            "iterations": KDF.iterations,
            "parallelism": KDF.parallelism,
            "kek": hex(&kek),
        },
        "slots": {
            "master": hex(master.as_bytes()),
            "repo_id": hex(&repo_id),
            "password_slot": hex(&password_slot),
            "recovery_slot": hex(&recovery_slot),
        },
    })
}

fn aead() -> Value {
    let key = [7u8; 32];
    let repo_id = [1u8; 16];
    let id = [2u8; 32];
    let payload = random(41, 300);
    let frame = seal_frame(
        &key,
        &repo_id,
        BLOB_TYPE_DATA,
        &id,
        0,
        &payload,
        &mut entropy(8),
    )
    .unwrap();
    let (codec, opened) = open_frame(&key, &repo_id, BLOB_TYPE_DATA, &id, &frame).unwrap();
    assert_eq!((codec, opened.as_slice()), (0, payload.as_slice()));

    let header = seal_pack_header(&key, &repo_id, &payload, &mut entropy(8)).unwrap();
    assert_eq!(open_pack_header(&key, &repo_id, &header).unwrap(), payload);
    let shard = seal_index_shard(&key, &repo_id, &payload, &mut entropy(8)).unwrap();
    let body = seal_snapshot_body(&key, &repo_id, &payload, &mut entropy(8)).unwrap();

    json!({
        "xchacha20poly1305": {
            "key": hex(&key),
            "repo_id": hex(&repo_id),
            "blob_id": hex(&id),
            "codec": 0,
            "chacha20_seed": 8,
            "plaintext": hex(&payload),
            "frame": hex(&frame),
            "pack_header": hex(&header),
            "index_shard": hex(&shard),
            "snapshot_body": hex(&body),
        }
    })
}

fn hpke() -> Value {
    let ikm = [10u8; 32];
    let repo_id = [1u8; 16];
    let key = [5u8; 32];
    let (sk, pk) = XWing::derive_keypair(&ikm);

    let (pack_enc, pack_sealed) = seal_pack_key(&pk, &repo_id, &key, &mut entropy(11)).unwrap();
    assert_eq!(
        *open_pack_key(&sk, &repo_id, &pack_enc, &pack_sealed).unwrap(),
        key
    );
    let (snap_enc, snap_sealed) = seal_snapshot_key(&pk, &repo_id, &key, &mut entropy(12)).unwrap();
    assert_eq!(
        *open_snapshot_key(&sk, &repo_id, &snap_enc, &snap_sealed).unwrap(),
        key
    );

    json!({
        "suite": "X-Wing / HKDF-SHA256 / ChaCha20-Poly1305",
        "ikm": hex(&ikm),
        "repo_id": hex(&repo_id),
        "plaintext_key": hex(&key),
        "public_key_blake3": hex(blake3::hash(&pk.to_bytes()).as_bytes()),
        "pack_key": { "chacha20_seed": 11, "enc": hex(&pack_enc), "sealed": hex(&pack_sealed) },
        "snapshot_key": { "chacha20_seed": 12, "enc": hex(&snap_enc), "sealed": hex(&snap_sealed) },
    })
}

fn pack() -> Value {
    let repo_id = [1u8; 16];
    let k_id = [3u8; 32];
    let (_, pk) = XWing::derive_keypair(&[10u8; 32]);
    let mut ent = entropy(13);
    let mut builder = PackBuilder::new(BLOB_TYPE_DATA, &repo_id, &pk, &mut ent).unwrap();
    let mut blobs = Vec::new();
    for (seed, len) in [(51u8, 17usize), (52, 1000), (53, 4096)] {
        let data = random(seed, len);
        assert_eq!(compress(&data).unwrap().0, CODEC_RAW);
        let id = blob_id(&k_id, &data);
        builder.add(&id, &data, &mut ent).unwrap();
        blobs.push(json!({ "chacha20_seed": seed, "length": len, "id": hex(&id) }));
    }
    let pack = builder.finish(&mut ent).unwrap();
    json!({
        "chacha20_seed": 13,
        "repo_id": hex(&repo_id),
        "k_id": hex(&k_id),
        "hpke_ikm": hex(&[10u8; 32]),
        "blobs": blobs,
        "length": pack.bytes.len(),
        "pack_id": hex(&pack.pack_id),
        "entries": hex(&pack.record.entries),
        "hpke_enc": hex(&pack.record.hpke_enc),
        "sealed_pack_key": hex(&pack.record.sealed_pack_key),
    })
}

fn vectors() -> Value {
    let mut all = serde_json::Map::new();
    all.insert("fastcdc".into(), fastcdc());
    all.insert("derive_key".into(), derive());
    for (name, value) in recovery_and_slots().as_object().unwrap() {
        all.insert(name.clone(), value.clone());
    }
    all.insert("aead".into(), aead());
    all.insert("hpke".into(), hpke());
    all.insert("pack".into(), pack());
    Value::Object(all)
}

#[test]
fn golden_vectors_match() {
    let committed: Value = serde_json::from_str(&fs::read_to_string(path()).unwrap()).unwrap();
    assert_eq!(vectors(), committed);
}

#[test]
#[ignore = "generates frozen fixtures; run once via LODE_GENERATE_FIXTURES=1 cargo nextest run -p lodestore-format --run-ignored only vectors_generate_v1"]
fn vectors_generate_v1() {
    if std::env::var_os("LODE_GENERATE_FIXTURES").is_none() {
        eprintln!("skipped: set LODE_GENERATE_FIXTURES=1 to generate fixtures");
        return;
    }
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path())
        .unwrap_or_else(|e| panic!("{}: {e}; the v1 vectors are frozen", path().display()));
    let text = serde_json::to_string_pretty(&vectors()).unwrap();
    std::io::Write::write_all(&mut out, format!("{text}\n").as_bytes()).unwrap();
}
