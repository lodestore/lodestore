//! Which snapshot is `latest` when a host with writer keys can plant
//! snapshots with any time it likes, and which snapshots count as signed.

use crate::adv_support;
use crate::gc_support;

use std::collections::BTreeSet;
use std::ops::Range;
use std::time::{Duration, UNIX_EPOCH};

use adv_support::attacker_keys;
use bytes::Bytes;
use gc_support::Fx;
use lodestore_format::attest::{
    AttestedSet, MARKER_LEGACY_SHARDS, attest_key, ensure_upgraded, put_attestation,
    read_attestations, seal_attestation, seal_provenance,
};
use lodestore_format::crypto::{BlobId, Entropy};
use lodestore_format::error::Error;
use lodestore_format::index::hex32;
use lodestore_format::keys::{MasterKey, ReadKeys};
use lodestore_format::locks::new_lock_id;
use lodestore_format::pb;
use lodestore_format::provenance::{
    FUTURE_TOLERANCE_MS, LatestMode, LatestScope, LatestWarning, ProvenanceSet, Signer,
    owner_record, pick_latest, pick_latest_in,
};
use lodestore_format::reader::Reader;
use lodestore_format::snapshot::SnapshotEntry;
use lodestore_format::source::{self, Meta, SourceKey};
use lodestore_format::store::{BlobError, BlobStore, ListEntry, LocalFsStore, put_named};
use lodestore_format::writer::{BackupOptions, Writer};
use proptest::prelude::*;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{Rng, SeedableRng};

fn record(id: BlobId, host: &str, job: &str, verified: i64) -> pb::ProvenanceRecord {
    pb::ProvenanceRecord {
        snapshot_id: id.to_vec(),
        host_id: host.into(),
        hostname: "honest".into(),
        job_id: job.into(),
        verified_at: Some(pb::Timestamp {
            seconds: verified,
            nanos: 0,
        }),
        source: None,
    }
}

async fn sign(fx: &Fx, records: &[pb::ProvenanceRecord]) {
    let keys = fx.repo.read_keys();
    let sealed = records
        .iter()
        .map(|r| seal_provenance(keys, r, &mut Entropy::os()).unwrap())
        .collect();
    put_attestation(&fx.store, keys, &[], &[], sealed, &mut Entropy::os())
        .await
        .unwrap();
}

async fn upgrade(fx: &Fx) {
    ensure_upgraded(&fx.store, fx.repo.read_keys(), &mut Entropy::os())
        .await
        .unwrap();
}

fn reader(fx: &Fx) -> Reader<&lodestore_format::store::LocalFsStore> {
    Reader::open(&fx.store, fx.repo.read_keys().duplicate(), fx.repo.config()).unwrap()
}

/// A snapshot written with the writer keys every backing-up host holds.
async fn plant(fx: &Fx, label: &str, hostname: &str, seconds: i64) -> BlobId {
    let src = fx.make_src(label, &[("f", 1, 1000)]);
    let keys = attacker_keys(&fx.repo);
    let mut w = Writer::new(&fx.store, keys, fx.repo.config())
        .await
        .unwrap();
    let options = BackupOptions {
        hostname: Some(hostname.as_bytes().to_vec()),
        time: Some(pb::Timestamp { seconds, nanos: 0 }),
        ..Default::default()
    };
    w.backup(&[src], options, &|_| {})
        .await
        .unwrap()
        .snapshot_id
}

const YEAR_9999: i64 = 253_370_764_800;

#[tokio::test]
async fn provenance_latest_strict_ignores_forged_future_snapshot() {
    let fx = Fx::new().await;
    let honest = plant(&fx, "honest", "honest", 1_700_000_000).await;
    upgrade(&fx).await;
    sign(&fx, &[record(honest, "h1", "j1", 1_700_000_100)]).await;
    let forged = plant(&fx, "forged", "victim", YEAR_9999).await;

    let r = reader(&fx);
    let got = r.resolve_snapshot("latest").await.unwrap();
    assert_eq!(got.id, honest, "forged snapshot {forged:?} won");
    assert_eq!(
        r.resolve_latest(false).await.unwrap().mode,
        LatestMode::Strict
    );
}

const NOW_MS: i64 = 1_700_000_000_000;
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;

fn keys() -> ReadKeys {
    ReadKeys::derive(MasterKey::from_bytes([1u8; 32]), [2u8; 16])
}

fn id(n: u8) -> BlobId {
    [n; 32]
}

fn entry(n: u8, modified_ms: Option<i64>, manifest_ms: i64) -> SnapshotEntry {
    SnapshotEntry {
        id: id(n),
        snapshot: pb::Snapshot {
            time: Some(pb::Timestamp {
                seconds: manifest_ms / 1000,
                nanos: 0,
            }),
            ..Default::default()
        },
        modified_ms,
    }
}

fn sealed(keys: &ReadKeys, r: &pb::ProvenanceRecord) -> Vec<u8> {
    seal_provenance(keys, r, &mut Entropy::os()).unwrap()
}

fn attested(sealed_provenance: Vec<Vec<u8>>) -> AttestedSet {
    AttestedSet {
        sealed_provenance,
        objects: vec!["locks/attest-x".into()],
        ..Default::default()
    }
}

fn signed(records: &[pb::ProvenanceRecord]) -> ProvenanceSet {
    let k = keys();
    let sealed = records.iter().map(|r| sealed(&k, r)).collect();
    ProvenanceSet::from_attested(&attested(sealed), &k)
}

fn unsigned() -> ProvenanceSet {
    ProvenanceSet::from_attested(&AttestedSet::default(), &keys())
}

fn text(e: Error) -> String {
    match e {
        Error::InvalidInput(m) => m,
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn provenance_latest_warns_future_and_newer_unverified() {
    let set = signed(&[record(id(1), "h", "j", 10)]);
    let entries = vec![
        entry(1, Some(1000), NOW_MS - DAY_MS),
        entry(2, Some(2000), NOW_MS + DAY_MS),
        entry(3, Some(3000), NOW_MS - DAY_MS),
    ];
    let pick = pick_latest(entries, &set, NOW_MS, false).unwrap();
    assert_eq!(pick.entry.id, id(1));
    assert_eq!(pick.mode, LatestMode::Strict);
    assert!(pick.warnings.contains(&LatestWarning::NewerUnverified {
        count: 2,
        newest: id(3)
    }));
    assert!(pick.warnings.contains(&LatestWarning::FutureTime {
        count: 1,
        newest: id(2)
    }));
}

#[test]
fn provenance_latest_ties_break_by_id() {
    let set = signed(&[record(id(1), "h", "j", 10), record(id(2), "h", "j", 10)]);
    let mut entries = vec![entry(1, Some(5000), 0), entry(2, Some(5000), 0)];
    let pick = pick_latest(entries.clone(), &set, NOW_MS, false).unwrap();
    assert_eq!(pick.entry.id, id(2));
    entries.reverse();
    let pick = pick_latest(entries, &set, NOW_MS, false).unwrap();
    assert_eq!(pick.entry.id, id(2));
}

#[test]
fn provenance_latest_missing_modified_falls_back() {
    let set = signed(&[
        record(id(1), "h", "j", 500),
        record(id(2), "h", "j", 900),
        record(id(3), "h", "j", 700),
    ]);
    let entries = vec![
        entry(1, None, 9000),
        entry(2, None, 1000),
        entry(3, None, 0),
    ];
    let pick = pick_latest(entries, &set, NOW_MS, false).unwrap();
    assert_eq!(pick.entry.id, id(2), "strict orders by verified_at");

    let entries = vec![
        entry(1, None, 3000),
        entry(2, None, 7000),
        entry(3, None, 5000),
    ];
    let pick = pick_latest(entries, &unsigned(), NOW_MS, false).unwrap();
    assert_eq!(pick.entry.id, id(2), "legacy orders by the manifest");
}

#[test]
fn provenance_latest_strict_without_verified_asks_for_id() {
    let set = signed(&[record(id(9), "h", "j", 10)]);
    let err = pick_latest(vec![entry(1, Some(5), 5)], &set, NOW_MS, false).unwrap_err();
    assert!(text(err).contains("Name one by id"));

    let entries = vec![entry(1, Some(5), 5), entry(9, Some(1), 1)];
    let pick = pick_latest(entries, &set, NOW_MS, false).unwrap();
    assert_eq!(pick.entry.id, id(9));
}

#[test]
fn provenance_latest_legacy_without_attestations() {
    let pick = pick_latest(vec![entry(1, Some(5), 5)], &unsigned(), NOW_MS, false).unwrap();
    assert_eq!(pick.mode, LatestMode::Legacy);
    assert!(pick.warnings.contains(&LatestWarning::Legacy));

    let err = pick_latest(Vec::new(), &unsigned(), NOW_MS, false).unwrap_err();
    assert_eq!(text(err), "the repository has no snapshots");
}

#[tokio::test]
async fn provenance_latest_rejected_attestation_keeps_legacy() {
    let fx = Fx::new().await;
    let snap = plant(&fx, "one", "h", 1_700_000_000).await;
    let keys = fx.repo.read_keys();
    let lock = new_lock_id(&mut Entropy::os());
    let body = pb::Attestation {
        attest_id: lock.to_vec(),
        markers: vec![MARKER_LEGACY_SHARDS],
        sealed_provenance: vec![sealed(keys, &record(snap, "h", "j", 1))],
        ..Default::default()
    };
    let bytes = seal_attestation(&[9u8; 32], keys.repo_id(), &body);
    put_named(&fx.store, &attest_key(&lock), bytes)
        .await
        .unwrap();

    let set = read_attestations(&fx.store, keys).await.unwrap();
    assert_eq!((set.objects.len(), set.rejected.len()), (0, 1));
    let r = reader(&fx);
    assert!(!r.provenance().await.unwrap().contains(&snap));
    let pick = r.resolve_latest(false).await.unwrap();
    assert_eq!(pick.mode, LatestMode::Legacy);
}

#[test]
fn provenance_duplicate_records_collapse() {
    let owner = record(id(1), "", "", 5);
    let server = record(id(1), "h", "j", 50);
    let set = signed(&[owner.clone(), server.clone()]);
    assert_eq!((set.len(), set.signer(&id(1))), (1, Some(Signer::Server)));
    let set = signed(&[server, owner]);
    assert_eq!((set.len(), set.signer(&id(1))), (1, Some(Signer::Server)));

    let set = signed(&[
        record(id(2), "h", "j", 90),
        record(id(2), "h", "k", 30),
        record(id(2), "h", "m", 60),
    ]);
    assert_eq!(set.len(), 1);
    assert_eq!(set.get(&id(2)).unwrap().job_id, "k");
}

#[test]
fn provenance_owner_record_is_owner() {
    let set = signed(&[record(id(1), "", "", 5), record(id(2), "h", "j", 5)]);
    assert_eq!(set.signer(&id(1)), Some(Signer::Owner));
    assert_eq!(set.signer(&id(2)), Some(Signer::Server));
    assert_eq!(set.signer(&id(3)), None);
}

#[test]
fn provenance_seeded_mutations_never_panic() {
    let k = keys();
    let intact = sealed(&k, &record(id(1), "h", "j", 5));
    let victim = sealed(&k, &record(id(2), "h", "j", 5));
    let mut rng = ChaCha20Rng::from_seed([42u8; 32]);
    for _ in 0..256 {
        let mut bad = victim.clone();
        if rng.next_u32() % 4 == 0 {
            bad.truncate(rng.next_u32() as usize % bad.len());
        } else {
            for _ in 0..=(rng.next_u32() % 3) {
                let at = rng.next_u32() as usize % bad.len();
                bad[at] ^= u8::try_from(1 + rng.next_u32() % 255).unwrap();
            }
        }
        let set = ProvenanceSet::from_attested(&attested(vec![intact.clone(), bad]), &k);
        assert_eq!(set.len(), 1);
        assert!(set.contains(&id(1)));
        assert_eq!(set.unreadable(), 1);
    }
}

#[test]
fn provenance_latest_legacy_skips_future_storage_time() {
    let entries = vec![
        entry(1, Some(NOW_MS - HOUR_MS), 0),
        entry(2, Some(NOW_MS + DAY_MS), 0),
    ];
    let pick = pick_latest(entries, &unsigned(), NOW_MS, false).unwrap();
    assert_eq!(pick.entry.id, id(1));
    assert!(pick.warnings.contains(&LatestWarning::Legacy));
    assert!(pick.warnings.contains(&LatestWarning::FutureStored {
        count: 1,
        newest: id(2)
    }));

    let entries = vec![
        entry(1, Some(NOW_MS - HOUR_MS), 0),
        entry(2, None, NOW_MS + DAY_MS),
    ];
    let pick = pick_latest(entries, &unsigned(), NOW_MS, false).unwrap();
    assert_eq!(pick.entry.id, id(1));

    let entries = vec![
        entry(1, Some(NOW_MS + DAY_MS), 0),
        entry(2, None, NOW_MS + DAY_MS),
    ];
    let err = pick_latest(entries, &unsigned(), NOW_MS, false).unwrap_err();
    assert!(text(err).contains("stored with a time in the future"));
}

#[test]
fn provenance_latest_expected_upgrade_without_attestations_refuses() {
    let entries = vec![entry(1, Some(NOW_MS - HOUR_MS), 0)];
    let err = pick_latest(entries.clone(), &unsigned(), NOW_MS, true).unwrap_err();
    assert!(text(err).contains("may have been deleted"));
    let pick = pick_latest(entries.clone(), &unsigned(), NOW_MS, false).unwrap();
    assert_eq!(pick.mode, LatestMode::Legacy);

    let set = signed(&[record(id(1), "h", "j", 5)]);
    let pick = pick_latest(entries, &set, NOW_MS, true).unwrap();
    assert_eq!(pick.mode, LatestMode::Strict);
}

#[test]
fn provenance_latest_strict_skips_future_storage_time() {
    let set = signed(&[
        record(id(1), "h", "j", 10),
        record(id(2), "h", "j", 10),
        record(id(3), "h", "j", (NOW_MS + DAY_MS) / 1000),
    ]);
    let entries = vec![
        entry(1, Some(NOW_MS - HOUR_MS), 0),
        entry(2, Some(NOW_MS + 2 * DAY_MS), 0),
        entry(3, None, 0),
    ];
    let pick = pick_latest(entries, &set, NOW_MS, false).unwrap();
    assert_eq!((pick.entry.id, pick.mode), (id(1), LatestMode::Strict));
    assert!(pick.warnings.contains(&LatestWarning::FutureStored {
        count: 2,
        newest: id(2)
    }));

    let set = signed(&[record(id(2), "h", "j", 10)]);
    let entries = vec![entry(1, Some(5), 5), entry(2, Some(NOW_MS + DAY_MS), 0)];
    let err = pick_latest(entries, &set, NOW_MS, false).unwrap_err();
    assert!(text(err).contains("stored with a time in the future"));
}

/// Lists one attestation more than there is, as a listing taken just before
/// a cleanup cycle swept it does.
struct Vanishing<'a>(&'a LocalFsStore);

impl BlobStore for Vanishing<'_> {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<(), BlobError> {
        self.0.put_if_absent(key, data).await
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        self.0.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.0.get_range(key, range).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        let mut listed = self.0.list(prefix).await?;
        if prefix == "locks/" {
            listed.push(ListEntry {
                key: attest_key(&new_lock_id(&mut Entropy::os())),
                size: 100,
                modified_ms: None,
            });
        }
        Ok(listed)
    }
}

#[tokio::test]
async fn provenance_reader_refuses_a_partial_attestation_read() {
    let fx = Fx::new().await;
    let honest = fx.backup("one", &[("f", 1, 1000)]).await.snapshot_id;
    upgrade(&fx).await;
    sign(&fx, &[record(honest, "h", "j", 10)]).await;
    let r = Reader::open(
        Vanishing(&fx.store),
        fx.repo.read_keys().duplicate(),
        fx.repo.config(),
    )
    .unwrap();
    let err = r.provenance().await.unwrap_err();
    assert!(err.to_string().contains("changed while reading"), "{err}");
    assert!(r.resolve_latest(false).await.is_err());
}

const GUEST: &str = "proxmox-vm/pve1/0b6f1a2c-3d4e-4f50-8a9b-1c2d3e4f5a6b";

fn guest() -> SourceKey {
    SourceKey::parse(GUEST).unwrap()
}

fn from_source(mut r: pb::ProvenanceRecord, key: &SourceKey) -> pb::ProvenanceRecord {
    r.source = source::encode(key, &Meta::new()).unwrap();
    r
}

fn claiming(mut e: SnapshotEntry, key: &SourceKey) -> SnapshotEntry {
    e.snapshot.source = source::encode(key, &Meta::new()).unwrap();
    e
}

#[test]
fn latest_is_per_source_instance() {
    let k = guest();
    let set = signed(&[
        record(id(1), "h1", "j", 10),
        from_source(record(id(2), "h1", "j", 10), &k),
        from_source(record(id(3), "h1", "j", 10), &k),
    ]);
    let entries = vec![
        entry(1, Some(NOW_MS - 3 * HOUR_MS), 0),
        entry(2, Some(NOW_MS - 2 * HOUR_MS), 0),
        entry(3, Some(NOW_MS - HOUR_MS), 0),
    ];
    let scope = LatestScope::Instance(k.clone());
    let pick = pick_latest_in(entries.clone(), &set, NOW_MS, false, &scope).unwrap();
    assert_eq!(pick.entry.id, id(3));
    let pick = pick_latest(entries, &set, NOW_MS, false).unwrap();
    assert_eq!(
        pick.entry.id,
        id(1),
        "a guest image became latest for files"
    );
    assert_eq!(set.source_of(&id(2)), k);
    assert!(set.source_of(&id(1)).is_files());
}

#[test]
fn label_comes_from_the_signed_record() {
    let mut named = record(id(2), "h", "j", 10);
    let meta = Meta::from([("vmid".into(), "101".into()), ("name".into(), "web".into())]);
    named.source = source::encode(&guest(), &meta).unwrap();
    let set = signed(&[record(id(1), "h", "j", 10), named]);
    assert_eq!(set.label_of(&id(2)).as_deref(), Some("VM 101 · web"));
    assert_eq!(set.label_of(&id(1)), None);
    assert_eq!(set.label_of(&id(3)), None);
}

#[test]
fn latest_legacy_skips_claimed_other_sources() {
    let k = guest();
    let plain = entry(1, Some(NOW_MS - 2 * HOUR_MS), 0);
    let claimed = claiming(entry(2, Some(NOW_MS - HOUR_MS), 0), &k);

    let pick = pick_latest(
        vec![plain.clone(), claimed.clone()],
        &unsigned(),
        NOW_MS,
        false,
    )
    .unwrap();
    assert_eq!((pick.entry.id, pick.mode), (id(1), LatestMode::Legacy));

    let all_claimed = pick_latest(vec![claimed.clone()], &unsigned(), NOW_MS, false).unwrap_err();
    let empty = pick_latest(Vec::new(), &unsigned(), NOW_MS, false).unwrap_err();
    assert_eq!(text(all_claimed), text(empty));

    let scope = LatestScope::Instance(k);
    let err = pick_latest_in(vec![plain, claimed], &unsigned(), NOW_MS, false, &scope).unwrap_err();
    assert!(text(err).contains("No snapshot in this repository is signed"));
}

fn idw(n: u16) -> BlobId {
    let mut out = [0u8; 32];
    out[..2].copy_from_slice(&n.to_be_bytes());
    out
}

fn entry_w(n: u16, modified_ms: i64) -> SnapshotEntry {
    SnapshotEntry {
        id: idw(n),
        ..entry(0, Some(modified_ms), 0)
    }
}

#[test]
fn latest_separates_forty_instances() {
    let key = |i: u32| {
        SourceKey::proxmox_vm("pve1", &format!("{i:08x}-0000-4000-8000-000000000000")).unwrap()
    };
    let at = |n: u16| NOW_MS - 10 * DAY_MS + i64::from(n) * 1000;
    let build = |skip: Option<u32>| {
        let mut records = vec![record(idw(1), "h", "j", 10), record(idw(2), "h", "j", 10)];
        let mut entries = vec![entry_w(1, at(1)), entry_w(2, at(2))];
        for i in 0..40u32 {
            if skip == Some(i) {
                continue;
            }
            for j in 0..2 + u16::try_from(i % 2).unwrap() {
                let n = u16::try_from(10 + i * 3).unwrap() + j;
                records.push(from_source(record(idw(n), "h", "j", 10), &key(i)));
                entries.push(entry_w(n, at(n)));
            }
        }
        (signed(&records), entries)
    };
    let (set, entries) = build(None);
    let (set7, entries7) = build(Some(7));
    for i in 0..40u32 {
        let newest = 10 + i * 3 + 1 + i % 2;
        let want = idw(u16::try_from(newest).unwrap());
        let scope = LatestScope::Instance(key(i));
        let pick = pick_latest_in(entries.clone(), &set, NOW_MS, false, &scope).unwrap();
        assert_eq!(pick.entry.id, want, "instance {i}");
        if i != 7 {
            let again = pick_latest_in(entries7.clone(), &set7, NOW_MS, false, &scope).unwrap();
            assert_eq!(again.entry.id, want, "instance {i} moved with 7 gone");
        }
    }
    let pick = pick_latest(entries, &set, NOW_MS, false).unwrap();
    assert_eq!(pick.entry.id, idw(2));
}

#[test]
fn latest_skips_wal_batches() {
    let wal = SourceKey::postgres_wal(7_300_000_000_000_000_001).unwrap();
    let set = signed(&[
        record(id(1), "h", "j", 10),
        record(id(2), "h", "j", 10),
        from_source(record(id(3), "h", "j", 10), &wal),
        from_source(record(id(4), "h", "j", 10), &wal),
    ]);
    let entries = vec![
        entry(1, Some(NOW_MS - 4 * HOUR_MS), 0),
        entry(2, Some(NOW_MS - 3 * HOUR_MS), 0),
        entry(3, Some(NOW_MS - 2 * HOUR_MS), 0),
        entry(4, Some(NOW_MS - HOUR_MS), 0),
    ];
    let err = pick_latest_in(
        entries.clone(),
        &set,
        NOW_MS,
        false,
        &LatestScope::Instance(wal),
    )
    .unwrap_err();
    assert!(text(err).contains("No snapshot in this repository is signed"));
    assert_eq!(
        pick_latest(entries, &set, NOW_MS, false).unwrap().entry.id,
        id(2)
    );
}

async fn plant_claiming(fx: &Fx, label: &str, claim: Option<pb::Source>) -> BlobId {
    let src = fx.make_src(label, &[("f", 1, 1000)]);
    let keys = attacker_keys(&fx.repo);
    let mut w = Writer::new(&fx.store, keys, fx.repo.config())
        .await
        .unwrap();
    let options = BackupOptions {
        source: claim,
        ..Default::default()
    };
    w.backup(&[src], options, &|_| {})
        .await
        .unwrap()
        .snapshot_id
}

#[tokio::test]
async fn latest_ignores_the_claim_of_the_manifest() {
    let fx = Fx::new().await;
    let k = guest();
    let claimed_guest = source::encode(&k, &Meta::new()).unwrap();
    let a = plant_claiming(&fx, "a", claimed_guest).await;
    let b = plant_claiming(&fx, "b", None).await;
    upgrade(&fx).await;
    sign(
        &fx,
        &[
            record(a, "h", "j", 10),
            from_source(record(b, "h", "j", 10), &k),
        ],
    )
    .await;

    let r = reader(&fx);
    let set = r.provenance().await.unwrap();
    assert!(set.source_of(&a).is_files());
    assert_eq!(set.source_of(&b), k);
    assert_eq!(r.resolve_latest(false).await.unwrap().entry.id, a);
    let scope = LatestScope::Instance(k);
    assert_eq!(
        r.resolve_latest_in(false, &scope).await.unwrap().entry.id,
        b
    );
}

fn named(mut r: pb::ProvenanceRecord, hostname: &str) -> pb::ProvenanceRecord {
    r.hostname = hostname.into();
    r
}

fn guest_record(n: u8, host: &str, hostname: &str) -> pb::ProvenanceRecord {
    named(
        from_source(record(id(n), host, "j", 10), &guest()),
        hostname,
    )
}

/// A record whose source no build reads: an unknown type, or meta outside
/// the rules, written past `source::encode`.
fn unreadable(mut r: pb::ProvenanceRecord, unknown_type: bool) -> pb::ProvenanceRecord {
    r.source = Some(if unknown_type {
        pb::Source {
            key: "zfs/pool/x".into(),
            meta: Vec::new(),
        }
    } else {
        pb::Source {
            key: GUEST.into(),
            meta: vec![pb::SourceMeta {
                key: "BAD".into(),
                value: "x".into(),
            }],
        }
    });
    r
}

fn hours_ago(n: u8, hours: i64) -> SnapshotEntry {
    entry(n, Some(NOW_MS - hours * HOUR_MS), 0)
}

#[test]
fn latest_of_an_instance_refuses_two_hosts() {
    let scope = LatestScope::Instance(guest());
    let pick = |records: &[pb::ProvenanceRecord], entries: Vec<SnapshotEntry>| {
        pick_latest_in(entries, &signed(records), NOW_MS, false, &scope)
    };

    let records = [
        guest_record(1, "h1", "web"),
        guest_record(2, "h2", "old"),
        guest_record(3, "h2", "db"),
    ];
    let entries = vec![hours_ago(1, 3), hours_ago(2, 2), hours_ago(3, 1)];
    let msg = text(pick(&records, entries.clone()).unwrap_err());
    assert!(msg.contains("more than one host"), "{msg}");
    assert!(msg.contains(r#""web" ("h1")"#), "{msg}");
    assert!(msg.contains(r#""db" ("h2")"#), "{msg}");
    let db = format!(r#""db" ("h2") newest {} 1h ago"#, &hex32(&id(3))[..8]);
    let web = format!(r#""web" ("h1") newest {} 3h ago"#, &hex32(&id(1))[..8]);
    assert!(msg.contains(&format!("{db}, {web}")), "newest first: {msg}");
    assert!(!msg.contains(r#""old""#), "one item per host: {msg}");
    assert!(msg.contains("--host"), "{msg}");

    let hostile = "a\u{1b}[2Jb";
    let long = "n".repeat(70);
    let records = [
        guest_record(1, "h1", hostile),
        guest_record(2, "h2", &long),
        guest_record(3, "h3", r#"x", "y"#),
    ];
    let entries = vec![hours_ago(1, 3), hours_ago(2, 2), hours_ago(3, 1)];
    let msg = text(pick(&records, entries).unwrap_err());
    assert!(!msg.contains('\u{1b}'), "{msg}");
    assert!(msg.contains(r#""a\u{1b}[2Jb" ("h1")"#), "{msg}");
    assert!(
        msg.contains(&format!("\"{}\" (\"h2\")", "n".repeat(64))),
        "{msg}"
    );
    assert!(!msg.contains(&"n".repeat(65)), "{msg}");
    assert!(msg.contains(r#""x\", \"y" ("h3")"#), "{msg}");

    let records: Vec<_> = (1..=7)
        .map(|n| guest_record(n, &format!("h{n}"), &format!("host{n}")))
        .collect();
    let entries = (1..=7).map(|n| hours_ago(n, i64::from(n))).collect();
    let msg = text(pick(&records, entries).unwrap_err());
    assert!(msg.contains(r#""host5" ("h5")"#), "{msg}");
    assert!(!msg.contains(r#"("h6")"#), "{msg}");
    assert!(msg.contains("and 2 more"), "{msg}");

    let entries = (1..=5).map(|n| hours_ago(n, i64::from(n))).collect();
    let msg = text(pick(&records[..5], entries).unwrap_err());
    assert!(msg.contains(r#""host5" ("h5")"#), "{msg}");
    assert!(!msg.contains("more;"), "all five shown: {msg}");

    let other = SourceKey::proxmox_vm("pve1", "11111111-1111-4111-8111-111111111111").unwrap();
    let records = [
        guest_record(1, "h1", "web"),
        guest_record(2, "h1", "web"),
        from_source(record(id(3), "h2", "j", 10), &other),
        record(id(4), "h2", "j", 10),
    ];
    let entries = vec![
        hours_ago(1, 4),
        hours_ago(2, 3),
        hours_ago(3, 2),
        hours_ago(4, 1),
    ];
    assert_eq!(pick(&records, entries).unwrap().entry.id, id(2));
}

#[test]
fn latest_skips_a_source_this_build_cannot_read() {
    let k = guest();
    let records = [
        record(id(1), "h1", "j", 10),
        from_source(record(id(2), "h1", "j", 10), &k),
        unreadable(record(id(3), "h2", "j", 10), true),
        unreadable(record(id(4), "h2", "j", 10), false),
    ];
    let set = signed(&records);
    let entries = vec![
        hours_ago(1, 4),
        hours_ago(2, 3),
        hours_ago(3, 2),
        hours_ago(4, 1),
    ];
    let unsigned_newer = LatestWarning::NewerUnverified {
        count: 2,
        newest: id(4),
    };
    let unopened = LatestWarning::UnreadableProvenance { count: 2 };
    for (scope, want) in [
        (LatestScope::Files, id(1)),
        (LatestScope::Instance(k.clone()), id(2)),
    ] {
        let pick = pick_latest_in(entries.clone(), &set, NOW_MS, false, &scope).unwrap();
        assert_eq!(pick.entry.id, want, "{scope:?}");
        assert!(
            pick.warnings.contains(&unsigned_newer),
            "{:?}",
            pick.warnings
        );
        assert!(pick.warnings.contains(&unopened), "{:?}", pick.warnings);
    }
    assert!(!set.contains(&id(3)) && !set.contains(&id(4)));

    let only = signed(&records[2..]);
    let err = pick_latest_in(entries, &only, NOW_MS, false, &LatestScope::Files).unwrap_err();
    assert!(text(err).starts_with("No snapshot in this repository is signed"));
}

mod before_phase_20 {
    // Frozen copy of pick_latest from commit 83884fdf (main after 19.1, before phase 20); never fix it together with the code.
    use super::*;
    use lodestore_format::provenance::LatestPick;

    const NO_VERIFIED_SNAPSHOT: &str = "No snapshot in this repository is signed by the server or by the password holder. Name one by id; `lode snapshots` lists them.";
    const ATTESTATIONS_MISSING: &str = "This repository was upgraded for server signatures, but no signature can be read now; they may have been deleted. Name a snapshot by id; `lode snapshots` lists them.";
    const ALL_STORED_IN_FUTURE: &str = "Every snapshot in this repository was stored with a time in the future. Name one by id; `lode snapshots` lists them.";
    const ALL_SIGNED_STORED_IN_FUTURE: &str = "Every signed snapshot in this repository was stored with a time in the future. Name one by id; `lode snapshots` lists them.";
    const FUTURE_TOLERANCE_MS: i64 = 600_000;

    fn manifest_ms(e: &SnapshotEntry) -> i64 {
        let t = e.snapshot.time.unwrap_or_default();
        t.seconds
            .saturating_mul(1000)
            .saturating_add(i64::from(t.nanos / 1_000_000))
    }

    fn stored_key(e: &SnapshotEntry) -> (i64, BlobId) {
        (e.modified_ms.unwrap_or_else(|| manifest_ms(e)), e.id)
    }

    fn newest_of<'a>(
        entries: impl Iterator<Item = &'a SnapshotEntry>,
        key: impl Fn(&SnapshotEntry) -> (i64, BlobId),
    ) -> Option<(usize, BlobId)> {
        let mut count = 0;
        let mut best: Option<(i64, BlobId)> = None;
        for e in entries {
            count += 1;
            best = best.max(Some(key(e)));
        }
        best.map(|(_, id)| (count, id))
    }

    pub fn pick_latest_before_phase_20(
        entries: Vec<SnapshotEntry>,
        provenance: &ProvenanceSet,
        now_ms: i64,
        expect_upgraded: bool,
    ) -> Result<LatestPick, Error> {
        if entries.is_empty() {
            return Err(Error::InvalidInput(
                "the repository has no snapshots".into(),
            ));
        }
        if expect_upgraded && !provenance.attested_any() {
            return Err(Error::InvalidInput(ATTESTATIONS_MISSING.into()));
        }
        let limit = now_ms.saturating_add(FUTURE_TOLERANCE_MS);
        let mut warnings = Vec::new();

        let (mode, chosen) = if provenance.attested_any() {
            let key = |e: &SnapshotEntry| provenance.stored_order(e);
            let (kept, future): (Vec<&SnapshotEntry>, Vec<&SnapshotEntry>) = entries
                .iter()
                .filter(|e| provenance.contains(&e.id))
                .partition(|e| key(e).0 <= limit);
            if kept.is_empty() && !future.is_empty() {
                return Err(Error::InvalidInput(ALL_SIGNED_STORED_IN_FUTURE.into()));
            }
            if let Some((count, newest)) = newest_of(future.into_iter(), key) {
                warnings.push(LatestWarning::FutureStored { count, newest });
            }
            let chosen = kept
                .into_iter()
                .max_by_key(|e| key(e))
                .ok_or_else(|| Error::InvalidInput(NO_VERIFIED_SNAPSHOT.into()))?;
            let bar = key(chosen);
            let newer = entries
                .iter()
                .filter(|e| !provenance.contains(&e.id) && stored_key(e) > bar);
            if let Some((count, newest)) = newest_of(newer, stored_key) {
                warnings.push(LatestWarning::NewerUnverified { count, newest });
            }
            (LatestMode::Strict, chosen)
        } else {
            warnings.push(LatestWarning::Legacy);
            let (kept, future): (Vec<&SnapshotEntry>, Vec<&SnapshotEntry>) =
                entries.iter().partition(|e| stored_key(e).0 <= limit);
            if let Some((count, newest)) = newest_of(future.into_iter(), stored_key) {
                warnings.push(LatestWarning::FutureStored { count, newest });
            }
            let chosen = kept
                .into_iter()
                .max_by_key(|e| stored_key(e))
                .ok_or_else(|| Error::InvalidInput(ALL_STORED_IN_FUTURE.into()))?;
            (LatestMode::Legacy, chosen)
        };

        let ahead = entries.iter().filter(|e| manifest_ms(e) > limit);
        if let Some((count, newest)) = newest_of(ahead, |e| (manifest_ms(e), e.id)) {
            warnings.push(LatestWarning::FutureTime { count, newest });
        }
        if provenance.unreadable() > 0 {
            warnings.push(LatestWarning::UnreadableProvenance {
                count: provenance.unreadable(),
            });
        }
        Ok(LatestPick {
            entry: chosen.clone(),
            mode,
            warnings,
        })
    }
}

#[derive(Debug, Clone)]
struct Spec {
    stored_ms: Option<i64>,
    manifest_s: i64,
    signers: u8,
    server_s: i64,
    owner_s: i64,
}

fn offset_ms() -> impl Strategy<Value = i64> {
    prop_oneof![-3 * DAY_MS..3 * DAY_MS, 599_990..600_010i64, -1000..1000i64]
}

fn spec() -> impl Strategy<Value = Spec> {
    (
        prop_oneof![Just(None), offset_ms().prop_map(|d| Some(NOW_MS + d))],
        offset_ms().prop_map(|d| (NOW_MS + d) / 1000),
        0u8..4,
        offset_ms().prop_map(|d| (NOW_MS + d) / 1000),
        offset_ms().prop_map(|d| (NOW_MS + d) / 1000),
    )
        .prop_map(|(stored_ms, manifest_s, signers, server_s, owner_s)| Spec {
            stored_ms,
            manifest_s,
            signers,
            server_s,
            owner_s,
        })
}

fn outcome(pick: Result<lodestore_format::provenance::LatestPick, Error>) -> String {
    match pick {
        Ok(p) => format!("{:?} {:?} {:?}", p.entry.id, p.mode, p.warnings),
        Err(e) => text(e),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn files_latest_picks_like_before(
        specs in proptest::collection::vec(spec(), 0..=12),
        attested_any in any::<bool>(),
        unreadable in any::<bool>(),
        expect_upgraded in any::<bool>(),
    ) {
        let mut records = Vec::new();
        let mut entries = Vec::new();
        for (i, s) in specs.iter().enumerate() {
            let n = u16::try_from(i + 1).unwrap();
            let mut e = entry_w(n, 0);
            e.modified_ms = s.stored_ms;
            e.snapshot.time = Some(pb::Timestamp { seconds: s.manifest_s, nanos: 0 });
            entries.push(e);
            if s.signers & 1 != 0 {
                records.push(record(idw(n), "h", "j", s.server_s));
            }
            if s.signers & 2 != 0 {
                records.push(record(idw(n), "", "", s.owner_s));
            }
        }
        let set = if attested_any {
            let k = keys();
            let mut sealed_all: Vec<Vec<u8>> = records.iter().map(|r| sealed(&k, r)).collect();
            if unreadable {
                sealed_all.push(vec![7u8; 40]);
            }
            ProvenanceSet::from_attested(&attested(sealed_all), &k)
        } else {
            unsigned()
        };
        let before = before_phase_20::pick_latest_before_phase_20(
            entries.clone(), &set, NOW_MS, expect_upgraded,
        );
        let now = pick_latest(entries, &set, NOW_MS, expect_upgraded);
        prop_assert_eq!(outcome(now), outcome(before));
    }
}

fn on_host(host: &str) -> LatestScope {
    LatestScope::InstanceOnHost(guest(), host.into())
}

fn by_name(warnings: &[LatestWarning]) -> Vec<&LatestWarning> {
    warnings
        .iter()
        .filter(|w| matches!(w, LatestWarning::HostByName { .. }))
        .collect()
}

fn host_by_name(name: &str, host_id: &str, current: &str) -> LatestWarning {
    LatestWarning::HostByName {
        name: name.into(),
        host_id: host_id.into(),
        current: current.into(),
    }
}

#[test]
fn latest_on_a_host_matches_id_or_name() {
    let set = signed(&[
        guest_record(1, "h1", "web"),
        guest_record(2, "h2", "db"),
        guest_record(3, "h2", "db"),
        guest_record(4, "h3", "web"),
        record(id(5), "h4", "j", 10),
    ]);
    let entries: Vec<_> = (1..=5).map(|n| hours_ago(n, 6 - i64::from(n))).collect();
    let pick = |host: &str| pick_latest_in(entries.clone(), &set, NOW_MS, false, &on_host(host));

    let by_id = pick("h2").unwrap();
    assert_eq!(by_id.entry.id, id(3));
    assert!(by_name(&by_id.warnings).is_empty(), "{:?}", by_id.warnings);

    let named = pick("db").unwrap();
    assert_eq!(named.entry.id, id(3));
    assert_eq!(by_name(&named.warnings), [&host_by_name("db", "h2", "db")]);

    let msg = text(pick("web").unwrap_err());
    assert!(msg.contains("names more than one host"), "{msg}");
    assert!(
        msg.contains(r#"("h1")"#) && msg.contains(r#"("h3")"#),
        "{msg}"
    );
    assert!(!msg.contains(r#"("h2")"#), "{msg}");
    assert!(msg.contains("--host with the host id"), "{msg}");

    for host in ["h4", "nobody"] {
        let msg = text(pick(host).unwrap_err());
        assert!(
            msg.starts_with(&format!(
                "No signed snapshot of {GUEST} comes from host {host:?}. Hosts with signed snapshots of it: "
            )),
            "{host}: {msg}"
        );
        assert!(
            msg.contains(r#""db" ("h2") newest"#) && msg.contains(r#""web" ("h3") newest"#),
            "{host}: {msg}"
        );
    }
}

#[test]
fn latest_on_a_host_prefers_the_id_over_a_name() {
    let entries = vec![hours_ago(1, 2), hours_ago(2, 1)];
    let pick = |records: &[pb::ProvenanceRecord]| {
        pick_latest_in(
            entries.clone(),
            &signed(records),
            NOW_MS,
            false,
            &on_host("h1"),
        )
    };
    let impostor = guest_record(2, "h2", "h1");

    let real = pick(&[guest_record(1, "h1", "web"), impostor.clone()]).unwrap();
    assert_eq!(real.entry.id, id(1));
    assert!(by_name(&real.warnings).is_empty(), "{:?}", real.warnings);
    assert!(
        real.warnings.iter().any(|w| matches!(
            w,
            LatestWarning::HostIdAlsoName { host_id, hosts }
                if host_id == "h1" && hosts.contains(r#""h1" ("h2")"#)
        )),
        "{:?}",
        real.warnings
    );

    let files_only = named(record(id(1), "h1", "j", 10), "web");
    let msg = text(pick(&[files_only, impostor.clone()]).unwrap_err());
    assert!(
        msg.starts_with(r#"No signed snapshot of "#) && msg.contains(r#"from host "h1". "#),
        "{msg}"
    );
    assert!(msg.contains(r#""h1" ("h2") newest"#), "{msg}");

    let absent = pick(&[impostor]).unwrap();
    assert_eq!(absent.entry.id, id(2));
    assert_eq!(by_name(&absent.warnings), [&host_by_name("h1", "h2", "h1")]);
}

#[test]
fn latest_on_a_host_never_takes_an_id_as_a_name() {
    const VICTIM: &str = "0198a2b4-7c1d-7e2f-8a3b-4c5d6e7f8091";
    let entries = vec![hours_ago(1, 2), hours_ago(2, 1)];
    let pick = |records: &[pb::ProvenanceRecord], host: &str| {
        pick_latest_in(
            entries.clone(),
            &signed(records),
            NOW_MS,
            false,
            &on_host(host),
        )
    };
    let impostor = guest_record(2, "h2", VICTIM);

    for host in [VICTIM, &VICTIM.to_uppercase()] {
        let msg = text(pick(std::slice::from_ref(&impostor), host).unwrap_err());
        assert!(
            msg.starts_with(&format!(
                "No signed snapshot of {GUEST} comes from host {host:?} (a host id is never taken as a host name)."
            )),
            "{msg}"
        );
        assert!(
            msg.contains(&format!("{VICTIM:?} (\"h2\") newest")),
            "{msg}"
        );
    }

    let real = pick(&[guest_record(1, VICTIM, "web"), impostor.clone()], VICTIM).unwrap();
    assert_eq!(real.entry.id, id(1));
    assert!(by_name(&real.warnings).is_empty(), "{:?}", real.warnings);
    assert!(
        real.warnings.iter().any(
            |w| matches!(w, LatestWarning::HostIdAlsoName { host_id, .. } if host_id == VICTIM)
        ),
        "{:?}",
        real.warnings
    );

    let msg = text(
        pick(
            &[named(record(id(1), VICTIM, "j", 10), "web"), impostor],
            VICTIM,
        )
        .unwrap_err(),
    );
    assert!(msg.contains("comes from host"), "{msg}");
}

#[test]
fn latest_on_a_host_follows_a_renamed_host() {
    let set = signed(&[guest_record(1, "h2", "web"), guest_record(2, "h2", "db")]);
    let entries = vec![hours_ago(1, 2), hours_ago(2, 1)];
    let pick = pick_latest_in(entries, &set, NOW_MS, false, &on_host("web")).unwrap();
    assert_eq!(pick.entry.id, id(2));
    assert_eq!(by_name(&pick.warnings), [&host_by_name("web", "h2", "db")]);
}

const UUID_A: &str = "0198a2b4-7c1d-7e2f-8a3b-4c5d6e7f8091";
const UUID_B: &str = "0198a2b4-7c1d-7e2f-8a3b-4c5d6e7f8092";
const HOST_IDS: [&str; 4] = ["h1", "h2", "h3", UUID_A];
const HOST_NAMES: [&str; 6] = ["web", "db", "h1", "h2", UUID_A, UUID_B];
const TIED_MS: i64 = NOW_MS - HOUR_MS;

#[derive(Debug, Clone)]
struct Hostile {
    host: usize,
    name: usize,
    /// 0 files, 1 the guest, 2 another guest, 3 WAL, 4 unknown type, 5 bad meta.
    source: u8,
    signed: bool,
    stored_ms: Option<i64>,
    verified_s: i64,
}

impl Hostile {
    fn opens(&self) -> bool {
        self.signed && self.source < 4
    }
}

fn hostile_record() -> impl Strategy<Value = Hostile> {
    let now_s = NOW_MS / 1000;
    (
        0..HOST_IDS.len(),
        0..HOST_NAMES.len(),
        0u8..6,
        proptest::bool::weighted(0.85),
        prop_oneof![
            Just(None),
            Just(Some(TIED_MS)),
            (-3 * DAY_MS..0i64).prop_map(|d| Some(NOW_MS + d)),
            (FUTURE_TOLERANCE_MS + 1..DAY_MS).prop_map(|d| Some(NOW_MS + d)),
        ],
        now_s - 3 * 86_400..now_s + 86_400,
    )
        .prop_map(
            |(host, name, source, signed, stored_ms, verified_s)| Hostile {
                host,
                name,
                source,
                signed,
                stored_ms,
                verified_s,
            },
        )
}

fn hostile_set(specs: &[Hostile], order: &[usize]) -> (ProvenanceSet, Vec<SnapshotEntry>) {
    let other = SourceKey::proxmox_vm("pve1", "11111111-1111-4111-8111-111111111111").unwrap();
    let wal = SourceKey::postgres_wal(7_300_000_000_000_000_001).unwrap();
    let mut records = Vec::new();
    let mut entries = Vec::new();
    for &i in order {
        let s = &specs[i];
        let n = u16::try_from(i + 1).unwrap();
        entries.push(SnapshotEntry {
            modified_ms: s.stored_ms,
            ..entry_w(n, 0)
        });
        if !s.signed {
            continue;
        }
        let r = named(
            record(idw(n), HOST_IDS[s.host], "j", s.verified_s),
            HOST_NAMES[s.name],
        );
        records.push(match s.source {
            0 => r,
            1 => from_source(r, &guest()),
            2 => from_source(r, &other),
            3 => from_source(r, &wal),
            n => unreadable(r, n == 4),
        });
    }
    (signed(&records), entries)
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Pick {
        id: BlobId,
        by_name: Option<String>,
        also_named: bool,
    },
    NoSigned,
    NoneOnHost,
    AllFuture,
    TwoHosts,
    TwoNamed,
}

fn observed(pick: Result<lodestore_format::provenance::LatestPick, Error>) -> Outcome {
    match pick {
        Ok(p) => Outcome::Pick {
            id: p.entry.id,
            by_name: p.warnings.iter().find_map(|w| match w {
                LatestWarning::HostByName { host_id, .. } => Some(host_id.clone()),
                _ => None,
            }),
            also_named: p
                .warnings
                .iter()
                .any(|w| matches!(w, LatestWarning::HostIdAlsoName { .. })),
        },
        Err(e) => {
            let msg = text(e);
            if msg.contains("names more than one host") {
                Outcome::TwoNamed
            } else if msg.contains("come from more than one host") {
                Outcome::TwoHosts
            } else if msg.starts_with("Every signed snapshot") {
                Outcome::AllFuture
            } else if msg.starts_with("No snapshot in this repository is signed") {
                Outcome::NoSigned
            } else if msg.starts_with("No signed snapshot of ") {
                Outcome::NoneOnHost
            } else {
                panic!("unexpected refusal: {msg}")
            }
        }
    }
}

/// The rule written out on its own: a record whose source does not open
/// leaves its snapshot unsigned; `--host` is an id when it has the form of a
/// UUID or any signed record carries it, else the name of exactly one host;
/// nothing left on the host is a refusal; the newest by (store time, id)
/// within the tolerance wins.
fn model(specs: &[Hostile], scope: &LatestScope) -> Outcome {
    let want_source = if matches!(scope, LatestScope::Files) {
        0
    } else {
        1
    };
    let mut admitted: Vec<usize> = (0..specs.len())
        .filter(|&i| specs[i].opens() && specs[i].source == want_source)
        .collect();
    let mut by_name = None;
    let mut also_named = false;
    match scope {
        LatestScope::Files => {}
        LatestScope::Instance(_) => {
            let hosts: BTreeSet<usize> = admitted.iter().map(|&i| specs[i].host).collect();
            if hosts.len() > 1 {
                return Outcome::TwoHosts;
            }
        }
        LatestScope::InstanceOnHost(_, host) => {
            let named: BTreeSet<usize> = admitted
                .iter()
                .filter(|&&i| HOST_NAMES[specs[i].name] == host && HOST_IDS[specs[i].host] != host)
                .map(|&i| specs[i].host)
                .collect();
            let uuid = host.len() == 36 && host.contains('-');
            let chosen = if uuid || specs.iter().any(|s| s.opens() && HOST_IDS[s.host] == host) {
                also_named = !named.is_empty();
                HOST_IDS.iter().position(|h| h == host)
            } else {
                if named.len() > 1 {
                    return Outcome::TwoNamed;
                }
                let one = named.first().copied();
                by_name = one.map(|h| HOST_IDS[h].to_owned());
                one
            };
            admitted.retain(|&i| Some(specs[i].host) == chosen);
            if admitted.is_empty() {
                return Outcome::NoneOnHost;
            }
        }
    }
    let order = |&i: &usize| {
        let s = &specs[i];
        let n = u16::try_from(i + 1).unwrap();
        (s.stored_ms.unwrap_or(s.verified_s * 1000), idw(n))
    };
    let limit = NOW_MS + FUTURE_TOLERANCE_MS;
    match admitted.iter().map(order).filter(|o| o.0 <= limit).max() {
        Some((_, id)) => Outcome::Pick {
            id,
            by_name,
            also_named,
        },
        None if admitted.is_empty() => Outcome::NoSigned,
        None => Outcome::AllFuture,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn latest_of_an_instance_on_hostile_records(
        specs in proptest::collection::vec(hostile_record(), 1..=10),
        seed in any::<[u8; 32]>(),
    ) {
        let forward: Vec<usize> = (0..specs.len()).collect();
        let reversed: Vec<usize> = forward.iter().rev().copied().collect();
        let mut shuffled = forward.clone();
        let mut rng = ChaCha20Rng::from_seed(seed);
        for i in (1..shuffled.len()).rev() {
            shuffled.swap(i, rng.next_u32() as usize % (i + 1));
        }
        let pool: BTreeSet<&str> = HOST_IDS.iter().chain(HOST_NAMES.iter()).copied().collect();
        let mut scopes = vec![LatestScope::Files, LatestScope::Instance(guest())];
        scopes.extend(pool.iter().map(|s| on_host(s)));
        let signed_hosts: BTreeSet<&str> = specs
            .iter()
            .filter(|s| s.opens())
            .map(|s| HOST_IDS[s.host])
            .collect();

        for scope in &scopes {
            let want = model(&specs, scope);
            for order in [&forward, &reversed, &shuffled] {
                let (set, entries) = hostile_set(&specs, order);
                let got = observed(pick_latest_in(entries, &set, NOW_MS, false, scope));
                prop_assert_eq!(&got, &want, "{:?} in order {:?}", scope, order);
                if let Outcome::Pick { id, .. } = got {
                    let i = usize::from(u16::from_be_bytes([id[0], id[1]])) - 1;
                    prop_assert!(specs[i].opens(), "a record that does not open was chosen");
                    if let LatestScope::InstanceOnHost(_, host) = scope
                        && signed_hosts.contains(host.as_str())
                    {
                        prop_assert_eq!(HOST_IDS[specs[i].host], host.as_str());
                    }
                }
            }
        }
    }
}

#[test]
fn provenance_owner_record_signs_as_the_owner() {
    let at = UNIX_EPOCH + Duration::new(5, 7);
    assert_eq!(
        owner_record(&id(1), b"h", None, at),
        pb::ProvenanceRecord {
            snapshot_id: id(1).to_vec(),
            host_id: String::new(),
            hostname: "h".into(),
            job_id: String::new(),
            verified_at: Some(pb::Timestamp {
                seconds: 5,
                nanos: 7
            }),
            source: None,
        }
    );
}
