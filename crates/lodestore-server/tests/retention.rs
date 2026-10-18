mod common;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use common::{AgentEnv, TestServer, WAIT, backends};
use lodestore_policy::{BackupPolicy, Retention};
use lodestore_server::catalog::retention::{after_backup, plan_policy, run_for_pair};
use lodestore_server::db::now_ms;
use serde_json::{Value, json};
use sqlx::Row;

impl AgentEnv {
    fn retention_data(&self, name: &str) -> PathBuf {
        let dir = self.work.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let old = SystemTime::now() - Duration::from_secs(3600);
        for i in 0..4 {
            let path = dir.join(format!("file-{i}"));
            std::fs::write(&path, format!("file {i} {}", "x".repeat(i * 300))).unwrap();
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        dir
    }

    async fn retention_policy(&self, name: &str, path: &Path, retention: &str) {
        let yaml = format!(
            "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: {name}\n\
             spec:\n  repository: main\n  selector:\n    matchLabels:\n      env: e2e\n  \
             paths:\n    - {}\n{retention}",
            path.display()
        );
        let response = self
            .api
            .post("/api/v1/policies", &json!({ "yaml": yaml }))
            .await;
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        assert_eq!(status, 201, "{body}");
    }

    async fn retention_run(&self, policy: &str) -> Value {
        let response = self
            .api
            .post(&format!("/api/v1/policies/{policy}/run"), &json!({}))
            .await;
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["jobs"][0]["created"], true, "{body}");
        let job = self.wait_job(body["jobs"][0]["id"].as_str().unwrap()).await;
        assert!(
            job["state"] == "succeeded" || job["state"] == "warnings",
            "{job}"
        );
        job
    }

    async fn retention_of(&self, job_id: &str) -> Option<Value> {
        let row = sqlx::query("SELECT retention_json FROM jobs WHERE id = $1")
            .bind(job_id)
            .fetch_one(&self.ts().state().db.pool)
            .await
            .unwrap();
        row.try_get::<Option<String>, _>("retention_json")
            .unwrap()
            .map(|json| serde_json::from_str(&json).unwrap())
    }

    async fn wait_retention(&self, job_id: &str) -> Value {
        let deadline = std::time::Instant::now() + WAIT;
        loop {
            let current = self.retention_of(job_id).await;
            if let Some(done) = current.filter(|r| r["state"] != "running") {
                return done;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "retention of {job_id} stuck"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn retention_runs(&self, policy: &str, count: usize) -> Vec<Value> {
        let mut jobs = Vec::new();
        for _ in 0..count {
            let job = self.retention_run(policy).await;
            self.wait_retention(job["id"].as_str().unwrap()).await;
            jobs.push(job);
        }
        jobs
    }

    async fn catalog_ids(&self) -> Vec<String> {
        sqlx::query("SELECT id FROM snapshots ORDER BY received_at, id")
            .fetch_all(&self.ts().state().db.pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.try_get("id").unwrap())
            .collect()
    }

    async fn stored_policy(&self, name: &str, keep_last: i64) -> BackupPolicy {
        let row = sqlx::query("SELECT spec_json FROM policies WHERE name = $1")
            .bind(name)
            .fetch_one(&self.ts().state().db.pool)
            .await
            .unwrap();
        let mut policy: BackupPolicy =
            serde_json::from_str(&row.try_get::<String, _>("spec_json").unwrap()).unwrap();
        policy.spec.retention = Some(Retention {
            keep_last: Some(keep_last),
            ..Retention::default()
        });
        policy
    }

    async fn insert_row(&self, id: &str, host: Option<&str>, policy: Option<&str>, received: i64) {
        self.insert_row_as(id, host, policy, received, Some("seeded"))
            .await;
    }

    async fn insert_row_as(
        &self,
        id: &str,
        host: Option<&str>,
        policy: Option<&str>,
        received: i64,
        job: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO snapshots (id, repository_id, host_id, hostname, time_ms, paths_json, \
             tags_json, policy_name, job_id, incomplete, errors_total, warnings_total, \
             stats_json, discovered_at, received_at, received_exact) \
             SELECT $1, id, $2, 'inserted', $3, '[]', '[]', $4, $5, 0, 0, 0, '{}', $3, $3, 1 \
             FROM repositories WHERE name = 'main'",
        )
        .bind(id)
        .bind(host)
        .bind(received)
        .bind(policy)
        .bind(job)
        .execute(&self.ts().state().db.pool)
        .await
        .unwrap();
    }

    async fn count(&self, sql: &'static str, args: &[&str]) -> i64 {
        let mut query = sqlx::query(sql);
        for arg in args {
            query = query.bind(*arg);
        }
        query
            .fetch_one(&self.ts().state().db.pool)
            .await
            .unwrap()
            .try_get("n")
            .unwrap()
    }

    async fn run_sql(&self, sql: &'static str, args: &[&str]) {
        let mut query = sqlx::query(sql);
        for arg in args {
            query = query.bind(*arg);
        }
        query.execute(&self.ts().state().db.pool).await.unwrap();
    }

    fn bucket_snapshot_files(&self) -> usize {
        std::fs::read_dir(self.ts().local_repo_root("main").join("snapshots"))
            .unwrap()
            .count()
    }

    async fn event_kinds(&self, job_id: &str) -> Vec<String> {
        self.events(job_id, "")
            .await
            .iter()
            .map(|e| e["kind"].as_str().unwrap().to_string())
            .collect()
    }
}

fn snapshot_of(job: &Value) -> String {
    job["snapshot_id"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_forgets_after_backup() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let data = env.retention_data("data");
        env.retention_policy("keep-two", &data, "  retention:\n    keepLast: 2\n")
            .await;
        let jobs = env.retention_runs("keep-two", 4).await;
        let ids: Vec<String> = jobs.iter().map(snapshot_of).collect();

        let mut remaining = env.catalog_ids().await;
        remaining.sort();
        let mut expected = ids[2..].to_vec();
        expected.sort();
        assert_eq!(remaining, expected, "{}", backend.name());
        assert_eq!(env.bucket_snapshot_files(), 2, "{}", backend.name());

        for (n, job) in jobs.iter().enumerate() {
            let job_id = job["id"].as_str().unwrap();
            let retention = env.retention_of(job_id).await.unwrap();
            let forgot = u64::from(n >= 2);
            assert_eq!(retention["state"], "done", "{retention}");
            assert_eq!(retention["forgot"], forgot, "job {n}: {retention}");
            assert_eq!(retention["blocked"], json!([]), "job {n}: {retention}");
            let forgotten = env
                .event_kinds(job_id)
                .await
                .iter()
                .filter(|k| *k == "snapshot_forgotten")
                .count() as u64;
            assert_eq!(forgotten, forgot, "job {n}");
        }
        let events = env.events(jobs[2]["id"].as_str().unwrap(), "").await;
        let entry = events
            .iter()
            .find(|e| e["kind"] == "snapshot_forgotten")
            .unwrap();
        assert_eq!(
            entry["message"],
            format!("Forgot snapshot {}: not kept by any rule", ids[0])
        );
        assert_eq!(entry["level"], "info");
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_schema() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let repo = ts.create_local_repository("main").await;
        let pool = &ts.state().db.pool;
        sqlx::query(
            "INSERT INTO snapshots (id, repository_id, hostname, time_ms, paths_json, tags_json, \
             incomplete, errors_total, warnings_total, stats_json, discovered_at) \
             VALUES ('s1', $1, 'h', 0, '[]', '[]', 0, 0, 0, '{}', 0)",
        )
        .bind(&repo)
        .execute(pool)
        .await
        .unwrap();
        let row = sqlx::query(
            "SELECT suspicious, suspicious_by, suspicious_reason, anomaly_json, suspicious_at, \
             suspicious_cleared_by, suspicious_cleared_reason, suspicious_cleared_at \
             FROM snapshots WHERE id = 's1'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(row.try_get::<i64, _>("suspicious").unwrap(), 0);
        for column in ["suspicious_by", "suspicious_reason", "anomaly_json"] {
            assert_eq!(row.try_get::<Option<String>, _>(column).unwrap(), None);
        }
        for column in ["suspicious_at", "suspicious_cleared_at"] {
            assert_eq!(row.try_get::<Option<i64>, _>(column).unwrap(), None);
        }
        sqlx::query("SELECT retention_json FROM jobs")
            .fetch_all(pool)
            .await
            .unwrap();
        ts.stop().await;
    }
}

const PAIR_COUNT: &str =
    "SELECT COUNT(*) AS n FROM snapshots WHERE policy_name = $1 AND host_id = $2";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_ignores_other_pairs_and_manual() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let other = AgentEnv::start_on(env.ts(), &env.repo, "main", "host-two").await;
        let data = env.retention_data("data");
        env.retention_policy("a", &data, "  retention:\n    keepLast: 1\n")
            .await;
        env.retention_policy("b", &data, "").await;
        let base = now_ms() - 600_000;
        env.insert_row(&"1".repeat(64), Some(&env.host), None, base)
            .await;
        for (n, id) in ["2", "3", "4"].iter().enumerate() {
            env.insert_row(
                &id.repeat(64),
                Some(&other.host),
                Some("a"),
                base + n as i64,
            )
            .await;
        }

        let a_jobs = env.retention_runs("a", 3).await;
        let b_jobs = env.retention_runs("b", 2).await;

        let name = backend.name();
        assert_eq!(env.count(PAIR_COUNT, &["a", &env.host]).await, 2, "{name}");
        assert_eq!(env.count(PAIR_COUNT, &["b", &env.host]).await, 2, "{name}");
        assert_eq!(
            env.count(PAIR_COUNT, &["a", &other.host]).await,
            3,
            "{name}"
        );
        let manual = "SELECT COUNT(*) AS n FROM snapshots WHERE policy_name IS NULL";
        assert_eq!(env.count(manual, &[]).await, 1, "{name}");
        let last = env.retention_of(a_jobs[2]["id"].as_str().unwrap()).await;
        assert_eq!(last.unwrap()["forgot"], 1);
        for job in &b_jobs {
            let skipped = env.retention_of(job["id"].as_str().unwrap()).await;
            assert_eq!(skipped, Some(json!({ "state": "skipped" })));
        }
        other.stop().await;
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_suspicious_not_counted() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let data = env.retention_data("data");
        env.retention_policy("p", &data, "").await;
        let jobs = env.retention_runs("p", 4).await;
        let ids: Vec<String> = jobs.iter().map(snapshot_of).collect();
        env.run_sql(
            "UPDATE snapshots SET suspicious = 1, suspicious_by = 'anomaly' WHERE id = $1",
            &[&ids[3]],
        )
        .await;

        let policy = env.stored_policy("p", 1).await;
        let job_id = jobs[3]["id"].as_str().unwrap();
        let outcome = run_for_pair(env.ts().state(), &policy, &env.host, job_id)
            .await
            .unwrap();

        assert_eq!((outcome.forgot, outcome.kept), (1, 2), "{}", backend.name());
        let mut remaining = env.catalog_ids().await;
        remaining.sort();
        let mut expected = ids[1..].to_vec();
        expected.sort();
        assert_eq!(remaining, expected, "{}", backend.name());
        let stored = env.retention_of(job_id).await.unwrap();
        assert_eq!(stored["forgot"], 1);
        assert_eq!(stored["kept"], 2);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_deleted_policy_snapshots_stay() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let data = env.retention_data("data");
        env.retention_policy("gone", &data, "").await;
        env.retention_policy("live", &data, "  retention:\n    keepLast: 1\n")
            .await;
        let gone_jobs = env.retention_runs("gone", 3).await;
        env.run_sql("DELETE FROM policies WHERE name = $1", &["gone"])
            .await;
        let gone_job = gone_jobs[2]["id"].as_str().unwrap();
        env.run_sql(
            "UPDATE jobs SET retention_json = NULL WHERE id = $1",
            &[gone_job],
        )
        .await;
        env.retention_runs("live", 3).await;

        after_backup(env.ts().state(), gone_job).await.unwrap();

        assert_eq!(
            env.retention_of(gone_job).await,
            Some(json!({ "state": "skipped" }))
        );
        assert_eq!(env.count(PAIR_COUNT, &["gone", &env.host]).await, 3);
        assert_eq!(env.count(PAIR_COUNT, &["live", &env.host]).await, 2);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_runs_once_per_job() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let data = env.retention_data("data");
        env.retention_policy("p", &data, "  retention:\n    keepLast: 2\n")
            .await;
        let jobs = env.retention_runs("p", 3).await;
        let job_id = jobs[2]["id"].as_str().unwrap();
        let before = env.retention_of(job_id).await;
        let events = env.events(job_id, "").await.len();
        let straggler = "b".repeat(64);
        env.insert_row(&straggler, Some(&env.host), Some("p"), now_ms() - 600_000)
            .await;

        after_backup(env.ts().state(), job_id).await.unwrap();

        assert_eq!(env.retention_of(job_id).await, before);
        assert_eq!(env.events(job_id, "").await.len(), events);
        let by_id = "SELECT COUNT(*) AS n FROM snapshots WHERE id = $1";
        assert_eq!(
            env.count(by_id, &[&straggler]).await,
            1,
            "{}",
            backend.name()
        );
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_serialized_per_pair() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let data = env.retention_data("data");
        env.retention_policy("p", &data, "").await;
        let jobs = env.retention_runs("p", 4).await;
        let ids: Vec<String> = jobs.iter().map(snapshot_of).collect();
        let policy = env.stored_policy("p", 2).await;
        let job_id = jobs[3]["id"].as_str().unwrap();
        let state = env.ts().state();

        let (first, second) = tokio::join!(
            run_for_pair(state, &policy, &env.host, job_id),
            run_for_pair(state, &policy, &env.host, job_id)
        );

        let (first, second) = (first.unwrap(), second.unwrap());
        assert_eq!(first.forgot + second.forgot, 2, "{}", backend.name());
        let mut remaining = env.catalog_ids().await;
        remaining.sort();
        let mut expected = ids[2..].to_vec();
        expected.sort();
        assert_eq!(remaining, expected, "{}", backend.name());
        assert_eq!(env.bucket_snapshot_files(), 2);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_skips_failed_and_incomplete() {
    const FAILED: &str = "UPDATE jobs SET state = 'failed', retention_json = NULL WHERE id = $1";
    const DONE: &str = "UPDATE jobs SET state = 'succeeded', retention_json = NULL WHERE id = $1";
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let data = env.retention_data("data");
        env.retention_policy("p", &data, "").await;
        let jobs = env.retention_runs("p", 3).await;
        let job_id = jobs[2]["id"].as_str().unwrap();
        let snapshot = snapshot_of(&jobs[2]);
        let policy = env.stored_policy("p", 1).await;
        env.run_sql(
            "UPDATE policies SET spec_json = $1 WHERE name = 'p'",
            &[&serde_json::to_string(&policy).unwrap()],
        )
        .await;
        let state = env.ts().state();
        let skipped = Some(json!({ "state": "skipped" }));

        env.run_sql(FAILED, &[job_id]).await;
        after_backup(state, job_id).await.unwrap();
        assert_eq!(env.retention_of(job_id).await, None);

        env.run_sql(DONE, &[job_id]).await;
        env.run_sql(
            "UPDATE snapshots SET incomplete = 1 WHERE id = $1",
            &[&snapshot],
        )
        .await;
        after_backup(state, job_id).await.unwrap();
        assert_eq!(env.retention_of(job_id).await, skipped);

        env.run_sql(DONE, &[job_id]).await;
        env.run_sql("DELETE FROM snapshots WHERE id = $1", &[&snapshot])
            .await;
        after_backup(state, job_id).await.unwrap();
        assert_eq!(env.retention_of(job_id).await, skipped);
        assert_eq!(env.count(PAIR_COUNT, &["p", &env.host]).await, 2);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_plan_matches_execution() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let data = env.retention_data("data");
        env.retention_policy("p", &data, "").await;
        let jobs = env.retention_runs("p", 4).await;
        let policy = env.stored_policy("p", 2).await;
        let rules = policy.spec.retention.clone().unwrap();
        let state = env.ts().state();

        let plan = plan_policy(state, &policy, &rules).await.unwrap();

        assert_eq!(plan.hosts.len(), 1);
        assert_eq!(plan.hosts[0].host_id, env.host);
        assert_eq!(plan.hosts[0].hostname, env.hostname);
        assert_eq!(plan.hosts[0].items.len(), 4);
        let mut doomed: Vec<String> = plan.hosts[0]
            .items
            .iter()
            .filter(|i| !i.keep)
            .map(|i| i.id.clone())
            .collect();
        doomed.sort();
        assert_eq!(doomed.len(), 2, "{}", backend.name());

        let before = env.catalog_ids().await;
        run_for_pair(state, &policy, &env.host, jobs[3]["id"].as_str().unwrap())
            .await
            .unwrap();
        let after = env.catalog_ids().await;
        let mut forgotten: Vec<String> = before
            .into_iter()
            .filter(|id| !after.contains(id))
            .collect();
        forgotten.sort();
        assert_eq!(forgotten, doomed, "{}", backend.name());
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_ignores_snapshots_not_linked_to_a_job() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let data = env.retention_data("data");
        env.retention_policy("p", &data, "  retention:\n    keepLast: 1\n")
            .await;
        let future = now_ms() + 3_600_000;
        for (n, id) in ["e", "f"].iter().enumerate() {
            env.insert_row_as(
                &id.repeat(64),
                Some(&env.host),
                Some("p"),
                future + n as i64,
                None,
            )
            .await;
        }

        let jobs = env.retention_runs("p", 3).await;

        let name = backend.name();
        let ids: Vec<String> = jobs.iter().map(snapshot_of).collect();
        let linked = "SELECT COUNT(*) AS n FROM snapshots WHERE job_id IS NOT NULL";
        assert_eq!(env.count(linked, &[]).await, 2, "{name}");
        let mut remaining = env.catalog_ids().await;
        remaining.retain(|id| id.as_str() != "e".repeat(64) && id.as_str() != "f".repeat(64));
        remaining.sort();
        let mut expected = ids[1..].to_vec();
        expected.sort();
        assert_eq!(remaining, expected, "{name}");
        assert_eq!(env.count(PAIR_COUNT, &["p", &env.host]).await, 4, "{name}");

        let policy = env.stored_policy("p", 1).await;
        let rules = policy.spec.retention.clone().unwrap();
        let plan = plan_policy(env.ts().state(), &policy, &rules)
            .await
            .unwrap();
        assert_eq!(plan.hosts[0].items.len(), 2, "{name}");
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_postponed_while_times_unconfirmed() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let data = env.retention_data("data");
        env.retention_policy("p", &data, "").await;
        let jobs = env.retention_runs("p", 3).await;
        let job_id = jobs[2]["id"].as_str().unwrap();
        let policy = env.stored_policy("p", 1).await;
        env.run_sql(
            "UPDATE policies SET spec_json = $1 WHERE name = 'p'",
            &[&serde_json::to_string(&policy).unwrap()],
        )
        .await;
        env.run_sql(
            "UPDATE snapshots SET received_exact = 0 WHERE id = $1",
            &[&snapshot_of(&jobs[0])],
        )
        .await;
        env.run_sql(
            "UPDATE jobs SET retention_json = NULL WHERE id = $1",
            &[job_id],
        )
        .await;
        let state = env.ts().state();

        after_backup(state, job_id).await.unwrap();

        let name = backend.name();
        assert_eq!(
            env.retention_of(job_id).await,
            Some(json!({ "state": "skipped" })),
            "{name}"
        );
        assert_eq!(env.catalog_ids().await.len(), 3, "{name}");
        assert!(
            env.event_kinds(job_id)
                .await
                .contains(&"retention_postponed".to_string()),
            "{name}"
        );
        let job = env.wait_job(job_id).await;
        assert_eq!(job["state"], "succeeded", "{name}");

        env.run_sql(
            "UPDATE snapshots SET received_exact = 1 WHERE received_exact = 0",
            &[],
        )
        .await;
        env.run_sql(
            "UPDATE jobs SET retention_json = NULL WHERE id = $1",
            &[job_id],
        )
        .await;
        after_backup(state, job_id).await.unwrap();
        assert_eq!(env.catalog_ids().await.len(), 2, "{name}");
        env.stop().await;
    }
}
