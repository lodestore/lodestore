//! A host with writer credentials plants a shard before the server first
//! opens the repository, so the server's upgrade signs it. A full collection
//! by the server must still keep every snapshot of every host restorable and
//! finish. Only the API of the first release is used, so the scenario can run
//! against that release unchanged.

#[path = "../../lodestore-format/tests/adv_support/mod.rs"]
mod adv_support;
mod common;

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use adv_support::{attacker_keys, forge_offsets, open_all_shards, plant_shard_where};
use common::{Api, Backend, TestServer, WAIT, backends, wait_until};
use lodestore_format::config::config_hash;
use lodestore_format::crypto::Entropy;
use lodestore_format::index::{hex32, parse_hex32};
use lodestore_format::keys::KdfCost;
use lodestore_format::pb;
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::snapshot::{seal_snapshot, snapshot_object_key};
use lodestore_format::store::{BlobStore, LocalFsStore};
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_server::maintenance::gc::GcState;
use lodestore_server::maintenance::gc::cycle::{StepOutcome, step};
use lodestore_server::repos::db::{NewRepository, RepoLocation, create_repository};
use serde_json::{Value, json};
use sqlx::Row;

const PASSWORD: &[u8] = b"test-password";
const LOW_KDF: KdfCost = KdfCost {
    memory_kib: 64,
    iterations: 1,
    parallelism: 1,
};

fn noise(seed: u64, size: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..size)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

struct Snapshot {
    id: String,
    dir: PathBuf,
}

async fn backup(
    ts: &TestServer,
    store: &LocalFsStore,
    host: &str,
    tag: &str,
    files: &[(&str, u64, usize)],
) -> Snapshot {
    let repo = Repository::open(store, PASSWORD).await.unwrap();
    let mut writer = Writer::new(store, attacker_keys(&repo), repo.config())
        .await
        .unwrap();
    let dir = ts.dir.path().join(format!("data-{tag}"));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, seed, size) in files {
        std::fs::write(dir.join(name), noise(*seed, *size)).unwrap();
    }
    let outcome = writer
        .backup(
            std::slice::from_ref(&dir),
            BackupOptions {
                hostname: Some(host.as_bytes().to_vec()),
                ..BackupOptions::default()
            },
            &|_| {},
        )
        .await
        .expect("backup");
    Snapshot {
        id: hex32(&outcome.snapshot_id),
        dir,
    }
}

fn forget(root: &Path, snapshot: &Snapshot) {
    let id = parse_hex32(&snapshot.id).unwrap();
    std::fs::remove_file(root.join(snapshot_object_key(&id))).unwrap();
}

fn age_packs(root: &Path) {
    let old = SystemTime::now() - Duration::from_secs(3 * 86_400);
    let mut stack = vec![root.join("packs")];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_modified(old)
                    .unwrap();
            }
        }
    }
}

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

fn enabled_input() -> Value {
    json!({
        "enabled": true,
        "cron": "0 3 * * 0",
        "timezone": "UTC",
        "pause": "24h",
        "repack_below_pct": 50,
        "max_repack": "10GiB",
    })
}

async fn enable(ts: &TestServer, api: &Api, repo: &str) {
    let response = reqwest::Client::new()
        .put(format!(
            "{}/api/v1/repositories/{repo}/gc/settings",
            ts.rest_url()
        ))
        .header("cookie", api.cookie())
        .header("x-csrf-token", &api.csrf)
        .json(&enabled_input())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{:?}", response.text().await);
}

async fn cycle_state(ts: &TestServer, id: &str) -> (String, Option<i64>, Option<String>) {
    let row = sqlx::query("SELECT state, delete_after, error FROM gc_cycles WHERE id = $1")
        .bind(id)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap();
    (row.get("state"), row.get("delete_after"), row.get("error"))
}

async fn after_pause(ts: &TestServer, id: &str) -> i64 {
    let reached = wait_until(WAIT, || async { cycle_state(ts, id).await.0 == "waiting" }).await;
    let (state, delete_after, error) = cycle_state(ts, id).await;
    assert!(
        reached,
        "the cycle never reached waiting: it is {state} ({error:?})"
    );
    assert!(
        wait_until(WAIT, || async { ts.state().gc.claim(id).is_some() }).await,
        "the cycle driver never stopped"
    );
    delete_after.unwrap() + 1_000
}

async fn steps(ts: &TestServer, id: &str, now_ms: i64) -> StepOutcome {
    let _no_driver = loop {
        match ts.state().gc.claim(id) {
            Some(guard) => break guard,
            None => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    };
    loop {
        match step(ts.state(), id, now_ms).await.unwrap() {
            StepOutcome::Advanced(_) => {}
            other => return other,
        }
    }
}

async fn restores(store: &LocalFsStore, snapshot: &Snapshot) -> Result<(), String> {
    let repo = Repository::open(store, PASSWORD)
        .await
        .map_err(|e| e.to_string())?;
    let mut reader = Reader::open(store, repo.read_keys().duplicate(), repo.config())
        .map_err(|e| e.to_string())?;
    let target = tempfile::tempdir().unwrap();
    let report = reader
        .restore(
            &parse_hex32(&snapshot.id).unwrap(),
            target.path(),
            &RestoreOptions::default(),
            &|_| {},
        )
        .await
        .map_err(|e| e.to_string())?;
    if !report.errors.is_empty() {
        return Err(format!("{:?}", report.errors));
    }
    let restored = target.path().join(
        snapshot
            .dir
            .canonicalize()
            .unwrap()
            .strip_prefix("/")
            .unwrap(),
    );
    for entry in std::fs::read_dir(&snapshot.dir).unwrap() {
        let name = entry.unwrap().file_name();
        let got = std::fs::read(restored.join(&name)).map_err(|e| format!("{name:?}: {e}"))?;
        if got != std::fs::read(snapshot.dir.join(&name)).unwrap() {
            return Err(format!("{name:?} differs"));
        }
    }
    Ok(())
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

async fn fresh_repo(ts: &TestServer) -> (String, LocalFsStore, PathBuf) {
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
    (id, store, root)
}

async fn start_cycle(api: &Api, repo: &str) -> String {
    let response = api
        .post(&format!("/api/v1/repositories/{repo}/gc/run"), &json!({}))
        .await;
    assert_eq!(response.status(), 202, "{:?}", response.text().await);
    response.json::<Value>().await.unwrap()["cycle_id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn pack_listing(store: &LocalFsStore) -> Vec<String> {
    let mut keys: Vec<String> = store
        .list("packs/")
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.key)
        .collect();
    keys.sort();
    keys
}

/// A snapshot object the collector cannot open, named by its own hash so the
/// name is not what gives it away.
fn plant_unopenable(root: &Path) -> String {
    let bytes = noise(99, 600);
    let key = format!("snapshots/{}", hex32(&config_hash(&bytes)));
    std::fs::write(root.join(&key), bytes).unwrap();
    key
}

async fn quarantine_of(api: &Api, repo: &str) -> Vec<Value> {
    let status: Value = api
        .get(&format!("/api/v1/repositories/{repo}/gc"))
        .await
        .json()
        .await
        .unwrap();
    status["quarantine"].as_array().unwrap().clone()
}

/// A repository with one kept snapshot, one forgotten (its packs are
/// garbage) and an aged pack folder.
async fn kept_and_forgotten(ts: &TestServer, store: &LocalFsStore, root: &Path) -> Snapshot {
    let kept = backup(ts, store, "host-a", "keep", &[("a", 1, 200 << 10)]).await;
    let gone = backup(ts, store, "host-b", "gone", &[("b", 2, 200 << 10)]).await;
    forget(root, &gone);
    age_packs(root);
    kept
}

async fn assert_stops_then_finishes(
    ts: &TestServer,
    api: &Api,
    repo: &str,
    store: &LocalFsStore,
    kept: &Snapshot,
    planted: &str,
) {
    let packs = pack_listing(store).await;
    let first = start_cycle(api, repo).await;
    assert!(
        wait_until(WAIT, || async {
            cycle_state(ts, &first).await.0 == "failed"
        })
        .await,
        "the cycle did not fail: {:?}",
        cycle_state(ts, &first).await
    );
    assert_eq!(pack_listing(store).await, packs, "a pack was deleted");
    let quarantine = quarantine_of(api, repo).await;
    assert_eq!(quarantine.len(), 1, "{quarantine:?}");
    assert_eq!(quarantine[0]["key"], planted);

    let hex = planted.strip_prefix("snapshots/").unwrap();
    let response = api
        .post(
            &format!("/api/v1/repositories/{repo}/gc/quarantine/{hex}/exclude"),
            &json!({}),
        )
        .await;
    assert_eq!(response.status(), 200, "{:?}", response.text().await);
    assert!(
        store
            .list("snapshots/")
            .await
            .unwrap()
            .iter()
            .any(|e| e.key == planted),
        "the planted object must stay in the bucket"
    );

    let second = start_cycle(api, repo).await;
    let later = after_pause(ts, &second).await;
    assert_eq!(
        steps(ts, &second, later).await,
        StepOutcome::Finished(GcState::Done)
    );
    assert!(pack_listing(store).await.len() < packs.len());
    restores(store, kept)
        .await
        .expect("the kept snapshot restores");
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
