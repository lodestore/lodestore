//! Which snapshot is `latest` when a host with writer keys can plant
//! snapshots with any time it likes, and which snapshots count as signed.

mod adv_support;
mod gc_support;

use adv_support::attacker_keys;
use gc_support::Fx;
use lodestore_format::attest::{ensure_upgraded, put_attestation, seal_provenance};
use lodestore_format::crypto::{BlobId, Entropy};
use lodestore_format::pb;
use lodestore_format::provenance::LatestMode;
use lodestore_format::reader::Reader;
use lodestore_format::writer::{BackupOptions, Writer};

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
