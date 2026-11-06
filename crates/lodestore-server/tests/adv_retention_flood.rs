//! A token that may start runs or edit policies must not be able to push a
//! policy's scheduled history out through retention. Only public paths of the
//! first release are used (REST, SQL reads, `schedule::tick`), so the
//! scenarios run unchanged against that release.

mod common;

use std::time::Duration;

use common::{AgentEnv, TestServer, backends, bearer_client, wait_until};
use lodestore_policy::jiff::Timestamp;
use lodestore_server::schedule::tick;
use serde_json::{Value, json};
use sqlx::Row;

const POLICY: &str = "keep-three";

fn yaml(data: &str, cron: &str) -> String {
    format!(
        "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: {POLICY}\n\
         spec:\n  repository: main\n  selector:\n    matchLabels:\n      agent: host-a\n  \
         paths:\n    - {data}\n  schedule:\n    cron: \"{cron}\"\n    jitter: 0m\n  \
         retention:\n    keepLast: 3\n"
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_run_now_flood_keeps_scheduled_history() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        let env = AgentEnv::start_on(&ts, &repo, "main", "host-a").await;
        let data = env.work.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("file"), "content").unwrap();
        let response = env
            .api
            .post(
                "/api/v1/policies",
                &json!({ "yaml": yaml(&data.display().to_string(), "*/5 * * * *") }),
            )
            .await;
        assert_eq!(response.status(), 201);

        let state = ts.state();
        let at = |text: &str| -> Timestamp { text.parse().unwrap() };
        let mut scheduled = Vec::new();
        tick(state, at("2026-06-01T10:02:00Z")).await.unwrap();
        for n in 1..=3 {
            let now = format!("2026-06-01T10:{:02}:10Z", 5 * n);
            let queued = tick(state, at(&now)).await.unwrap().queued;
            assert_eq!(queued.len(), 1, "slot {n}");
            let job = env.wait_job_settled(&queued[0].job_id).await;
            assert!(
                job["state"] == "succeeded" || job["state"] == "warnings",
                "{job}"
            );
            scheduled.push(job["snapshot_id"].as_str().unwrap().to_string());
        }

        let token = bearer_client(&ts.issue_token(&["jobs:run"]).await);
        let pool = &state.db.pool;
        let mut refused = 0;
        for n in 0..10 {
            let response = token
                .post(format!("{}/api/v1/policies/{POLICY}/run", ts.rest_url()))
                .json(&json!({}))
                .send()
                .await
                .unwrap();
            if response.status() == 429 {
                refused += 1;
                continue;
            }
            assert_eq!(response.status(), 200, "run {n}");
            let body: Value = response.json().await.unwrap();
            env.wait_job_settled(body["jobs"][0]["id"].as_str().unwrap())
                .await;
            let settled = wait_until(Duration::from_secs(60), || async {
                let pending: i64 = sqlx::query(
                    "SELECT COUNT(*) AS n FROM jobs WHERE source = 'manual' \
                     AND (retention_json IS NULL OR retention_json LIKE '%running%')",
                )
                .fetch_one(pool)
                .await
                .unwrap()
                .try_get("n")
                .unwrap();
                pending == 0
            })
            .await;
            assert!(settled, "[{}] retention never settled", backend.name());
        }
        assert_eq!(refused, 4, "[{}] 429 after the sixth run", backend.name());

        for id in &scheduled {
            let kept: i64 = sqlx::query("SELECT COUNT(*) AS n FROM snapshots WHERE id = $1")
                .bind(id)
                .fetch_one(pool)
                .await
                .unwrap()
                .try_get("n")
                .unwrap();
            assert_eq!(
                kept,
                1,
                "[{}] scheduled snapshot {id} was evicted",
                backend.name()
            );
        }
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_every_minute_cron_needs_snapshots_delete() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        ts.create_local_repository("main").await;
        let api = ts.api().await;
        let response = api
            .post(
                "/api/v1/policies",
                &json!({ "yaml": yaml("/etc", "0 3 * * *") }),
            )
            .await;
        assert_eq!(response.status(), 201);

        let token = bearer_client(&ts.issue_token(&["policies:read", "policies:write"]).await);
        let response = token
            .put(format!("{}/api/v1/policies/{POLICY}", ts.rest_url()))
            .json(&json!({ "yaml": yaml("/etc", "* * * * *"), "base_revision": 1 }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "[{}]", backend.name());
        let body: Value = response.json().await.unwrap();
        assert!(
            body["message"]
                .as_str()
                .unwrap()
                .contains("snapshots:delete"),
            "{body}"
        );
        ts.stop().await;
    }
}
