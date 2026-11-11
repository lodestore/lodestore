mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use common::{TestServer, backends, seed_agent, seed_backup_job};
use lodestore_format::crypto::Entropy;
use lodestore_format::index::{hex32, parse_hex32};
use lodestore_format::keys::{KdfCost, MasterKey};
use lodestore_format::locks::read_gc_locks;
use lodestore_format::pack::pack_object_key;
use lodestore_format::reader::{Reader, RestoreOptions};
use lodestore_format::repo::{InitOptions, PendingInit, Repository};
use lodestore_format::snapshot::snapshot_object_key;
use lodestore_format::store::policy::agent_minio_policy_json;
use lodestore_format::store::{
    BlobDelete, BlobError, BlobStore, RepoLocation, StaticCredentials, Store,
};
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_server::catalog::sync_repository;
use lodestore_server::maintenance::gc::cycle::{StepOutcome, step};
use lodestore_server::maintenance::gc::preflight::run_preflight;
use lodestore_server::maintenance::gc::{
    GcOrigin, GcSettings, GcState, db, run_dry_run_at, start_cycle,
};
use lodestore_server::repos::db::{AccessKey, NewRepository, RepoLocation as DbLocation};
use lodestore_testkit::minio::MinioServer;
use opendal::{Operator, services};
use serde_json::Value;

const PASSWORD: &[u8] = b"test-password";
const BUCKET: &str = "repo";
const DAY_MS: i64 = 86_400_000;
const HOUR_MS: i64 = 3_600_000;
const MINUTE_MS: i64 = 60_000;
const SHARED_SIZE: usize = 64 * 1024;
const UNIQUE_SIZE: usize = 600 * 1024;
const PREFIXES: [&str; 3] = ["packs/", "index/", "snapshots/"];

#[derive(Clone, Copy)]
enum Bucket {
    Versioned,
    Compliance,
    Governance,
}

struct Cloud {
    minio: MinioServer,
    agent_key: (String, String),
    server_key: (String, String),
    server_store: Store,
    agent_store: Store,
    options: BTreeMap<String, String>,
    format_repo_id: [u8; 16],
    config_hash: [u8; 32],
    master: MasterKey,
}

fn open(options: &BTreeMap<String, String>, key: &(String, String)) -> Store {
    RepoLocation {
        backend: "s3".to_string(),
        options: options.clone(),
    }
    .open(Some(StaticCredentials {
        access_key_id: key.0.clone(),
        secret_access_key: key.1.clone(),
    }))
    .unwrap()
}

impl Cloud {
    async fn start(kind: Bucket) -> Cloud {
        let minio = MinioServer::start().await;
        match kind {
            Bucket::Versioned => minio.create_versioned_bucket(BUCKET).await,
            Bucket::Compliance => minio.create_locked_bucket(BUCKET).await,
            Bucket::Governance => minio.create_governance_bucket(BUCKET).await,
        }
        let agent_key = minio
            .create_user_with_policy(BUCKET, &agent_minio_policy_json(BUCKET, ""))
            .await;
        let server_key = minio.root_credentials();
        let options = BTreeMap::from([
            ("bucket".to_string(), BUCKET.to_string()),
            ("endpoint".to_string(), minio.endpoint().to_string()),
            ("path_style".to_string(), "true".to_string()),
        ]);
        let server_store = open(&options, &server_key);
        let agent_store = open(&options, &agent_key);
        let mut entropy = Entropy::os();
        let pending = PendingInit::generate(
            &mut entropy,
            InitOptions {
                kdf: KdfCost {
                    memory_kib: 64,
                    iterations: 1,
                    parallelism: 1,
                },
                ..InitOptions::default()
            },
        )
        .unwrap();
        let keys = pending.read_keys();
        let master = keys.master().clone();
        let format_repo_id = *keys.repo_id();
        let config_hash = lodestore_format::config::config_hash(pending.config_bytes());
        pending
            .commit(&server_store, PASSWORD, &mut entropy)
            .await
            .unwrap();
        Cloud {
            minio,
            agent_key,
            server_key,
            server_store,
            agent_store,
            options,
            format_repo_id,
            config_hash,
            master,
        }
    }

    async fn register(&self, ts: &TestServer) -> String {
        let repo = lodestore_server::repos::db::create_repository(
            ts.state(),
            NewRepository {
                name: "cloud".to_string(),
                location: DbLocation {
                    backend: "s3".to_string(),
                    options: self.options.clone(),
                },
                experimental: false,
                format_repo_id: self.format_repo_id,
                config_hash: self.config_hash,
                master: self.master.clone(),
                server_slot_secret: None,
                server_creds: Some(AccessKey {
                    access_key_id: self.server_key.0.clone(),
                    secret_access_key: self.server_key.1.clone(),
                }),
                agent_creds: Some(AccessKey {
                    access_key_id: self.agent_key.0.clone(),
                    secret_access_key: self.agent_key.1.clone(),
                }),
            },
        )
        .await
        .unwrap();
        sync_repository(ts.state(), &repo).await.unwrap();
        repo
    }

    async fn open_writer(&self) -> Writer<&Store> {
        let repo = Repository::open(&self.server_store, PASSWORD)
            .await
            .unwrap();
        let keys = repo.read_keys().write_keys(repo.config_hash());
        Writer::new(&self.agent_store, keys, repo.config())
            .await
            .unwrap()
    }

    async fn backup(&self, tag: &str, unique_seed: u64) -> Backed {
        let writer = self.open_writer().await;
        self.commit(writer, tag, unique_seed).await
    }

    async fn commit(&self, mut writer: Writer<&Store>, tag: &str, unique_seed: u64) -> Backed {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shared.bin"), noise(1, SHARED_SIZE)).unwrap();
        std::fs::write(
            dir.path().join("unique.bin"),
            noise(unique_seed, UNIQUE_SIZE),
        )
        .unwrap();
        let outcome = writer
            .backup(
                &[dir.path().to_path_buf()],
                BackupOptions::default(),
                &|_| {},
            )
            .await
            .unwrap_or_else(|e| panic!("backup {tag}: {e}"));
        Backed {
            id: hex32(&outcome.snapshot_id),
            dir,
        }
    }

    async fn forget(&self, snapshot: &Backed) {
        let id = parse_hex32(&snapshot.id).unwrap();
        self.server_store
            .delete(&snapshot_object_key(&id))
            .await
            .unwrap();
    }

    async fn assert_restores(&self, snapshot: &Backed) {
        let repo = Repository::open(&self.server_store, PASSWORD)
            .await
            .unwrap();
        let mut reader = Reader::open(
            &self.server_store,
            repo.read_keys().duplicate(),
            repo.config(),
        )
        .unwrap();
        let target = tempfile::tempdir().unwrap();
        let report = reader
            .restore(
                &parse_hex32(&snapshot.id).unwrap(),
                target.path(),
                &RestoreOptions::default(),
                &|_| {},
            )
            .await
            .unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let root = target.path().join(
            snapshot
                .dir
                .path()
                .canonicalize()
                .unwrap()
                .strip_prefix("/")
                .unwrap(),
        );
        for file in ["shared.bin", "unique.bin"] {
            assert!(
                std::fs::read(root.join(file)).unwrap()
                    == std::fs::read(snapshot.dir.path().join(file)).unwrap(),
                "{file} of snapshot {} was not restored byte for byte",
                snapshot.id
            );
        }
    }

    /// Key to the modification time of every pack, index shard and manifest.
    async fn listing(&self) -> BTreeMap<String, Option<i64>> {
        let mut out = BTreeMap::new();
        for prefix in PREFIXES {
            for entry in self.server_store.list(prefix).await.unwrap() {
                out.insert(entry.key, entry.modified_ms);
            }
        }
        out
    }
}

struct Backed {
    id: String,
    dir: tempfile::TempDir,
}

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

fn wall_clock_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn three_days_on() -> i64 {
    wall_clock_ms() + 3 * DAY_MS
}

async fn real_cycle(ts: &TestServer, repo: &str) -> String {
    start_cycle(ts.state(), repo, false, GcOrigin::Manual, "test")
        .await
        .unwrap()
}

/// Steps through the pauses on a clock that starts at `now` until the cycle ends.
async fn finish(ts: &TestServer, id: &str, mut now: i64) -> GcState {
    loop {
        match step(ts.state(), id, now).await.unwrap() {
            StepOutcome::Advanced(_) => {}
            StepOutcome::Waiting { until_ms } => now = until_ms + 1_000,
            StepOutcome::Postponed => panic!("the cycle is postponed"),
            StepOutcome::Finished(state) => return state,
        }
    }
}

async fn report(ts: &TestServer, repo: &str, id: &str) -> Value {
    let response = ts
        .api()
        .await
        .get(&format!(
            "/api/v1/repositories/{repo}/gc/cycles/{id}/report"
        ))
        .await;
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

async fn failure(ts: &TestServer, id: &str) -> Option<String> {
    db::load_cycle(ts.state(), id).await.unwrap().unwrap().error
}

fn gone(
    before: &BTreeMap<String, Option<i64>>,
    after: &BTreeMap<String, Option<i64>>,
) -> Vec<String> {
    before
        .keys()
        .filter(|key| !after.contains_key(*key))
        .cloned()
        .collect()
}

fn operator_as(minio: &MinioServer, key: &str, secret: &str) -> Operator {
    opendal::install_default();
    Operator::new(
        services::S3::default()
            .endpoint(minio.endpoint())
            .bucket(BUCKET)
            .root("/")
            .region("us-east-1")
            .access_key_id(key)
            .secret_access_key(secret)
            .disable_ec2_metadata(),
    )
    .unwrap()
}

/// A repository with snapshots A, B and C of which A was forgotten.
async fn forgotten_a(cloud: &Cloud) -> (Backed, Backed, BTreeMap<String, Option<i64>>) {
    let a = cloud.backup("a", 11).await;
    let b = cloud.backup("b", 12).await;
    let c = cloud.backup("c", 13).await;
    let before = cloud.listing().await;
    cloud.forget(&a).await;
    (b, c, before)
}

async fn on_each_backend(kind: Bucket, test: impl AsyncFn(&Cloud, &TestServer, &str)) {
    for backend in backends() {
        let cloud = Cloud::start(kind).await;
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let repo = cloud.register(&ts).await;
        test(&cloud, &ts, &repo).await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_s3_agent_key_denied() {
    on_each_backend(Bucket::Versioned, async |cloud, ts, repo| {
        let (b, c, before) = forgotten_a(cloud).await;
        let pack = before
            .keys()
            .find(|key| key.starts_with("packs/"))
            .unwrap()
            .clone();
        assert!(matches!(
            cloud.agent_store.delete(&pack).await,
            Err(BlobError::Denied { .. })
        ));
        let version = cloud.minio.object_versions_all(BUCKET, &pack).await[0]
            .0
            .clone();
        let refused = operator_as(&cloud.minio, &cloud.agent_key.0, &cloud.agent_key.1)
            .delete_with(&pack)
            .version(&version)
            .await
            .unwrap_err();
        assert_eq!(refused.kind(), opendal::ErrorKind::PermissionDenied);
        assert!(matches!(
            cloud
                .agent_store
                .put_if_absent("locks/gc-x", Bytes::from_static(b"x"))
                .await,
            Err(BlobError::Denied { .. })
        ));
        assert_eq!(
            cloud.minio.object_versions_all(BUCKET, &pack).await.len(),
            1
        );

        let preflight = run_preflight(ts.state(), repo, true).await.unwrap();
        assert!(preflight.problems.is_empty(), "{:?}", preflight.problems);
        let id = real_cycle(ts, repo).await;
        assert_eq!(
            finish(ts, &id, three_days_on()).await,
            GcState::Done,
            "{:?}",
            failure(ts, &id).await
        );
        let removed = gone(&before, &cloud.listing().await);
        assert!(
            removed.iter().any(|key| key.starts_with("packs/")),
            "{removed:?}"
        );
        cloud.assert_restores(&b).await;
        cloud.assert_restores(&c).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_s3_versioned_purge() {
    on_each_backend(Bucket::Versioned, async |cloud, ts, repo| {
        let (b, c, before) = forgotten_a(cloud).await;
        let id = real_cycle(ts, repo).await;
        assert_eq!(
            finish(ts, &id, three_days_on()).await,
            GcState::Done,
            "{:?}",
            failure(ts, &id).await
        );

        let removed = gone(&before, &cloud.listing().await);
        assert!(
            removed.iter().any(|key| key.starts_with("packs/")),
            "{removed:?}"
        );
        for key in &removed {
            let versions = cloud.minio.object_versions_all(BUCKET, key).await;
            assert!(versions.is_empty(), "{key} keeps {versions:?}");
        }
        let done = report(ts, repo, &id).await;
        assert_eq!(done["locked_bytes"], 0, "{done}");
        assert!(done["bytes_freed"].as_u64().unwrap() > 0, "{done}");
        cloud.assert_restores(&b).await;
        cloud.assert_restores(&c).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_s3_object_lock_compliance() {
    on_each_backend(Bucket::Compliance, async |cloud, ts, repo| {
        let (b, c, before) = forgotten_a(cloud).await;
        let id = real_cycle(ts, repo).await;
        assert_eq!(
            finish(ts, &id, three_days_on()).await,
            GcState::Done,
            "{:?}",
            failure(ts, &id).await
        );

        let removed = gone(&before, &cloud.listing().await);
        assert!(
            removed.iter().any(|key| key.starts_with("packs/")),
            "{removed:?}"
        );
        for key in &removed {
            let versions = cloud.minio.object_versions_all(BUCKET, key).await;
            assert!(
                versions.iter().any(|(_, marker)| !marker),
                "the locked version of {key} is gone: {versions:?}"
            );
        }
        let done = report(ts, repo, &id).await;
        assert!(done["locked_bytes"].as_u64().unwrap() > 0, "{done}");
        assert_eq!(done["locked_until_unknown"], false, "{done}");
        let oldest = removed
            .iter()
            .filter_map(|key| before[key])
            .min()
            .expect("the removed objects have times");
        let until = done["locked_until"].as_i64().expect("a lock end");
        assert!(
            (until - (oldest + DAY_MS)).abs() <= MINUTE_MS,
            "locked_until {until}, expected about {}",
            oldest + DAY_MS
        );
        cloud.assert_restores(&b).await;
        cloud.assert_restores(&c).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_s3_object_lock_expired_purged() {
    on_each_backend(Bucket::Governance, async |cloud, ts, repo| {
        let (b, c, _) = forgotten_a(cloud).await;
        let first = real_cycle(ts, repo).await;
        let now = three_days_on();
        assert_eq!(
            finish(ts, &first, now).await,
            GcState::Done,
            "{:?}",
            failure(ts, &first).await
        );
        let held = report(ts, repo, &first).await;
        assert!(held["locked_bytes"].as_u64().unwrap() > 0, "{held}");
        let locked: BTreeSet<String> = held["locked"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["key"].as_str().unwrap().to_string())
            .collect();
        assert!(!locked.is_empty());

        for key in &locked {
            for (version, marker) in cloud.minio.object_versions_all(BUCKET, key).await {
                if !marker {
                    cloud.minio.clear_retention(BUCKET, key, &version).await;
                }
            }
        }
        let settings = GcSettings {
            enabled: true,
            repack_below_pct: 1,
            ..GcSettings::default()
        };
        db::save_settings(ts.state(), repo, &settings)
            .await
            .unwrap();

        let second = real_cycle(ts, repo).await;
        assert_eq!(
            finish(ts, &second, now + 3 * DAY_MS).await,
            GcState::Done,
            "{:?}",
            failure(ts, &second).await
        );
        let freed = report(ts, repo, &second).await;
        assert_eq!(freed["locked_bytes"], 0, "{freed}");
        assert!(freed["bytes_freed"].as_u64().unwrap() > 0, "{freed}");
        for key in &locked {
            let versions = cloud.minio.object_versions_all(BUCKET, key).await;
            assert!(versions.is_empty(), "{key} keeps {versions:?}");
        }
        cloud.assert_restores(&b).await;
        cloud.assert_restores(&c).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_s3_dry_run_object_lock() {
    on_each_backend(Bucket::Compliance, async |cloud, ts, repo| {
        let (_, _, before) = forgotten_a(cloud).await;
        let untouched = cloud.listing().await;
        let id = start_cycle(ts.state(), repo, true, GcOrigin::Manual, "test")
            .await
            .unwrap();
        run_dry_run_at(ts.state(), &id, three_days_on()).await;

        let estimate = report(ts, repo, &id).await;
        assert_eq!(estimate["dry_run"], true, "{estimate}");
        assert!(estimate["locked_bytes"].as_u64().unwrap() > 0, "{estimate}");
        assert_eq!(estimate["bytes_freed"], 0, "{estimate}");
        let until = estimate["locked_until"].as_i64().expect("a lock end");
        let packs = before
            .iter()
            .filter(|(key, _)| key.starts_with("packs/"))
            .filter_map(|(_, modified)| *modified);
        let (oldest, newest) =
            packs.fold((i64::MAX, i64::MIN), |(lo, hi), t| (lo.min(t), hi.max(t)));
        assert!(
            until >= oldest + DAY_MS - MINUTE_MS && until <= newest + DAY_MS + MINUTE_MS,
            "locked_until {until} outside the packs' lock window"
        );
        assert_eq!(cloud.listing().await, untouched);
        assert!(cloud.server_store.list("locks/").await.unwrap().is_empty());
    })
    .await;
}

/// The server's clock jumped two days ahead during the pause while the
/// storage's did not: deletion waits until the storage has seen the pause.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_s3_server_clock_ahead_waits() {
    on_each_backend(Bucket::Versioned, async |cloud, ts, repo| {
        let (_, _, before) = forgotten_a(cloud).await;
        let id = real_cycle(ts, repo).await;
        loop {
            match step(ts.state(), &id, three_days_on()).await.unwrap() {
                StepOutcome::Advanced(_) => {}
                StepOutcome::Waiting { .. } => break,
                other => panic!("the cycle must wait, not {other:?}"),
            }
        }
        let now = wall_clock_ms();
        sqlx::query("UPDATE gc_cycles SET published_at = $1, delete_after = $2 WHERE id = $3")
            .bind(now - 2 * DAY_MS)
            .bind(now - DAY_MS)
            .bind(&id)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();

        let StepOutcome::Waiting { until_ms } = step(ts.state(), &id, now).await.unwrap() else {
            panic!("the cycle went on although the storage has not seen the pause");
        };
        assert!(until_ms > now + DAY_MS - 10 * MINUTE_MS, "{until_ms}");
        let cycle = db::load_cycle(ts.state(), &id).await.unwrap().unwrap();
        assert_eq!(cycle.state, GcState::Waiting);
        assert_eq!(cycle.delete_after, Some(until_ms));
        let packs = |listing: &BTreeMap<String, Option<i64>>| {
            listing
                .keys()
                .filter(|key| key.starts_with("packs/"))
                .count()
        };
        assert!(packs(&cloud.listing().await) >= packs(&before));
        let probes = cloud.server_store.list("locks/clock-").await.unwrap();
        assert!(probes.is_empty(), "{probes:?}");
    })
    .await;
}

/// A backup that opened before the cleanup marker and commits at the very end
/// of its week-long limit keeps the data it deduplicated against. The agent
/// writes no writer marker, so the job's time limit is the only thing that
/// holds the deletion.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_s3_week_long_backup_keeps_its_data() {
    on_each_backend(Bucket::Versioned, async |cloud, ts, repo| {
        let t0 = wall_clock_ms();
        let a = cloud.backup("a", 11).await;
        let a_packs: BTreeSet<String> = pack_keys(&cloud.listing().await);
        let b = cloud.backup("b", 12).await;
        let c = cloud.backup("c", 13).await;
        let before = pack_keys(&cloud.listing().await);
        cloud.forget(&a).await;
        seed_agent(ts, repo, "host-1", r#"["gc_v1"]"#).await;
        seed_backup_job(ts, "host-1", t0, 7 * 86_400).await;
        let writer = cloud.open_writer().await;

        let id = real_cycle(ts, repo).await;
        let StepOutcome::Waiting { until_ms } = settle(ts, &id, t0 + 3 * DAY_MS).await else {
            panic!("the cycle must wait for the week-long backup");
        };
        let limit = t0 + 7 * DAY_MS + HOUR_MS;
        let published = pack_keys(&cloud.listing().await);

        let opened = Repository::open(&cloud.server_store, PASSWORD)
            .await
            .unwrap();
        let locks = read_gc_locks(
            &cloud.server_store,
            opened.read_keys().k_idx(),
            opened.read_keys().repo_id(),
        )
        .await
        .unwrap();
        let candidates: BTreeSet<String> = locks
            .candidate_packs()
            .iter()
            .map(pack_object_key)
            .collect();
        assert!(
            a_packs.iter().any(|key| candidates.contains(key)),
            "the packs of A are not candidates of the marker: the scenario would be empty"
        );

        let mut waits = vec![(
            t0 + 3 * DAY_MS,
            StepOutcome::Waiting { until_ms },
            published.clone(),
        )];
        for t in [
            t0 + 4 * DAY_MS + HOUR_MS,
            t0 + 5 * DAY_MS,
            t0 + 6 * DAY_MS,
            limit - MINUTE_MS,
        ] {
            let outcome = settle(ts, &id, t).await;
            waits.push((t, outcome, pack_keys(&cloud.listing().await)));
        }

        let week = cloud.commit(writer, "week", 11).await;
        let mut added_bytes = 0;
        for key in pack_keys(&cloud.listing().await).difference(&published) {
            added_bytes += cloud.server_store.get(key).await.unwrap().len();
        }
        assert!(
            added_bytes < UNIQUE_SIZE / 2,
            "the week's backup wrote {added_bytes} new bytes: it did not deduplicate against A"
        );

        sqlx::query(
            "UPDATE jobs SET state = 'succeeded', finished_at = $1 WHERE host_id = 'host-1'",
        )
        .bind(limit)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
        assert_eq!(
            finish(ts, &id, limit + HOUR_MS).await,
            GcState::Done,
            "{:?}",
            failure(ts, &id).await
        );
        cloud.assert_restores(&week).await;
        cloud.assert_restores(&b).await;
        cloud.assert_restores(&c).await;

        for (t, outcome, seen) in waits {
            let StepOutcome::Waiting { until_ms } = outcome else {
                panic!("at {t} the cycle must wait, not {outcome:?}");
            };
            assert!(until_ms >= limit, "at {t} the pause ends at {until_ms}");
            assert_eq!(seen, published, "packs changed while waiting at {t}");
            assert!(before.is_subset(&seen), "a pack was deleted at {t}");
        }
    })
    .await;
}

fn pack_keys(listing: &BTreeMap<String, Option<i64>>) -> BTreeSet<String> {
    listing
        .keys()
        .filter(|key| key.starts_with("packs/"))
        .cloned()
        .collect()
}

/// Steps while the cycle advances and returns the first outcome that is not an advance.
async fn settle(ts: &TestServer, id: &str, now: i64) -> StepOutcome {
    loop {
        match step(ts.state(), id, now).await.unwrap() {
            StepOutcome::Advanced(_) => {}
            other => return other,
        }
    }
}
