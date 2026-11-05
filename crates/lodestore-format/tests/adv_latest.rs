//! A host with writer credentials plants a snapshot that claims to be the
//! newest; `latest` must not follow the manifest it wrote.

mod adv_support;
mod gc_support;

use std::time::{Duration, SystemTime};

use adv_support::attacker_keys;
use gc_support::Fx;
use lodestore_format::crypto::BlobId;
use lodestore_format::pb;
use lodestore_format::reader::Reader;
use lodestore_format::snapshot::snapshot_object_key;
use lodestore_format::writer::{BackupOptions, Writer};

const YEAR_9999: i64 = 253_370_764_800;

async fn plant_forged(fx: &Fx) -> BlobId {
    let src = fx.make_src("forged", &[("f", 9, 1000)]);
    let mut w = Writer::new(&fx.store, attacker_keys(&fx.repo), fx.repo.config())
        .await
        .unwrap();
    let options = BackupOptions {
        hostname: Some(b"victim".to_vec()),
        time: Some(pb::Timestamp {
            seconds: YEAR_9999,
            nanos: 0,
        }),
        ..Default::default()
    };
    w.backup(&[src], options, &|_| {})
        .await
        .unwrap()
        .snapshot_id
}

fn set_mtime(fx: &Fx, id: &BlobId, mtime: SystemTime) {
    let path = fx.tmp.path().join("repo").join(snapshot_object_key(id));
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
}

async fn latest(fx: &Fx) -> BlobId {
    Reader::open(&fx.store, fx.repo.read_keys().duplicate(), fx.repo.config())
        .unwrap()
        .resolve_snapshot("latest")
        .await
        .unwrap()
        .id
}

#[tokio::test]
async fn adv_legacy_latest_uses_storage_time_not_manifest_time() {
    let fx = Fx::new().await;
    let forged = plant_forged(&fx).await;
    set_mtime(&fx, &forged, SystemTime::now() - Duration::from_secs(3600));
    let honest = fx.backup("honest", &[("f", 1, 1000)]).await.snapshot_id;

    assert_eq!(
        latest(&fx).await,
        honest,
        "latest followed the time in the planted manifest ({forged:?})"
    );
}

#[tokio::test]
async fn adv_legacy_latest_ignores_future_storage_time() {
    let fx = Fx::new().await;
    let honest = fx.backup("honest", &[("f", 1, 1000)]).await.snapshot_id;
    let forged = plant_forged(&fx).await;
    set_mtime(
        &fx,
        &forged,
        SystemTime::now() + Duration::from_secs(86_400),
    );

    assert_eq!(
        latest(&fx).await,
        honest,
        "latest followed a snapshot stored in the future ({forged:?})"
    );
}
