mod common;

use std::collections::BTreeSet;
use std::path::PathBuf;

use common::{AgentEnv, Backend, TestServer, backends};
use lodestore_policy::jiff::Timestamp;
use lodestore_policy::{BackupPolicy, Retention};
use lodestore_server::catalog::retention::plan_policy;
use lodestore_server::db::now_ms;
use lodestore_server::jobs::{enqueue_with_origin, job_spec};
use lodestore_server::schedule::tick;
use serde_json::{Value, json};
use sqlx::{AnyPool, Row};

const POLICY: &str = "pools";
const DAY_MS: i64 = 24 * 3600 * 1000;

const BY_ORIGIN: &str =
    "SELECT COUNT(*) AS n FROM snapshots WHERE policy_name = 'pools' AND origin = $1";

struct Pools {
    ts: TestServer,
    env: AgentEnv,
}

struct Seed<'a> {
    id: &'a str,
    received: i64,
    origin: Option<&'a str>,
    stdin: Option<&'a str>,
    exact: bool,
    job: &'a str,
}

impl Pools {
    async fn start(backend: &Backend, retention: &str) -> Pools {
        let ts = TestServer::start_with(backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        let env = AgentEnv::start_on(&ts, &repo, "main", "host-a").await;
        let data: PathBuf = env.work.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("file"), "content").unwrap();
        let yaml = format!(
            "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: {POLICY}\n\
             spec:\n  repository: main\n  selector:\n    matchLabels:\n      agent: host-a\n  \
             paths:\n    - {}\n  schedule:\n    cron: \"*/5 * * * *\"\n    jitter: 0m\n{retention}",
            data.display()
        );
        let response = env
            .api
            .post("/api/v1/policies", &json!({ "yaml": yaml }))
            .await;
        assert_eq!(response.status(), 201);
        Pools { ts, env }
    }

    async fn stop(self) {
        self.env.stop().await;
        self.ts.stop().await;
    }

    fn pool(&self) -> &AnyPool {
        &self.ts.state().db.pool
    }

    async fn scheduled(&self, count: usize) {
        let at = |text: &str| -> Timestamp { text.parse().unwrap() };
        let state = self.ts.state();
        tick(state, at("2026-06-01T10:02:00Z")).await.unwrap();
        for n in 1..=count {
            let now = format!("2026-06-01T10:{:02}:10Z", 5 * n);
            let queued = tick(state, at(&now)).await.unwrap().queued;
            assert_eq!(queued.len(), 1, "slot {n}");
            self.settle(&queued[0].job_id).await;
        }
    }

    async fn run_now(&self) {
        let response = self
            .env
            .api
            .post(&format!("/api/v1/policies/{POLICY}/run"), &json!({}))
            .await;
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["jobs"][0]["created"], true, "{body}");
        self.settle(body["jobs"][0]["id"].as_str().unwrap()).await;
    }

    async fn settle(&self, job_id: &str) -> Value {
        let job = self.env.wait_job_settled(job_id).await;
        assert!(
            job["state"] == "succeeded" || job["state"] == "warnings",
            "{job}"
        );
        job
    }

    async fn count(&self, sql: &'static str, args: &[&str]) -> i64 {
        let mut query = sqlx::query(sql);
        for arg in args {
            query = query.bind(*arg);
        }
        query
            .fetch_one(self.pool())
            .await
            .unwrap()
            .try_get("n")
            .unwrap()
    }

    async fn policy_with(&self, retention: Retention) -> BackupPolicy {
        let row = sqlx::query("SELECT spec_json FROM policies WHERE name = $1")
            .bind(POLICY)
            .fetch_one(self.pool())
            .await
            .unwrap();
        let mut policy: BackupPolicy =
            serde_json::from_str(&row.try_get::<String, _>("spec_json").unwrap()).unwrap();
        policy.spec.retention = Some(retention);
        policy
    }

    async fn seed(&self, row: Seed<'_>) {
        sqlx::query(
            "INSERT INTO snapshots (id, repository_id, host_id, hostname, time_ms, paths_json, \
             tags_json, policy_name, stdin_name, job_id, incomplete, errors_total, warnings_total, \
             stats_json, discovered_at, received_at, received_exact, verified_at, verified_by, \
             origin) \
             SELECT $1, id, $2, 'inserted', $3, '[]', '[]', $4, $5, $6, 0, 0, 0, '{}', $3, $3, $7, \
             $3, 'server', $8 \
             FROM repositories WHERE name = 'main'",
        )
        .bind(row.id)
        .bind(&self.env.host)
        .bind(row.received)
        .bind(POLICY)
        .bind(row.stdin)
        .bind(row.job)
        .bind(i64::from(row.exact))
        .bind(row.origin)
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn seed_job(&self, id: &str, origin: &str, stdin: Option<&str>) {
        sqlx::query(
            "INSERT INTO jobs (id, host_id, source, state, spec, queued_at, origin, stdin_name) \
             VALUES ($1, $2, 'manual', 'succeeded', $3, 0, $4, $5)",
        )
        .bind(id)
        .bind(&self.env.host)
        .bind(Vec::<u8>::new())
        .bind(origin)
        .bind(stdin)
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn seed_pools(&self, scheduled: &[&str], manual: &[&str], exact: bool) {
        let base = now_ms() - 30 * DAY_MS;
        for (n, id) in scheduled.iter().enumerate() {
            let received = base + n as i64 * DAY_MS;
            self.seed(Seed {
                id: &id.repeat(64),
                received,
                origin: Some("schedule"),
                stdin: None,
                exact,
                job: "seeded",
            })
            .await;
        }
        for (n, id) in manual.iter().enumerate() {
            let received = base + n as i64 * DAY_MS;
            self.seed(Seed {
                id: &id.repeat(64),
                received,
                origin: Some("manual"),
                stdin: None,
                exact: true,
                job: "seeded",
            })
            .await;
        }
    }

    async fn forgotten(&self, policy: &BackupPolicy) -> BTreeSet<String> {
        let rules = policy.spec.retention.clone().unwrap();
        let plan = plan_policy(self.ts.state(), policy, &rules).await.unwrap();
        plan.hosts
            .iter()
            .flat_map(|h| &h.items)
            .filter(|i| !i.keep)
            .map(|i| i.id[..1].to_string())
            .collect()
    }

    async fn planned(&self, policy: &BackupPolicy) -> BTreeSet<String> {
        let rules = policy.spec.retention.clone().unwrap();
        let plan = plan_policy(self.ts.state(), policy, &rules).await.unwrap();
        plan.hosts
            .iter()
            .flat_map(|h| &h.items)
            .map(|i| i.id[..1].to_string())
            .collect()
    }
}

fn keep_last(n: i64) -> Retention {
    Retention {
        keep_last: Some(n),
        ..Retention::default()
    }
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_pools_manual_runs_keep_scheduled() {
    for backend in backends() {
        let pools = Pools::start(&backend, "  retention:\n    keepLast: 3\n").await;
        pools.scheduled(3).await;
        let name = backend.name();
        assert_eq!(pools.count(BY_ORIGIN, &["schedule"]).await, 3, "{name}");

        for _ in 0..5 {
            pools.run_now().await;
        }

        assert_eq!(pools.count(BY_ORIGIN, &["schedule"]).await, 3, "{name}");
        assert_eq!(pools.count(BY_ORIGIN, &["manual"]).await, 3, "{name}");
        pools.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_pools_scheduler_sets_origin_at_insert() {
    for backend in backends() {
        let pools = Pools::start(&backend, "").await;
        let policy = pools.policy_with(Retention::default()).await;
        let policy_id: String = sqlx::query("SELECT id FROM policies WHERE name = $1")
            .bind(POLICY)
            .fetch_one(pools.pool())
            .await
            .unwrap()
            .try_get("id")
            .unwrap();
        let hosts = [(pools.env.host.clone(), pools.env.hostname.clone())];
        let name = backend.name();

        for origin in ["schedule", "catchup"] {
            let queued = enqueue_with_origin(
                pools.ts.state(),
                &policy_id,
                POLICY,
                &hosts,
                &job_spec(&policy),
                origin,
            )
            .await
            .unwrap();
            let job_id = &queued[0].id;
            let stored: String = sqlx::query("SELECT origin FROM jobs WHERE id = $1")
                .bind(job_id)
                .fetch_one(pools.pool())
                .await
                .unwrap()
                .try_get("origin")
                .unwrap();
            assert_eq!(stored, origin, "{name}");

            let job = pools.settle(job_id).await;

            let snapshot: String = sqlx::query("SELECT origin FROM snapshots WHERE id = $1")
                .bind(job["snapshot_id"].as_str().unwrap())
                .fetch_one(pools.pool())
                .await
                .unwrap()
                .try_get("origin")
                .unwrap();
            assert_eq!(snapshot, origin, "{name}");
        }
        pools.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_pools_floor_per_pool() {
    for backend in backends() {
        let pools = Pools::start(&backend, "").await;
        pools
            .seed_pools(&["1", "2", "3", "4"], &["a", "b", "c", "d"], true)
            .await;
        let policy = pools
            .policy_with(Retention {
                keep_within: Some("1m".into()),
                ..Retention::default()
            })
            .await;

        assert_eq!(
            pools.forgotten(&policy).await,
            set(&["1", "2", "a", "b"]),
            "{}",
            backend.name()
        );
        pools.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_pools_stdin_never_forgotten() {
    for backend in backends() {
        let pools = Pools::start(&backend, "  retention:\n    keepLast: 1\n").await;
        let base = now_ms() - 30 * DAY_MS;
        pools
            .seed(Seed {
                id: &"e".repeat(64),
                received: base,
                origin: Some("stdin"),
                stdin: Some("app.dump"),
                exact: true,
                job: "seeded",
            })
            .await;
        pools
            .seed(Seed {
                id: &"f".repeat(64),
                received: base + DAY_MS,
                origin: None,
                stdin: Some("app.dump"),
                exact: true,
                job: "seeded",
            })
            .await;

        pools.run_now().await;

        let name = backend.name();
        let stdin = "SELECT COUNT(*) AS n FROM snapshots WHERE stdin_name IS NOT NULL";
        assert_eq!(pools.count(stdin, &[]).await, 2, "{name}");
        let policy = pools.policy_with(keep_last(1)).await;
        assert!(
            pools.planned(&policy).await.is_disjoint(&set(&["e", "f"])),
            "{name}"
        );
        pools.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_pools_unconfirmed_time_keeps_all() {
    for backend in backends() {
        let pools = Pools::start(&backend, "").await;
        pools
            .seed_pools(&["1", "2", "3"], &["a", "b", "c"], false)
            .await;
        let policy = pools.policy_with(keep_last(1)).await;

        assert!(
            pools.forgotten(&policy).await.is_empty(),
            "{}",
            backend.name()
        );
        pools.stop().await;
    }
}

fn backfill_sql(backend: &Backend) -> &'static str {
    match backend {
        Backend::Sqlite => include_str!("../migrations/sqlite/0024_snapshot_origin.sql"),
        Backend::Postgres(_) => include_str!("../migrations/postgres/0024_snapshot_origin.sql"),
    }
}

async fn backfill(pools: &Pools, backend: &Backend) {
    let update = backfill_sql(backend)
        .split(';')
        .find(|s| s.trim_start().starts_with("UPDATE"))
        .expect("the migration backfills snapshots");
    sqlx::query(sqlx::AssertSqlSafe(update.to_string()))
        .execute(pools.pool())
        .await
        .unwrap();
}

async fn origin_of(pools: &Pools, id: &str) -> Option<String> {
    sqlx::query("SELECT origin FROM snapshots WHERE id = $1")
        .bind(id.repeat(64))
        .fetch_one(pools.pool())
        .await
        .unwrap()
        .try_get("origin")
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_pools_backfill_sets_origin() {
    for backend in backends() {
        let pools = Pools::start(&backend, "").await;
        pools.seed_job("job-schedule", "schedule", None).await;
        pools
            .seed_job("job-stdin", "manual", Some("app.dump"))
            .await;
        let base = now_ms() - DAY_MS;
        for (n, (id, job)) in [("1", "job-schedule"), ("2", "job-stdin")]
            .iter()
            .enumerate()
        {
            pools
                .seed(Seed {
                    id: &id.repeat(64),
                    received: base + n as i64,
                    origin: None,
                    stdin: None,
                    exact: true,
                    job,
                })
                .await;
        }

        backfill(&pools, &backend).await;

        let name = backend.name();
        assert_eq!(
            origin_of(&pools, "1").await.as_deref(),
            Some("schedule"),
            "{name}"
        );
        assert_eq!(
            origin_of(&pools, "2").await.as_deref(),
            Some("stdin"),
            "{name}"
        );
        pools.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_pools_backfill_purged_job_is_scheduled() {
    for backend in backends() {
        let pools = Pools::start(&backend, "").await;
        pools.seed_job("job-manual", "manual", None).await;
        for n in 1..=3 {
            pools
                .seed_job(&format!("job-gone-{n}"), "manual", None)
                .await;
        }
        let base = now_ms() - 30 * DAY_MS;
        let rows = [
            ("a", "job-manual"),
            ("1", "job-gone-1"),
            ("2", "job-gone-2"),
            ("3", "job-gone-3"),
        ];
        for (n, (id, job)) in rows.iter().enumerate() {
            pools
                .seed(Seed {
                    id: &id.repeat(64),
                    received: base + n as i64 * DAY_MS,
                    origin: None,
                    stdin: None,
                    exact: true,
                    job,
                })
                .await;
        }
        sqlx::query("DELETE FROM jobs WHERE id LIKE 'job-gone-%'")
            .execute(pools.pool())
            .await
            .unwrap();

        backfill(&pools, &backend).await;

        let name = backend.name();
        assert_eq!(
            origin_of(&pools, "a").await.as_deref(),
            Some("manual"),
            "{name}"
        );
        for id in ["1", "2", "3"] {
            assert_eq!(origin_of(&pools, id).await, None, "{name}");
        }
        let policy = pools.policy_with(keep_last(1)).await;
        assert_eq!(pools.forgotten(&policy).await, set(&["1"]), "{name}");
        pools.stop().await;
    }
}
