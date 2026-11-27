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
use lodestore_format::attest::{
    ATTEST_FRAME_V1, MARKER_LEGACY_SHARDS, attest_key, open_attestation, open_provenance,
    seal_attestation, seal_provenance,
};
use lodestore_format::chunker::{Chunker, blob_id};
use lodestore_format::crypto::{
    BLOB_TYPE_DATA, BlobId, CTX_ATTEST_SEAL, CTX_CDC_SEED, CTX_CHUNK_ID_KEY, CTX_HPKE_IKM,
    CTX_INDEX_KEY, CTX_PACK_KEY, CTX_SERVER_MAC, CTX_SNAPSHOT_KEY, Entropy, HPKE_ENC_LEN,
    SEALED_KEY_LEN, attest_mac, derive_key, open_frame, open_pack_header, open_pack_key,
    open_snapshot_key, seal_frame, seal_index_shard, seal_pack_header, seal_pack_key,
    seal_provenance_body, seal_snapshot_body, seal_snapshot_key,
};
use lodestore_format::image;
use lodestore_format::index::{Index, index_object_key, open_shard, seal_shard_superseding};
use lodestore_format::keys::{
    KdfCost, MasterKey, ReadKeys, RecoveryKey, cdc_seed, open_password_slot, open_recovery_slot,
    seal_password_slot, seal_recovery_slot,
};
use lodestore_format::locks::{
    gc_manifest_key, open_gc_manifest, open_writer_marker, seal_gc_manifest, seal_writer_marker,
    writer_done_key, writer_marker_key,
};
use lodestore_format::pack::{CODEC_RAW, PACK_PREFIX_LEN, PackBuilder, compress};
use lodestore_format::pb;
use lodestore_format::source::{self, Meta, SourceKey};
use prost::Message;
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

fn index_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1/index-vectors.json")
}

fn gc_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1/gc-vectors.json")
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

fn gc_vectors() -> Value {
    let key = [7u8; 32];
    let repo_id = [1u8; 16];
    let at = Some(pb::Timestamp {
        seconds: 1_700_000_000,
        nanos: 0,
    });

    let gc_id = [0x11u8; 16];
    let manifest = pb::GcManifest {
        version: 1,
        gc_id: gc_id.to_vec(),
        published: at,
        candidate_packs: vec![vec![0xA1; 32], vec![0xB2; 32]],
    };
    let sealed_manifest = seal_gc_manifest(&key, &repo_id, &manifest, &mut entropy(14)).unwrap();
    let manifest_key = gc_manifest_key(&gc_id);
    assert_eq!(
        open_gc_manifest(&key, &repo_id, &manifest_key, &sealed_manifest).unwrap(),
        manifest
    );

    let writer_id = [0x22u8; 16];
    let marker = pb::WriterMarker {
        version: 1,
        writer_id: writer_id.to_vec(),
        hostname: b"vector-host".to_vec(),
        started: at,
    };
    let sealed_marker = seal_writer_marker(&key, &repo_id, &marker, &mut entropy(15)).unwrap();
    let marker_key = writer_marker_key(&writer_id);
    assert_eq!(
        open_writer_marker(&key, &repo_id, &marker_key, &sealed_marker).unwrap(),
        marker
    );

    json!({
        "key": hex(&key),
        "repo_id": hex(&repo_id),
        "gc_manifest": {
            "chacha20_seed": 14,
            "object": manifest_key,
            "plaintext": hex(&manifest.encode_to_vec()),
            "sealed": hex(&sealed_manifest),
        },
        "writer_marker": {
            "chacha20_seed": 15,
            "object": marker_key,
            "done_object": writer_done_key(&writer_id),
            "plaintext": hex(&marker.encode_to_vec()),
            "sealed": hex(&sealed_marker),
        },
    })
}

#[test]
fn gc_vectors_match() {
    let committed: Value = serde_json::from_str(&fs::read_to_string(gc_path()).unwrap()).unwrap();
    assert_eq!(gc_vectors(), committed);
}

#[test]
#[ignore = "generates frozen fixtures; run once via LODE_GENERATE_FIXTURES=1 cargo nextest run -p lodestore-format --run-ignored only gc_vectors_generate_v1"]
fn gc_vectors_generate_v1() {
    if std::env::var_os("LODE_GENERATE_FIXTURES").is_none() {
        eprintln!("skipped: set LODE_GENERATE_FIXTURES=1 to generate fixtures");
        return;
    }
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(gc_path())
        .unwrap_or_else(|e| panic!("{}: {e}; the v1 vectors are frozen", gc_path().display()));
    let text = serde_json::to_string_pretty(&gc_vectors()).unwrap();
    std::io::Write::write_all(&mut out, format!("{text}\n").as_bytes()).unwrap();
}

fn attest_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1/attest-vectors.json")
}

fn attest_vectors() -> Value {
    let master = [1u8; 32];
    let repo_id = [1u8; 16];
    let keys = ReadKeys::derive(MasterKey::from_bytes(master), repo_id);
    let k_srv = *keys.k_srv();
    assert_eq!(*keys.k_srv(), *derive_key(CTX_SERVER_MAC, &master));
    let seal_key = derive_key(CTX_ATTEST_SEAL, &k_srv);

    let mac_body = b"attestation vector body";
    let mac = attest_mac(&k_srv, &repo_id, ATTEST_FRAME_V1, mac_body);

    let record = pb::ProvenanceRecord {
        snapshot_id: vec![0x33; 32],
        host_id: "vector-host-id".into(),
        hostname: "vector-host".into(),
        job_id: "vector-job".into(),
        verified_at: Some(pb::Timestamp {
            seconds: 1_700_000_000,
            nanos: 0,
        }),
        source: None,
    };
    let sealed_body = seal_provenance_body(
        &k_srv,
        &repo_id,
        b"provenance vector body",
        &mut entropy(16),
    )
    .unwrap();
    let sealed_record = seal_provenance(&keys, &record, &mut entropy(17)).unwrap();
    assert_eq!(open_provenance(&keys, &sealed_record).unwrap(), record);

    let attest_id = [0x44u8; 16];
    let attestation = pb::Attestation {
        attest_id: attest_id.to_vec(),
        created: Some(pb::Timestamp {
            seconds: 1_700_000_000,
            nanos: 0,
        }),
        shard_ids: vec![vec![0xA1; 32], vec![0xB2; 32]],
        markers: vec![MARKER_LEGACY_SHARDS],
        sealed_provenance: vec![sealed_record.clone()],
    };
    let frame = seal_attestation(&k_srv, &repo_id, &attestation);
    let object = attest_key(&attest_id);
    assert_eq!(
        open_attestation(&k_srv, &repo_id, &object, &frame).unwrap(),
        attestation
    );

    json!({
        "master": hex(&master),
        "repo_id": hex(&repo_id),
        "contexts": {
            CTX_SERVER_MAC: hex(&k_srv),
            CTX_ATTEST_SEAL: hex(&*seal_key),
        },
        "mac": {
            "frame": ATTEST_FRAME_V1,
            "body": hex(mac_body),
            "mac": hex(&mac),
        },
        "attestation": {
            "object": object,
            "plaintext": hex(&attestation.encode_to_vec()),
            "frame": hex(&frame),
        },
        "provenance": {
            "body_chacha20_seed": 16,
            "sealed_body": hex(&sealed_body),
            "record_chacha20_seed": 17,
            "record_plaintext": hex(&record.encode_to_vec()),
            "sealed_record": hex(&sealed_record),
        },
    })
}

#[test]
fn attest_vectors_match() {
    let committed: Value =
        serde_json::from_str(&fs::read_to_string(attest_path()).unwrap()).unwrap();
    assert_eq!(attest_vectors(), committed);
}

#[test]
#[ignore = "generates frozen fixtures; run once via LODE_GENERATE_FIXTURES=1 cargo nextest run -p lodestore-format --run-ignored only attest_vectors_generate_v1"]
fn attest_vectors_generate_v1() {
    if std::env::var_os("LODE_GENERATE_FIXTURES").is_none() {
        eprintln!("skipped: set LODE_GENERATE_FIXTURES=1 to generate fixtures");
        return;
    }
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(attest_path())
        .unwrap_or_else(|e| {
            panic!(
                "{}: {e}; the v1 vectors are frozen",
                attest_path().display()
            )
        });
    let text = serde_json::to_string_pretty(&attest_vectors()).unwrap();
    std::io::Write::write_all(&mut out, format!("{text}\n").as_bytes()).unwrap();
}

fn source_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1/source-vectors.json")
}

fn source_vectors() -> Value {
    const VM_A: &str = "0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d";
    const VM_B: &str = "7f3e9a10-2b4c-4d5e-8f60-a1b2c3d4e5f6";
    const CT: &str = "c0ffee00-1234-4abc-8def-0123456789ab";
    const MYSQL: &str = "5d0c4a1e-9b2f-4e7a-a6c3-18f2d4b7e9a0";

    let keys: Vec<(&str, SourceKey)> = vec![
        ("files", SourceKey::files()),
        (
            "proxmox_vm(pve1, VM_A)",
            SourceKey::proxmox_vm("pve1", VM_A).unwrap(),
        ),
        (
            "proxmox_vm(pve1, VM_B): the same VMID 101 with another uuid is another instance",
            SourceKey::proxmox_vm("pve1", VM_B).unwrap(),
        ),
        (
            "proxmox_ct(pve1, CT)",
            SourceKey::proxmox_ct("pve1", CT).unwrap(),
        ),
        (
            "proxmox_vm(my cluster/1, VM_A)",
            SourceKey::proxmox_vm("my cluster/1", VM_A).unwrap(),
        ),
        (
            "postgres(7300000000000000001, 16384)",
            SourceKey::postgres(7_300_000_000_000_000_001, 16384).unwrap(),
        ),
        (
            "postgres(7300000000000000001, 16385): another database of the cluster",
            SourceKey::postgres(7_300_000_000_000_000_001, 16385).unwrap(),
        ),
        (
            "postgres(18446744073709551615, 4294967295)",
            SourceKey::postgres(u64::MAX, u32::MAX).unwrap(),
        ),
        (
            "postgres_wal(7300000000000000001)",
            SourceKey::postgres_wal(7_300_000_000_000_000_001).unwrap(),
        ),
        (
            "mysql(MYSQL, shop)",
            SourceKey::mysql(MYSQL, "shop").unwrap(),
        ),
        ("mysql(MYSQL, a/b)", SourceKey::mysql(MYSQL, "a/b").unwrap()),
        (
            "mysql(MYSQL, 100%)",
            SourceKey::mysql(MYSQL, "100%").unwrap(),
        ),
        ("mysql(MYSQL, ü)", SourceKey::mysql(MYSQL, "ü").unwrap()),
        ("mysql(MYSQL, x y)", SourceKey::mysql(MYSQL, "x y").unwrap()),
    ];
    let keys: Vec<Value> = keys
        .iter()
        .map(|(input, key)| {
            assert_eq!(&SourceKey::parse(key.as_str()).unwrap(), key);
            json!({ "input": input, "key": key.as_str() })
        })
        .collect();

    let rejected: serde_json::Map<String, Value> = [
        "files",
        "oracle/x",
        "proxmox-vm/pve1",
        "proxmox-vm/pve1/101",
        "proxmox-vm/pve1/0B1C2D3E-4F50-4A6B-8C7D-9E0F1A2B3C4D",
        "proxmox-ct/pve1/105",
        "proxmox-ct/pve1/web-ct",
        "proxmox-ct/pve1/0B1C2D3E-4F50-4A6B-8C7D-9E0F1A2B3C4D",
        "postgres/0/1",
        "postgres/01/1",
        "postgres/1/0",
        "postgres/18446744073709551616/1",
        "postgres-wal/1/2",
        "mysql/0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d/a%2fb",
        "mysql/0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d/%41",
        "mysql/0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d/a%4",
        "mysql/0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d/%FF",
    ]
    .into_iter()
    .map(|bad| {
        let error = SourceKey::parse(bad).unwrap_err().to_string();
        (bad.to_owned(), json!(error))
    })
    .collect();

    let meta = |pairs: &[(&str, &str)]| -> Meta {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    };
    let labelled: Vec<(SourceKey, Meta)> = vec![
        (
            SourceKey::proxmox_vm("pve1", VM_A).unwrap(),
            meta(&[("name", "web-01"), ("vmid", "101")]),
        ),
        (
            SourceKey::proxmox_vm("pve1", VM_A).unwrap(),
            meta(&[("vmid", "101")]),
        ),
        (
            SourceKey::proxmox_vm("pve1", VM_A).unwrap(),
            meta(&[("name", "web-01")]),
        ),
        (
            SourceKey::proxmox_ct("pve1", CT).unwrap(),
            meta(&[("name", "dns"), ("vmid", "105")]),
        ),
        (
            SourceKey::postgres(7_300_000_000_000_000_001, 16384).unwrap(),
            meta(&[("database", "app"), ("server_version", "16.4")]),
        ),
        (
            SourceKey::mysql(MYSQL, "shop").unwrap(),
            meta(&[("server_version", "8.4.2")]),
        ),
        (
            SourceKey::postgres_wal(7_300_000_000_000_000_001).unwrap(),
            meta(&[("database", "app")]),
        ),
        (SourceKey::files(), Meta::new()),
    ];
    let labels: Vec<Value> = labelled
        .iter()
        .map(|(key, meta)| json!({ "key": key.as_str(), "meta": meta, "label": source::label(key, meta) }))
        .collect();

    let guest = source::encode(
        &SourceKey::proxmox_vm("pve1", VM_A).unwrap(),
        &meta(&[("name", "web-01"), ("vmid", "101")]),
    )
    .unwrap()
    .unwrap();
    let database = source::encode(
        &SourceKey::postgres(7_300_000_000_000_000_001, 16384).unwrap(),
        &meta(&[("database", "app"), ("server_version", "16.4")]),
    )
    .unwrap()
    .unwrap();
    let encoded = json!({
        "guest": hex(&guest.encode_to_vec()),
        "database": hex(&database.encode_to_vec()),
    });

    let keys_of = ReadKeys::derive(MasterKey::from_bytes([1u8; 32]), [1u8; 16]);
    let record = pb::ProvenanceRecord {
        snapshot_id: vec![0x33; 32],
        host_id: "vector-host-id".into(),
        hostname: "vector-host".into(),
        job_id: "vector-job".into(),
        verified_at: Some(pb::Timestamp {
            seconds: 1_700_000_000,
            nanos: 0,
        }),
        source: Some(database),
    };
    let sealed = seal_provenance(&keys_of, &record, &mut entropy(18)).unwrap();
    let opened = open_provenance(&keys_of, &sealed).unwrap();
    assert_eq!(opened, record);

    json!({
        "keys": keys,
        "rejected": rejected,
        "labels": labels,
        "encoded": encoded,
        "sealed": {
            "master": hex(&[1u8; 32]),
            "repo_id": hex(&[1u8; 16]),
            "record_chacha20_seed": 18,
            "record_plaintext": hex(&record.encode_to_vec()),
            "sealed_record": hex(&sealed),
        },
    })
}

#[test]
fn source_vectors_match() {
    let committed: Value =
        serde_json::from_str(&fs::read_to_string(source_path()).unwrap()).unwrap();
    assert_eq!(source_vectors(), committed);
}

#[test]
#[ignore = "generates frozen fixtures; run once via LODE_GENERATE_FIXTURES=1 cargo nextest run -p lodestore-format --run-ignored only source_vectors_generate_v1"]
fn source_vectors_generate_v1() {
    if std::env::var_os("LODE_GENERATE_FIXTURES").is_none() {
        eprintln!("skipped: set LODE_GENERATE_FIXTURES=1 to generate fixtures");
        return;
    }
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(source_path())
        .unwrap_or_else(|e| {
            panic!(
                "{}: {e}; the v1 vectors are frozen",
                source_path().display()
            )
        });
    let text = serde_json::to_string_pretty(&source_vectors()).unwrap();
    std::io::Write::write_all(&mut out, format!("{text}\n").as_bytes()).unwrap();
}

fn vector_record(pack: u8, blobs: &[u8]) -> pb::PackRecord {
    let mut entries = Vec::new();
    for (i, blob) in blobs.iter().enumerate() {
        entries.extend_from_slice(&[*blob; 32]);
        entries.push(BLOB_TYPE_DATA);
        entries.extend_from_slice(&(PACK_PREFIX_LEN as u32 + 100 * i as u32).to_le_bytes());
        entries.extend_from_slice(&100u32.to_le_bytes());
        entries.extend_from_slice(&200u32.to_le_bytes());
    }
    pb::PackRecord {
        pack_id: vec![pack; 32],
        hpke_enc: vec![0; HPKE_ENC_LEN],
        sealed_pack_key: vec![0; SEALED_KEY_LEN],
        pack_size: 5000,
        entries,
    }
}

/// Which pack every blob resolves to when `shards` are loaded in the given
/// order.
fn resolve(shards: &[(BlobId, pb::IndexShard)], order: &[usize], blobs: &[u8]) -> Vec<String> {
    let mut index = Index::default();
    for i in order {
        let (id, shard) = &shards[*i];
        index.add_shard(*id, shard).unwrap();
    }
    blobs
        .iter()
        .map(|b| {
            index
                .get(&[*b; 32], BLOB_TYPE_DATA)
                .map_or_else(String::new, |loc| hex(&loc.pack_id[..1]))
        })
        .collect()
}

/// `supersedes` only ranks shards: a superseded shard's record is used while
/// no current shard places the blob, so no shard can hide a blob.
fn index_vectors() -> Value {
    let key = [7u8; 32];
    let repo_id = [1u8; 16];
    let seal = |packs: Vec<pb::PackRecord>, supersedes: Vec<BlobId>, seed: u8| {
        seal_shard_superseding(&key, &repo_id, packs, supersedes, &mut entropy(seed)).unwrap()
    };
    let (b, b_bytes) = seal(vec![vector_record(0x10, &[4, 5])], vec![], 20);
    let (c, c_bytes) = seal(vec![vector_record(0x11, &[4])], vec![], 21);
    let (a, a_bytes) = seal(vec![vector_record(0x12, &[5])], vec![b], 22);
    let (commit, commit_bytes) = seal(vec![], vec![a, b, c], 23);
    let sealed = [
        (b, b_bytes),
        (c, c_bytes),
        (a, a_bytes),
        (commit, commit_bytes),
    ];
    let shards: Vec<(BlobId, pb::IndexShard)> = sealed
        .iter()
        .map(|(id, bytes)| (*id, open_shard(&key, &repo_id, id, bytes).unwrap()))
        .collect();
    let blobs = [4u8, 5, 6];
    let orders: [&[usize]; 4] = [&[0, 1, 2], &[2, 1, 0], &[1, 2, 0], &[0, 1, 2, 3]];
    let shard_json: Vec<Value> = sealed
        .iter()
        .zip(&shards)
        .zip(20u8..)
        .map(|(((id, bytes), (_, shard)), seed)| {
            json!({
                "chacha20_seed": seed,
                "object": index_object_key(id),
                "plaintext": hex(&shard.encode_to_vec()),
                "sealed": hex(bytes),
            })
        })
        .collect();
    let resolved: Vec<Value> = orders
        .iter()
        .map(|order| {
            json!({
                "order": order,
                "packs": resolve(&shards, order, &blobs),
            })
        })
        .collect();
    json!({
        "key": hex(&key),
        "repo_id": hex(&repo_id),
        "shards": shard_json,
        "blobs": blobs.iter().map(|b| hex(&[*b])).collect::<Vec<_>>(),
        "resolved_pack": resolved,
    })
}

#[test]
fn index_vectors_match() {
    let committed: Value =
        serde_json::from_str(&fs::read_to_string(index_path()).unwrap()).unwrap();
    assert_eq!(index_vectors(), committed);
}

#[test]
#[ignore = "generates frozen fixtures; run once via LODE_GENERATE_FIXTURES=1 cargo nextest run -p lodestore-format --run-ignored only index_vectors_generate_v1"]
fn index_vectors_generate_v1() {
    if std::env::var_os("LODE_GENERATE_FIXTURES").is_none() {
        eprintln!("skipped: set LODE_GENERATE_FIXTURES=1 to generate fixtures");
        return;
    }
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(index_path())
        .unwrap_or_else(|e| panic!("{}: {e}; the v1 vectors are frozen", index_path().display()));
    let text = serde_json::to_string_pretty(&index_vectors()).unwrap();
    std::io::Write::write_all(&mut out, format!("{text}\n").as_bytes()).unwrap();
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

fn image_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1/image-vectors.json")
}

fn image_vectors() -> Value {
    const C: u64 = image::IMAGE_CHUNK_SIZE;
    const MIB: u64 = 1 << 20;
    let k_id = [3u8; 32];

    let node_json = |n: &pb::Node| {
        json!({
            "name": String::from_utf8_lossy(&n.name),
            "size": n.size,
            "content": n.content.iter().map(|id| hex(id)).collect::<Vec<_>>(),
            "sparse_extents": n
                .sparse_extents
                .iter()
                .map(|e| json!({ "offset": e.offset, "length": e.length }))
                .collect::<Vec<_>>(),
        })
    };
    let tree_hex = |nodes: &[pb::Node]| {
        hex(&pb::Tree {
            nodes: nodes.to_vec(),
        }
        .encode_to_vec())
    };

    // D is a data chunk, Z a hole; chunk ids are the keyed hash of a fixed text.
    let cases: [(&str, u64, &str, u64); 9] = [
        ("whole segments on a segment boundary", 4 * C, "DDDD", 2),
        (
            "a hole in the middle and one at the end of a segment",
            5 * C,
            "DZDDZ",
            3,
        ),
        ("a hole at the end of the image", 3 * C, "DDZ", 3),
        ("a short last chunk of data", 2 * C + MIB, "DDD", 2),
        ("a short last chunk that is a hole", C + 1, "DZ", 2),
        ("a segment that is all hole", 4 * C, "ZZDD", 2),
        ("a dense segment", 2 * C, "DD", 2),
        ("an image of nothing but holes", 3 * C, "ZZZ", 8),
        ("an image of zero length", 0, "", 8),
    ];
    let layout: Vec<Value> = cases
        .iter()
        .enumerate()
        .map(|(case, &(description, size, mask, per_segment))| {
            let chunks: Vec<image::ChunkRef> = mask
                .chars()
                .enumerate()
                .map(|(i, c)| match c {
                    'D' => image::ChunkRef::Data(blob_id(
                        &k_id,
                        format!("image vector {case} chunk {i}").as_bytes(),
                    )),
                    _ => image::ChunkRef::Zero,
                })
                .collect();
            let nodes = image::segment_nodes(size, &chunks, per_segment).unwrap();
            let parsed = image::parse_layout(&nodes).unwrap();
            assert_eq!(parsed.chunk_table().unwrap(), chunks);
            let merged = image::merge_segments(&parsed, b"disk.img");
            json!({
                "description": description,
                "size": size,
                "chunks": mask,
                "segment_chunks": per_segment,
                "segments": nodes.iter().map(node_json).collect::<Vec<_>>(),
                "tree": tree_hex(&nodes),
                "merged": node_json(&merged),
            })
        })
        .collect();

    let id = |n: u8| blob_id(&k_id, &[n]).to_vec();
    let two = || {
        let chunks = [
            image::ChunkRef::Data([1; 32]),
            image::ChunkRef::Data([2; 32]),
        ];
        image::segment_nodes(2 * C, &chunks, 2).unwrap()
    };
    let four = || {
        let chunks = [image::ChunkRef::Data([1; 32]); 4];
        image::segment_nodes(4 * C, &chunks, 2).unwrap()
    };
    let mut adjacent = two();
    adjacent[0].sparse_extents = vec![
        pb::Extent {
            offset: 0,
            length: C,
        },
        pb::Extent {
            offset: C,
            length: C,
        },
    ];
    let mut skipped = four();
    skipped.remove(0);
    let mut unaligned = two();
    unaligned[0].sparse_extents = vec![pb::Extent {
        offset: 1,
        length: 2 * C - 1,
    }];
    let mut extra_id = two();
    extra_id[0].content.push(id(9));
    let rejected_nodes: [(&str, Vec<pb::Node>); 4] = [
        ("adjacent extents that were not merged", adjacent),
        ("a segment named 000001 first", skipped),
        ("an extent that starts inside a chunk", unaligned),
        ("one chunk id more than the extents hold", extra_id),
    ];
    let rejected_layouts: Vec<Value> = rejected_nodes
        .iter()
        .map(|(description, nodes)| {
            json!({
                "description": description,
                "tree": tree_hex(nodes),
                "error": image::parse_layout(nodes).unwrap_err().to_string(),
            })
        })
        .collect();

    let accepted: Vec<Value> = ["disk", "vm-101.raw", "a b", "100%", "ü", "sda"]
        .iter()
        .map(|name| {
            let key = SourceKey::image(name).unwrap();
            assert_eq!(&SourceKey::parse(key.as_str()).unwrap(), &key);
            json!({ "input": name, "key": key.as_str() })
        })
        .collect();
    let rejected_keys: serde_json::Map<String, Value> = [
        "image",
        "image/",
        "image/a/b",
        "image/.",
        "image/..",
        "image/a%2fb",
        "image/a%0Ab",
        "image/a%7Fb",
        "image/%C3%BC%FF",
    ]
    .into_iter()
    .map(|bad| {
        let error = SourceKey::parse(bad).unwrap_err().to_string();
        (bad.to_owned(), json!(error))
    })
    .collect();
    let too_long = SourceKey::image(&"a".repeat(201)).unwrap_err().to_string();

    let disk_meta = image::ImageMeta {
        read_mode: image::ReadMode::Full,
        disk_size: 20 * MIB,
    }
    .to_meta("disk");
    let claimed = source::encode(&SourceKey::image("disk").unwrap(), &disk_meta)
        .unwrap()
        .unwrap();
    assert_eq!(
        image::ImageMeta::from_meta(&disk_meta, "disk")
            .unwrap()
            .disk_size,
        20 * MIB
    );

    let disk_keys: Vec<Value> = ["disk", "a b", "ü"]
        .iter()
        .map(|name| {
            json!({
                "name": name,
                "meta_key": image::disk_meta_key(name, image::DISK_SIZE),
            })
        })
        .collect();

    json!({
        "constants": {
            "chunk_size": image::IMAGE_CHUNK_SIZE,
            "segment_chunks": image::SEGMENT_CHUNKS,
            "max_image_size": image::MAX_IMAGE_SIZE,
            "max_segments": image::MAX_SEGMENTS,
            "max_name_len": image::MAX_IMAGE_NAME_LEN,
            "layout_version": image::LAYOUT_VERSION,
        },
        "chunk_id_key": hex(&k_id),
        "layout": layout,
        "rejected_layouts": rejected_layouts,
        "source_keys": {
            "accepted": accepted,
            "rejected": rejected_keys,
            "name_of_201_bytes": too_long,
        },
        "meta": {
            "disk_keys": disk_keys,
            "keys": disk_meta.keys().collect::<Vec<_>>(),
            "values": disk_meta,
            "claimed": hex(&claimed.encode_to_vec()),
        },
    })
}

#[test]
fn image_vectors_match() {
    let committed: Value =
        serde_json::from_str(&fs::read_to_string(image_path()).unwrap()).unwrap();
    assert_eq!(image_vectors(), committed);
}

#[test]
#[ignore = "generates frozen fixtures; run once via LODE_GENERATE_FIXTURES=1 cargo nextest run -p lodestore-format --run-ignored only image_vectors_generate_v1"]
fn image_vectors_generate_v1() {
    if std::env::var_os("LODE_GENERATE_FIXTURES").is_none() {
        eprintln!("skipped: set LODE_GENERATE_FIXTURES=1 to generate fixtures");
        return;
    }
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(image_path())
        .unwrap_or_else(|e| panic!("{}: {e}; the v1 vectors are frozen", image_path().display()));
    let text = serde_json::to_string_pretty(&image_vectors()).unwrap();
    std::io::Write::write_all(&mut out, format!("{text}\n").as_bytes()).unwrap();
}

fn image_binding_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1/image-binding-vectors.json")
}

fn image_binding_vectors() -> Value {
    const MIB: u64 = 1 << 20;
    let key = SourceKey::image("disk").unwrap();
    let repo = [0x11u8; 16];
    let host: &[u8] = b"host-a";
    let names = image::BitmapNames::new(&repo, host, &key);

    let sources: Vec<Value> = [
        (
            "a full read without a bitmap",
            image::ReadMode::Full,
            None,
            Some(image::FullReason::NoBitmap),
        ),
        (
            "a full read that binds generation 2",
            image::ReadMode::Full,
            Some(names.binding(2)),
            Some(image::FullReason::NoState),
        ),
        (
            "an incremental read that binds generation 3",
            image::ReadMode::Incremental,
            Some(names.binding(3)),
            None,
        ),
    ]
    .into_iter()
    .map(|(case, read_mode, binding, reason)| {
        let meta = image::ImageMeta {
            read_mode,
            disk_size: 20 * MIB,
        }
        .to_meta_bound("disk", binding.as_ref(), reason);
        assert_eq!(
            image::BitmapBinding::from_meta(&meta, "disk").unwrap(),
            binding
        );
        assert_eq!(image::FullReason::from_meta(&meta, "disk").unwrap(), reason);
        let claimed = source::encode(&key, &meta).unwrap().unwrap();
        json!({ "case": case, "meta": meta, "claimed": hex(&claimed.encode_to_vec()) })
    })
    .collect();

    let accepted: Vec<Value> = [1, 2, 10, u64::MAX - 1]
        .into_iter()
        .map(|generation| {
            let name = names.name(generation);
            assert_eq!(names.parse(&name), Some(generation));
            json!({ "name": name, "generation": generation })
        })
        .collect();
    let prefix = names.name(3).trim_end_matches('3').to_owned();
    let other = |repo: &[u8; 16], host: &[u8], key: &SourceKey| {
        image::BitmapNames::new(repo, host, key).name(3)
    };
    let rejected: Vec<Value> = [
        format!("{prefix}0"),
        format!("{prefix}03"),
        format!("{prefix}{}", u64::MAX),
        format!("{prefix}{}", u128::from(u64::MAX) + 1),
        format!("{prefix}+3"),
        format!("{prefix}-3"),
        prefix.clone(),
        format!("{prefix}3 "),
        format!("{prefix}3").to_ascii_uppercase(),
        format!("{prefix}3").replacen("lode-", "", 1),
        other(&[0x12; 16], host, &key),
        other(&repo, b"host-b", &key),
        other(&repo, host, &SourceKey::image("other").unwrap()),
    ]
    .into_iter()
    .map(|input| {
        assert_eq!(names.parse(&input), None);
        json!({ "input": input })
    })
    .collect();

    json!({
        "meta_keys": {
            "bitmap_name": image::disk_meta_key("disk", image::DISK_BITMAP_NAME),
            "bitmap_gen": image::disk_meta_key("disk", image::DISK_BITMAP_GEN),
            "full_reason": image::disk_meta_key("disk", image::DISK_FULL_REASON),
        },
        "full_reasons": image::FullReason::ALL.map(image::FullReason::as_str),
        "sources": sources,
        "bitmap_names": {
            "repo_id": hex(&repo),
            "host": String::from_utf8_lossy(host),
            "key": key.as_str(),
            "instance_id": hex(&image::instance_id(host, &key)),
            "first": names.first(),
            "accepted": accepted,
            "rejected": rejected,
        },
    })
}

#[test]
fn image_binding_vectors_match() {
    let committed: Value =
        serde_json::from_str(&fs::read_to_string(image_binding_path()).unwrap()).unwrap();
    assert_eq!(image_binding_vectors(), committed);
}

#[test]
#[ignore = "generates frozen fixtures; run once via LODE_GENERATE_FIXTURES=1 cargo nextest run -p lodestore-format --run-ignored only image_binding_vectors_generate_v1"]
fn image_binding_vectors_generate_v1() {
    if std::env::var_os("LODE_GENERATE_FIXTURES").is_none() {
        eprintln!("skipped: set LODE_GENERATE_FIXTURES=1 to generate fixtures");
        return;
    }
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(image_binding_path())
        .unwrap_or_else(|e| {
            panic!(
                "{}: {e}; the v1 vectors are frozen",
                image_binding_path().display()
            )
        });
    let text = serde_json::to_string_pretty(&image_binding_vectors()).unwrap();
    std::io::Write::write_all(&mut out, format!("{text}\n").as_bytes()).unwrap();
}

fn ct_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1/ct-vectors.json")
}

fn ct_vectors() -> Value {
    use lodestore_format::ct::{self, CtMeta, CtMode, IdKind, IdMap, IdRange};
    use lodestore_format::fs::idmap::{Direction, OVERFLOW_ID, translate_acl, translate_caps};
    use lodestore_format::tree::encode_tree;

    const CT: &str = "c0ffee00-1234-4abc-8def-0123456789ab";
    let range = |kind, ct, host, count| IdRange {
        kind,
        ct,
        host,
        count,
    };
    // One container id passed through to the node, the rest shifted.
    let through = IdMap::new(vec![
        range(IdKind::User, 0, 100_000, 1000),
        range(IdKind::User, 1000, 1000, 1),
        range(IdKind::User, 1001, 101_001, 64_535),
        range(IdKind::Group, 0, 100_000, 1000),
        range(IdKind::Group, 1000, 1000, 1),
        range(IdKind::Group, 1001, 101_001, 64_535),
    ])
    .unwrap();
    let default = IdMap::default_unprivileged();

    let mut metas = Vec::new();
    for mode in CtMode::ALL {
        for (unprivileged, idmap) in [(false, None), (true, Some(default.clone()))] {
            let meta = CtMeta {
                mode,
                unprivileged,
                idmap,
            }
            .to_meta()
            .unwrap();
            metas.push(
                json!({ "mode": mode.as_str(), "unprivileged": unprivileged, "values": meta }),
            );
        }
    }

    let acl = |entries: &[(u16, u16, u32)]| {
        let mut out = 2u32.to_le_bytes().to_vec();
        for &(tag, perm, id) in entries {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&perm.to_le_bytes());
            out.extend_from_slice(&id.to_le_bytes());
        }
        out
    };
    let acl_in = acl(&[
        (0x01, 6, u32::MAX),
        (0x02, 6, 101_000),
        (0x02, 4, 300_000),
        (0x04, 4, u32::MAX),
        (0x08, 4, 101_001),
        (0x10, 6, u32::MAX),
        (0x20, 4, u32::MAX),
    ]);
    let mut unmapped = 0;
    let acl_out = translate_acl(&acl_in, Direction::HostToCt, &default, &mut unmapped).unwrap();
    let mut caps_in = 0x0300_0001u32.to_le_bytes().to_vec();
    caps_in.extend_from_slice(&(1u32 << 13).to_le_bytes());
    caps_in.extend_from_slice(&[0; 12]);
    caps_in.extend_from_slice(&100_000u32.to_le_bytes());
    let caps_out = translate_caps(&caps_in, Direction::HostToCt, &default, &mut 0)
        .unwrap()
        .unwrap();
    let caps_back = translate_caps(&caps_out, Direction::CtToHost, &default, &mut 0)
        .unwrap()
        .unwrap();

    let mut source_meta = CtMeta {
        mode: CtMode::Snapshot,
        unprivileged: true,
        idmap: Some(default.clone()),
    }
    .to_meta()
    .unwrap();
    source_meta.insert("name".to_owned(), "dns".to_owned());
    source_meta.insert("vmid".to_owned(), "200".to_owned());
    let key = SourceKey::proxmox_ct("pve1", CT).unwrap();
    let claimed = source::encode(&key, &source_meta).unwrap().unwrap();
    let snapshot = pb::Snapshot {
        paths: vec![ct::FS_PATH.to_vec()],
        source: Some(claimed.clone()),
        ..Default::default()
    };
    let dir = |name: &[u8], fill: u8| pb::Node {
        name: name.to_vec(),
        r#type: pb::NodeType::Dir as i32,
        mode: 0o700,
        subtrees: vec![vec![fill; 32]],
        ..Default::default()
    };
    let file = |name: &[u8], size: u64, fill: u8| pb::Node {
        name: name.to_vec(),
        r#type: pb::NodeType::File as i32,
        mode: 0o600,
        size,
        content: vec![vec![fill; 32]],
        ..Default::default()
    };
    let top = encode_tree(vec![dir(ct::CONFIG_DIR, 1), dir(ct::FS_DIR, 2)]);
    let config = encode_tree(vec![
        file(ct::CONFIG_FILE, 28, 3),
        file(ct::FIREWALL_FILE, 20, 4),
    ]);

    json!({
        "layout": {
            "version": ct::CT_LAYOUT_VERSION,
            "keys": [ct::META_CT_LAYOUT, ct::META_CT_MODE, ct::META_CT_UNPRIVILEGED, ct::META_CT_IDMAP],
            "fs_dir": String::from_utf8_lossy(ct::FS_DIR),
            "config_dir": String::from_utf8_lossy(ct::CONFIG_DIR),
            "config_file": String::from_utf8_lossy(ct::CONFIG_FILE),
            "firewall_file": String::from_utf8_lossy(ct::FIREWALL_FILE),
            "paths": [String::from_utf8_lossy(ct::FS_PATH)],
            "max_config_bytes": ct::MAX_CT_CONFIG_BYTES,
            "max_idmap_len": ct::MAX_CT_IDMAP_LEN,
            "overflow_id": OVERFLOW_ID,
            "tree": hex(&top),
            "config_tree": hex(&config),
        },
        "meta": metas,
        "idmap": {
            "default": default.canonical().unwrap(),
            "through": through.canonical().unwrap(),
            "from_config": IdMap::from_config("unprivileged: 1\n").unwrap().unwrap().canonical().unwrap(),
        },
        "translate": {
            "acl_in": hex(&acl_in),
            "acl_out": hex(&acl_out),
            "acl_unmapped": unmapped,
            "caps_in": hex(&caps_in),
            "caps_out": hex(&caps_out),
            "caps_back": hex(&caps_back),
        },
        "source": {
            "key": key.as_str(),
            "claimed": hex(&claimed.encode_to_vec()),
            "snapshot": hex(&snapshot.encode_to_vec()),
        },
    })
}

#[test]
fn ct_vectors_match() {
    let committed: Value = serde_json::from_str(&fs::read_to_string(ct_path()).unwrap()).unwrap();
    assert_eq!(ct_vectors(), committed);
}

#[test]
#[ignore = "generates frozen fixtures; run once via LODE_GENERATE_FIXTURES=1 cargo nextest run -p lodestore-format --run-ignored only ct_vectors_generate_v1"]
fn ct_vectors_generate_v1() {
    if std::env::var_os("LODE_GENERATE_FIXTURES").is_none() {
        eprintln!("skipped: set LODE_GENERATE_FIXTURES=1 to generate fixtures");
        return;
    }
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(ct_path())
        .unwrap_or_else(|e| panic!("{}: {e}; the v1 vectors are frozen", ct_path().display()));
    let text = serde_json::to_string_pretty(&ct_vectors()).unwrap();
    std::io::Write::write_all(&mut out, format!("{text}\n").as_bytes()).unwrap();
}
