//! A host with writer credentials plants a shard before the server first
//! opens the repository, so the server's upgrade signs it. A full collection
//! by the server must still keep every snapshot of every host restorable and
//! finish. Only the API of the first release is used, so the scenario can run
//! against that release unchanged.

#[path = "../../lodestore-format/tests/adv_support/mod.rs"]
mod adv_support;
mod common;
mod gc_support;

use std::collections::BTreeMap;
use std::path::Path;

use adv_support::{attacker_keys, forge_offsets, open_all_shards, plant_shard_where};
use common::{Backend, TestServer, backends};
use gc_support::{
    LOW_KDF, PASSWORD, after_pause, age_packs, assert_stops_then_finishes, backup, enable, forget,
    fresh_repo, kept_and_forgotten, noise, restores, steps,
};
use lodestore_format::config::config_hash;
use lodestore_format::crypto::Entropy;
use lodestore_format::index::hex32;
use lodestore_format::pb;
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::snapshot::{seal_snapshot, snapshot_object_key};
use lodestore_format::store::{BlobStore, LocalFsStore};
use lodestore_server::maintenance::gc::GcState;
use lodestore_server::maintenance::gc::cycle::StepOutcome;
use lodestore_server::repos::db::{NewRepository, RepoLocation, create_repository};
use serde_json::{Value, json};
use sqlx::Row;

/// The entries of `record` claimed to lie in the pack of `to`, one byte off.
fn retarget(record: &pb::PackRecord, to: &pb::PackRecord) -> Option<pb::PackRecord> {
    if record.pack_size > to.pack_size {
        return None;
    }
    let mut forged = forge_offsets(record);
    forged.pack_id = to.pack_id.clone();
    forged.pack_size = to.pack_size;
    forged.hpke_enc = to.hpke_enc.clone();
    forged.sealed_pack_key = to.sealed_pack_key.clone();
    Some(forged)
}

/// Every record moved into the largest real pack, listed before the same
/// record one byte off its own frame: on a collector that believes the shard,
/// all live bytes sit in that one pack and every other pack looks dead.
fn forge(records: &[pb::PackRecord]) -> Vec<pb::PackRecord> {
    let biggest = records.iter().max_by_key(|r| r.pack_size).unwrap();
    let mut forged: Vec<_> = records
        .iter()
        .filter(|r| r.pack_id != biggest.pack_id)
        .filter_map(|r| retarget(r, biggest))
        .collect();
    forged.extend(records.iter().map(forge_offsets));
    forged
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_upgrade_signed_forged_shard_server_gc_keeps_every_snapshot() {
    let backend: Backend = backends().remove(0);
    let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
    let api = ts.api().await;
    let root = ts.local_repo_root("main");
    let store = LocalFsStore::new(root.clone());
    let mut entropy = Entropy::os();
    let pending = PendingInit::generate(
        &mut entropy,
        InitOptions {
            kdf: LOW_KDF,
            ..InitOptions::default()
        },
    )
    .unwrap();
    let keys = pending.read_keys();
    let master = keys.master().clone();
    let format_repo_id = *keys.repo_id();
    let hash = config_hash(pending.config_bytes());
    pending
        .commit(&store, PASSWORD, &mut entropy)
        .await
        .unwrap();

    let a_first = backup(
        &ts,
        &store,
        "host-a",
        "a1",
        &[
            ("a", 1, 300 << 10),
            ("shared", 3, 100 << 10),
            ("a2", 7, 100 << 10),
        ],
    )
    .await;
    let a_second = backup(
        &ts,
        &store,
        "host-a",
        "a2",
        &[("shared", 3, 100 << 10), ("a2", 7, 100 << 10)],
    )
    .await;
    let b = backup(
        &ts,
        &store,
        "host-b",
        "b",
        &[
            ("b", 2, 200 << 10),
            ("shared", 3, 100 << 10),
            ("a2", 7, 100 << 10),
        ],
    )
    .await;
    let c = backup(&ts, &store, "host-c", "c", &[("c", 5, 150 << 10)]).await;
    forget(&root, &a_first);
    forget(&root, &c);
    let remaining = [a_second, b];

    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let attacker = attacker_keys(&repo);
    let shards = open_all_shards(&store, &attacker).await;
    let records: Vec<_> = shards
        .iter()
        .flat_map(|(_, shard)| shard.packs.iter().cloned())
        .collect();
    let supersedes = shards.iter().map(|(id, _)| *id).collect();
    let lowest = shards.iter().map(|(id, _)| *id).min().unwrap();
    plant_shard_where(&store, &attacker, forge(&records), supersedes, |id| {
        *id < lowest
    })
    .await;

    let id = create_repository(
        ts.state(),
        NewRepository {
            name: "main".to_string(),
            location: RepoLocation {
                backend: "local".to_string(),
                options: BTreeMap::from([("path".to_string(), root.display().to_string())]),
            },
            experimental: false,
            format_repo_id,
            config_hash: hash,
            master,
            server_slot_secret: None,
            server_creds: None,
            agent_creds: None,
        },
    )
    .await
    .unwrap();
    age_packs(&root);

    enable(&ts, &api, &id).await;
    let response = api
        .post(&format!("/api/v1/repositories/{id}/gc/run"), &json!({}))
        .await;
    assert_eq!(response.status(), 202, "{:?}", response.text().await);
    let cycle: String = response.json::<Value>().await.unwrap()["cycle_id"]
        .as_str()
        .unwrap()
        .to_string();
    let later = after_pause(&ts, &cycle).await;
    let outcome = steps(&ts, &cycle, later).await;

    let listed = store.list("snapshots/").await.unwrap();
    assert_eq!(listed.len(), remaining.len());
    for snapshot in &remaining {
        if let Err(e) = restores(&store, snapshot).await {
            panic!(
                "snapshot {} no longer restores after the cycle ({outcome:?}): {e}",
                snapshot.id
            );
        }
    }

    assert_eq!(outcome, StepOutcome::Finished(GcState::Done));
    let report = api
        .get(&format!(
            "/api/v1/repositories/{id}/gc/cycles/{cycle}/report"
        ))
        .await;
    assert_eq!(report.status(), 200);
    let report: Value = report.json().await.unwrap();
    assert!(report["bytes_freed"].as_u64().unwrap() > 0, "{report}");

    let signed = sqlx::query("SELECT attest_upgraded_at FROM repositories WHERE id = $1")
        .bind(&id)
        .fetch_one(&ts.state().db.pool)
        .await
        .and_then(|row| row.try_get::<Option<i64>, _>("attest_upgraded_at"));
    match signed {
        Ok(Some(_)) => {}
        Ok(None) => panic!(
            "precondition failed: the GC cycle did not upgrade the repository, so the planted \
             shard was not signed by the upgrade"
        ),
        Err(e) => panic!(
            "precondition failed: the GC cycle did not upgrade the repository, so the planted \
             shard was not signed by the upgrade: {e}"
        ),
    }
    ts.stop().await;
}

/// A snapshot object the collector cannot open, named by its own hash so the
/// name is not what gives it away.
fn plant_unopenable(root: &Path) -> String {
    let bytes = noise(99, 600);
    let key = format!("snapshots/{}", hex32(&config_hash(&bytes)));
    std::fs::write(root.join(&key), bytes).unwrap();
    key
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_gc_quarantines_unopenable_snapshot_until_excluded() {
    let backend: Backend = backends().remove(0);
    let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
    let api = ts.api().await;
    let (repo, store, root) = fresh_repo(&ts).await;
    let kept = kept_and_forgotten(&ts, &store, &root).await;
    let planted = plant_unopenable(&root);
    enable(&ts, &api, &repo).await;
    assert_stops_then_finishes(&ts, &api, &repo, &store, &kept, &planted).await;
    ts.stop().await;
}

/// A snapshot sealed under the repository's own keys whose root tree is in no
/// index shard.
async fn plant_missing_tree(store: &LocalFsStore, root: &Path) -> String {
    let repo = Repository::open(store, PASSWORD).await.unwrap();
    let snapshot = pb::Snapshot {
        time: Some(pb::Timestamp {
            seconds: 1_700_000_000,
            nanos: 0,
        }),
        hostname: b"host-x".to_vec(),
        paths: vec![b"/".to_vec()],
        root_tree: noise(77, 32),
        ..Default::default()
    };
    let (id, bytes) = seal_snapshot(
        &snapshot,
        repo.read_keys().hpke_public_key(),
        repo.read_keys().repo_id(),
        &mut Entropy::os(),
    )
    .unwrap();
    let key = snapshot_object_key(&id);
    std::fs::write(root.join(&key), bytes).unwrap();
    key
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_gc_quarantines_snapshot_with_missing_tree() {
    let backend: Backend = backends().remove(0);
    let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
    let api = ts.api().await;
    let (repo, store, root) = fresh_repo(&ts).await;
    let kept = kept_and_forgotten(&ts, &store, &root).await;
    let planted = plant_missing_tree(&store, &root).await;
    enable(&ts, &api, &repo).await;
    assert_stops_then_finishes(&ts, &api, &repo, &store, &kept, &planted).await;
    ts.stop().await;
}
