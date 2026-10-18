mod common;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use common::{AgentEnv, WAIT, backends};
use lodestore_server::catalog::anomaly::{Reason, ReasonKind, evaluate};
use serde_json::{Value, json};
use sqlx::Row;

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;
const BASE_TIME: i64 = 1_000_000;

fn id(n: u64) -> String {
    format!("{n:064x}")
}

fn stats(files: u64, changed: u64, bytes: u64, added: u64) -> Value {
    json!({
        "files_new": 0,
        "files_changed": changed,
        "files_unchanged": files - changed,
        "dirs": 1,
        "bytes_total": bytes,
        "bytes_read": added,
        "bytes_added": added,
    })
}

fn usual() -> Value {
    stats(1000, 20, 10 * GIB, 50 * MIB)
}

fn kinds(reasons: &[Reason]) -> Vec<ReasonKind> {
    reasons.iter().map(|r| r.kind).collect()
}

impl AgentEnv {
    async fn seed(&self, n: u64, paths: &str, stats: &Value, incomplete: bool) -> String {
        let id = id(n);
        sqlx::query(
            "INSERT INTO snapshots (id, repository_id, host_id, hostname, time_ms, paths_json, \
             tags_json, policy_name, job_id, incomplete, errors_total, warnings_total, \
             stats_json, discovered_at, received_at, received_exact) \
             SELECT $1, id, $2, 'inserted', $3, $4, '[]', 'p', 'seeded', $5, 0, 0, $6, $3, $3, 1 \
             FROM repositories WHERE name = 'main'",
        )
        .bind(&id)
        .bind(&self.host)
        .bind(BASE_TIME + n as i64)
        .bind(paths)
        .bind(i64::from(incomplete))
        .bind(stats.to_string())
        .execute(&self.ts().state().db.pool)
        .await
        .unwrap();
        id
    }

    async fn seed_base(&self, count: u64, stats: &Value) {
        for n in 1..=count {
            self.seed(n, r#"["/data"]"#, stats, false).await;
        }
    }

    async fn judge(&self, n: u64, stats: &Value) -> Option<Vec<Reason>> {
        let id = self.seed(100 + n, r#"["/data"]"#, stats, false).await;
        evaluate(self.ts().state(), &id).await.unwrap()
    }

    async fn mark_of(&self, id: &str) -> sqlx::any::AnyRow {
        sqlx::query(
            "SELECT suspicious, suspicious_by, suspicious_reason, anomaly_json, suspicious_at \
             FROM snapshots WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&self.ts().state().db.pool)
        .await
        .unwrap()
    }

    fn anomaly_data(&self, name: &str) -> PathBuf {
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

    async fn anomaly_policy(&self, name: &str, path: &Path, retention: &str) {
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

    async fn anomaly_run(&self, policy: &str) -> Value {
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
        let job_id = job["id"].as_str().unwrap();
        let deadline = std::time::Instant::now() + WAIT;
        loop {
            let row = sqlx::query("SELECT retention_json FROM jobs WHERE id = $1")
                .bind(job_id)
                .fetch_one(&self.ts().state().db.pool)
                .await
                .unwrap();
            let state: Option<String> = row.try_get("retention_json").unwrap();
            if state.is_some_and(|s| !s.contains("running")) {
                return job;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "retention of {job_id} stuck"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn seed_base_from(&self, template: &str, count: u64, stats: &Value) {
        for n in 1..=count {
            sqlx::query(
                "INSERT INTO snapshots (id, repository_id, host_id, hostname, time_ms, paths_json, \
                 tags_json, policy_name, job_id, incomplete, errors_total, warnings_total, \
                 stats_json, discovered_at, received_at, received_exact) \
                 SELECT $1, repository_id, host_id, hostname, time_ms, paths_json, tags_json, \
                 policy_name, 'seeded', 0, 0, 0, $2, discovered_at, received_at - $3, 1 \
                 FROM snapshots WHERE id = $4",
            )
            .bind(id(n))
            .bind(stats.to_string())
            .bind(60_000 * n as i64)
            .bind(template)
            .execute(&self.ts().state().db.pool)
            .await
            .unwrap();
        }
    }

    async fn ids(&self) -> Vec<String> {
        sqlx::query("SELECT id FROM snapshots ORDER BY received_at, id")
            .fetch_all(&self.ts().state().db.pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.try_get("id").unwrap())
            .collect()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn anomaly_flags_files_drop() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        env.seed_base(5, &usual()).await;

        let id = env
            .seed(
                9,
                r#"["/data"]"#,
                &stats(300, 10, 10 * GIB, 50 * MIB),
                false,
            )
            .await;
        let reasons = evaluate(env.ts().state(), &id).await.unwrap().unwrap();

        assert_eq!(kinds(&reasons), [ReasonKind::FilesDrop]);
        assert_eq!(
            (reasons[0].pct, reasons[0].from, reasons[0].to),
            (70, 1000, 300)
        );
        let row = env.mark_of(&id).await;
        assert_eq!(row.try_get::<i64, _>("suspicious").unwrap(), 1);
        assert_eq!(
            row.try_get::<Option<String>, _>("suspicious_by").unwrap(),
            Some("anomaly".to_string())
        );
        assert!(
            row.try_get::<Option<i64>, _>("suspicious_at")
                .unwrap()
                .is_some()
        );
        let stored: Value =
            serde_json::from_str(&row.try_get::<String, _>("anomaly_json").unwrap()).unwrap();
        assert_eq!(stored[0]["kind"], "files_drop");
        assert_eq!(stored[0]["pct"], 70);
        assert_eq!(stored[0]["from"], 1000);
        assert_eq!(stored[0]["to"], 300);
        assert_eq!(
            reasons[0].describe(),
            "70% fewer files than before (1000 to 300)."
        );
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn anomaly_flags_bytes_drop() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        env.seed_base(5, &usual()).await;

        let reasons = env
            .judge(1, &stats(1000, 20, 4 * GIB, 50 * MIB))
            .await
            .unwrap();

        assert_eq!(kinds(&reasons), [ReasonKind::BytesDrop]);
        assert_eq!(reasons[0].pct, 60);
        assert_eq!(
            reasons[0].describe(),
            "60% less data than before (10.0 GiB to 4.0 GiB)."
        );
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn anomaly_flags_change_spike() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        env.seed_base(2, &usual()).await;
        assert!(
            env.judge(1, &stats(1000, 600, 10 * GIB, 50 * MIB))
                .await
                .is_none(),
            "{}",
            backend.name()
        );
        env.seed(3, r#"["/data"]"#, &usual(), false).await;
        env.seed(4, r#"["/data"]"#, &usual(), false).await;

        let reasons = env
            .judge(2, &stats(1000, 600, 10 * GIB, 50 * MIB))
            .await
            .unwrap();

        assert_eq!(kinds(&reasons), [ReasonKind::ChangedSpike]);
        assert_eq!(reasons[0].pct, 60);
        assert!((reasons[0].factor - 30.0).abs() < 0.5, "{reasons:?}");
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn anomaly_flags_added_spike() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        env.seed_base(5, &usual()).await;

        let reasons = env
            .judge(1, &stats(1000, 20, 10 * GIB, 5 * GIB))
            .await
            .unwrap();

        assert_eq!(kinds(&reasons), [ReasonKind::AddedSpike]);
        assert_eq!(reasons[0].pct, 50);
        assert_eq!(
            reasons[0].describe(),
            "5.0 GiB of new data, about 102 times the usual."
        );
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn anomaly_needs_baseline() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let tiny = stats(1, 0, 10, 10);
        assert!(env.judge(1, &tiny).await.is_none(), "{}", backend.name());

        env.seed(1, r#"["/data"]"#, &usual(), true).await;
        let marked = env.seed(2, r#"["/data"]"#, &usual(), false).await;
        sqlx::query("UPDATE snapshots SET suspicious = 1, suspicious_by = 'anomaly' WHERE id = $1")
            .bind(&marked)
            .execute(&env.ts().state().db.pool)
            .await
            .unwrap();
        assert!(env.judge(2, &tiny).await.is_none(), "{}", backend.name());
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn anomaly_ignores_path_change() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        env.seed_base(5, &usual()).await;

        let id = env
            .seed(9, r#"["/other"]"#, &stats(10, 0, GIB / 100, MIB), false)
            .await;

        assert!(evaluate(env.ts().state(), &id).await.unwrap().is_none());
        assert_eq!(
            env.mark_of(&id)
                .await
                .try_get::<i64, _>("suspicious")
                .unwrap(),
            0
        );
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn anomaly_ignores_incomplete() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        env.seed_base(5, &usual()).await;

        let id = env
            .seed(9, r#"["/data"]"#, &stats(10, 0, GIB / 100, MIB), true)
            .await;

        assert!(evaluate(env.ts().state(), &id).await.unwrap().is_none());
        assert_eq!(
            env.mark_of(&id)
                .await
                .try_get::<i64, _>("suspicious")
                .unwrap(),
            0
        );
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn anomaly_zero_median_floor() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        env.seed_base(5, &stats(1000, 0, 10 * GIB, 50 * MIB)).await;

        assert!(
            env.judge(1, &stats(1000, 100, 10 * GIB, 50 * MIB))
                .await
                .is_none(),
            "{}",
            backend.name()
        );
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn anomaly_keeps_admin_mark() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        env.seed_base(5, &usual()).await;
        let marked = env
            .seed(9, r#"["/data"]"#, &stats(10, 0, GIB / 100, MIB), false)
            .await;
        sqlx::query(
            "UPDATE snapshots SET suspicious = 1, suspicious_by = 'admin', \
             suspicious_reason = 'looks wrong' WHERE id = $1",
        )
        .bind(&marked)
        .execute(&env.ts().state().db.pool)
        .await
        .unwrap();
        let cleared = env
            .seed(10, r#"["/data"]"#, &stats(10, 0, GIB / 100, MIB), false)
            .await;
        sqlx::query("UPDATE snapshots SET suspicious_cleared_at = 1 WHERE id = $1")
            .bind(&cleared)
            .execute(&env.ts().state().db.pool)
            .await
            .unwrap();

        assert!(evaluate(env.ts().state(), &marked).await.unwrap().is_none());
        assert!(
            evaluate(env.ts().state(), &cleared)
                .await
                .unwrap()
                .is_none()
        );

        let row = env.mark_of(&marked).await;
        assert_eq!(row.try_get::<i64, _>("suspicious").unwrap(), 1);
        assert_eq!(
            row.try_get::<Option<String>, _>("suspicious_by").unwrap(),
            Some("admin".to_string())
        );
        assert_eq!(
            row.try_get::<Option<String>, _>("suspicious_reason")
                .unwrap(),
            Some("looks wrong".to_string())
        );
        assert_eq!(
            row.try_get::<Option<String>, _>("anomaly_json").unwrap(),
            None
        );
        assert_eq!(
            env.mark_of(&cleared)
                .await
                .try_get::<i64, _>("suspicious")
                .unwrap(),
            0
        );
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn anomaly_suspicious_not_counted_by_retention() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let data = env.anomaly_data("data");
        env.anomaly_policy("p", &data, "  retention:\n    keepLast: 1\n")
            .await;
        let first = env.anomaly_run("p").await;
        let first_id = first["snapshot_id"].as_str().unwrap().to_string();
        env.seed_base_from(&first_id, 5, &usual()).await;

        let second = env.anomaly_run("p").await;

        let second_id = second["snapshot_id"].as_str().unwrap().to_string();
        let mut remaining = env.ids().await;
        remaining.sort();
        let mut expected = vec![second_id.clone(), first_id, id(1)];
        expected.sort();
        assert_eq!(remaining, expected, "{}", backend.name());
        let row = env.mark_of(&second_id).await;
        assert_eq!(row.try_get::<i64, _>("suspicious").unwrap(), 1);
        let events = env.events(second["id"].as_str().unwrap(), "").await;
        let warning = events
            .iter()
            .find(|e| e["kind"] == "suspicious_snapshot")
            .unwrap();
        assert_eq!(warning["level"], "warning");
        let message = warning["message"].as_str().unwrap();
        assert!(message.starts_with("Snapshot looks unusual: "), "{message}");
        assert!(
            message.ends_with(". Retention will not count it"),
            "{message}"
        );
        assert_eq!(second["state"], "succeeded");
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn anomaly_runs_without_retention() {
    for backend in backends() {
        let env = AgentEnv::start(&backend).await;
        let data = env.anomaly_data("data");
        env.anomaly_policy("p", &data, "").await;
        let first = env.anomaly_run("p").await;
        env.seed_base_from(first["snapshot_id"].as_str().unwrap(), 5, &usual())
            .await;

        let second = env.anomaly_run("p").await;

        let second_id = second["snapshot_id"].as_str().unwrap();
        let row = env.mark_of(second_id).await;
        assert_eq!(row.try_get::<i64, _>("suspicious").unwrap(), 1);
        assert_eq!(
            row.try_get::<Option<String>, _>("suspicious_by").unwrap(),
            Some("anomaly".to_string())
        );
        assert_eq!(env.ids().await.len(), 7);
        let retention = sqlx::query("SELECT retention_json FROM jobs WHERE id = $1")
            .bind(second["id"].as_str().unwrap())
            .fetch_one(&env.ts().state().db.pool)
            .await
            .unwrap()
            .try_get::<Option<String>, _>("retention_json")
            .unwrap();
        assert_eq!(retention, Some(r#"{"state":"skipped"}"#.to_string()));
        let events = env.events(second["id"].as_str().unwrap(), "").await;
        assert!(events.iter().any(|e| e["kind"] == "suspicious_snapshot"));
        env.stop().await;
    }
}
