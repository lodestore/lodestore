//! Which snapshot is `latest` when a host with writer keys can plant
//! snapshots with any time it likes, and which snapshots count as signed.

mod adv_support;
mod gc_support;

use std::ops::Range;

use adv_support::attacker_keys;
use bytes::Bytes;
use gc_support::Fx;
use lodestore_format::attest::{
    AttestedSet, MARKER_LEGACY_SHARDS, attest_key, ensure_upgraded, put_attestation,
    read_attestations, seal_attestation, seal_provenance,
};
use lodestore_format::crypto::{BlobId, Entropy};
use lodestore_format::error::Error;
use lodestore_format::keys::{MasterKey, ReadKeys};
use lodestore_format::locks::new_lock_id;
use lodestore_format::pb;
use lodestore_format::provenance::{LatestMode, LatestWarning, ProvenanceSet, Signer, pick_latest};
use lodestore_format::reader::Reader;
use lodestore_format::snapshot::SnapshotEntry;
use lodestore_format::store::{BlobError, BlobStore, ListEntry, LocalFsStore, put_named};
use lodestore_format::writer::{BackupOptions, Writer};
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
