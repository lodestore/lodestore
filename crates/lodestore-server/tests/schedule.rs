mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{AgentEnv, Backend, TestServer, WAIT, backends, bearer_client, wait_until};
use lodestore_policy::jiff::{SignedDuration, Timestamp};
use lodestore_server::schedule::{Origin, jitter_offset, tick};
use serde_json::{Value, json};
use sqlx::{AnyPool, Row};

fn at(text: &str) -> Timestamp {
    text.parse().unwrap()
}

struct Fleet {
    ts: TestServer,
    env: AgentEnv,
    data: PathBuf,
}

impl Fleet {
    async fn start(backend: &Backend) -> Fleet {
        let ts = TestServer::start_with(backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        let env = AgentEnv::start_on(&ts, &repo, "main", "host-a").await;
        let data = env.work.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("file"), "content").unwrap();
        Fleet { ts, env, data }
    }

    async fn stop(self) {
        self.env.stop().await;
        self.ts.stop().await;
    }

    fn pool(&self) -> &AnyPool {
        &self.ts.state().db.pool
    }

    async fn policy(&self, name: &str, cron: &str, extra: &str) {
        let yaml = format!(
            "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: {name}\n\
             spec:\n  repository: main\n  selector:\n    matchLabels:\n      agent: host-a\n  \
             paths:\n    - {}\n  schedule:\n    cron: \"{cron}\"\n    jitter: 0m\n{extra}",
            self.data.display()
        );
        self.apply(name, &yaml, true).await;
    }

    async fn apply(&self, name: &str, yaml: &str, create: bool) {
        let response = if create {
            self.env
                .api
                .post("/api/v1/policies", &json!({ "yaml": yaml }))
                .await
        } else {
            let token = self.ts.issue_token(&["policies:write"]).await;
            bearer_client(&token)
                .put(format!("{}/api/v1/policies/{name}", self.ts.rest_url()))
                .json(&json!({ "yaml": yaml, "base_revision": 1 }))
                .send()
                .await
                .unwrap()
        };
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        assert!(status.is_success(), "{body}");
    }

    async fn jobs(&self, policy: &str) -> Vec<(String, String, Option<i64>)> {
        sqlx::query(
            "SELECT id, origin, missed_runs FROM jobs WHERE policy_name = $1 \
             ORDER BY queued_at, id",
        )
        .bind(policy)
        .fetch_all(self.pool())
        .await
        .unwrap()
        .iter()
        .map(|row| {
            (
                row.try_get("id").unwrap(),
                row.try_get("origin").unwrap(),
                row.try_get("missed_runs").unwrap(),
            )
        })
        .collect()
    }

    async fn cursor(&self, policy: &str) -> Option<i64> {
        sqlx::query(
            "SELECT s.last_slot_ms FROM schedule_state s JOIN policies p ON p.id = s.policy_id \
             WHERE p.name = $1",
        )
        .bind(policy)
        .fetch_optional(self.pool())
        .await
        .unwrap()
        .map(|row| row.try_get("last_slot_ms").unwrap())
    }

    async fn tick(&self, now: &str) -> Vec<lodestore_server::schedule::ScheduledJob> {
        tick(self.ts.state(), at(now)).await.unwrap().queued
    }

    async fn go_offline(&mut self) {
        self.env.stop_agent().await;
        let (state, host) = (self.ts.state(), self.env.host.clone());
        assert!(
            wait_until(WAIT, || async { !state.registry.is_online(&host) }).await,
            "the host stays online"
        );
    }

    async fn go_online(&mut self) {
        self.env.start_agent().await;
    }

    async fn finish(&self, job: &str) -> Value {
        let job = self.env.wait_job(job).await;
        assert!(
            job["state"] == "succeeded" || job["state"] == "warnings",
            "{job}"
        );
        job
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schedule_enqueues_due_slot() {
    for backend in backends() {
        let fleet = Fleet::start(&backend).await;
        fleet.policy("every-five", "*/5 * * * *", "").await;

        assert!(fleet.tick("2026-06-01T10:02:00Z").await.is_empty());
        assert_eq!(
            fleet.cursor("every-five").await,
            Some(at("2026-06-01T10:00:00Z").as_millisecond())
        );
        assert!(fleet.jobs("every-five").await.is_empty());

        let queued = fleet.tick("2026-06-01T10:05:10Z").await;
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].origin, Origin::Schedule);
        assert!(queued[0].created);
        fleet.finish(&queued[0].job_id).await;
        assert_eq!(
            fleet.jobs("every-five").await,
            vec![(queued[0].job_id.clone(), "schedule".to_string(), None)]
        );

        assert!(fleet.tick("2026-06-01T10:05:10Z").await.is_empty());
        assert_eq!(fleet.jobs("every-five").await.len(), 1);
        fleet.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schedule_schema() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        let pool = &ts.state().db.pool;
        sqlx::query(
            "INSERT INTO agents (id, hostname, repository_id, labels_json, os, arch, version, status, \
             enrolled_at, cert_serial, cert_issued_at, cert_not_after) \
             VALUES ('h1', 'h1', $1, '{}', 'linux', 'x86_64', 'test', 'active', 0, 'aa', 0, 0)",
        )
        .bind(&repo)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO policies (id, name, repository_id, yaml, spec_json, revision, created_at, updated_at) \
             VALUES ('p1', 'p1', $1, '', '{}', 1, 0, 0)",
        )
        .bind(&repo)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO schedule_state (policy_id, host_id, fingerprint, last_slot_ms, updated_at) \
             VALUES ('p1', 'h1', 'f', 1, 0)",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO schedule_state (policy_id, host_id, fingerprint, last_slot_ms, updated_at) \
             VALUES ('p1', 'h1', 'f', 2, 0)",
        )
        .execute(pool)
        .await
        .expect_err("one cursor per pair");

        sqlx::query(
            "INSERT INTO jobs (id, host_id, policy_id, source, state, spec, queued_at) \
             VALUES ('j1', 'h1', 'p1', 'manual', 'queued', $1, 0)",
        )
        .bind(Vec::<u8>::new())
        .execute(pool)
        .await
        .unwrap();
        let row = sqlx::query("SELECT origin, missed_runs FROM jobs WHERE id = 'j1'")
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(row.try_get::<String, _>("origin").unwrap(), "manual");
        assert_eq!(row.try_get::<Option<i64>, _>("missed_runs").unwrap(), None);

        sqlx::query("DELETE FROM jobs WHERE id = 'j1'")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM policies WHERE id = 'p1'")
            .execute(pool)
            .await
            .unwrap();
        let left: i64 = sqlx::query("SELECT COUNT(*) AS n FROM schedule_state")
            .fetch_one(pool)
            .await
            .unwrap()
            .try_get("n")
            .unwrap();
        assert_eq!(left, 0);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schedule_catch_up_after_offline() {
    for backend in backends() {
        let mut fleet = Fleet::start(&backend).await;
        fleet.policy("hourly", "0 * * * *", "").await;

        assert!(fleet.tick("2026-06-01T01:30:00Z").await.is_empty());
        fleet.go_offline().await;
        for now in [
            "2026-06-01T02:00:10Z",
            "2026-06-01T03:00:10Z",
            "2026-06-01T04:00:10Z",
        ] {
            assert!(fleet.tick(now).await.is_empty());
        }
        assert_eq!(
            fleet.cursor("hourly").await,
            Some(at("2026-06-01T01:00:00Z").as_millisecond())
        );
        assert!(fleet.jobs("hourly").await.is_empty());

        fleet.go_online().await;
        let queued = fleet.tick("2026-06-01T04:20:00Z").await;
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].origin, Origin::Catchup);
        fleet.finish(&queued[0].job_id).await;
        assert_eq!(
            fleet.jobs("hourly").await,
            vec![(queued[0].job_id.clone(), "catchup".to_string(), Some(3))]
        );
        let event =
            sqlx::query("SELECT level, message FROM job_events WHERE job_id = $1 AND seq = 0")
                .bind(&queued[0].job_id)
                .fetch_one(fleet.pool())
                .await
                .unwrap();
        assert_eq!(event.try_get::<String, _>("level").unwrap(), "info");
        assert_eq!(
            event.try_get::<String, _>("message").unwrap(),
            "Running one catch-up backup for 3 missed runs"
        );
        assert_eq!(
            fleet.cursor("hourly").await,
            Some(at("2026-06-01T04:00:00Z").as_millisecond())
        );
        fleet.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schedule_window_skips_old_slot() {
    for backend in backends() {
        let mut fleet = Fleet::start(&backend).await;
        fleet
            .policy("daily-wide", "@daily", "    catchUpWindow: 24h\n")
            .await;
        fleet
            .policy("daily-narrow", "@daily", "    catchUpWindow: 1h\n")
            .await;

        assert!(fleet.tick("2026-06-01T12:00:00Z").await.is_empty());
        fleet.go_offline().await;
        assert!(fleet.tick("2026-06-02T12:00:00Z").await.is_empty());
        fleet.go_online().await;

        let queued = fleet.tick("2026-06-03T06:00:00Z").await;
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].policy, "daily-wide");
        assert_eq!(queued[0].origin, Origin::Catchup);
        fleet.finish(&queued[0].job_id).await;
        assert_eq!(
            fleet.jobs("daily-wide").await[0].2,
            Some(2),
            "two midnights were missed"
        );
        assert!(fleet.jobs("daily-narrow").await.is_empty());
        assert_eq!(
            fleet.cursor("daily-narrow").await,
            Some(at("2026-06-03T00:00:00Z").as_millisecond())
        );
        fleet.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schedule_downtime_one_job_per_pair() {
    for backend in backends() {
        let fleet = Fleet::start(&backend).await;
        fleet.policy("every-five", "*/5 * * * *", "").await;

        assert!(fleet.tick("2026-06-01T10:02:00Z").await.is_empty());
        let queued = fleet.tick("2026-06-02T10:03:00Z").await;
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].origin, Origin::Catchup);
        fleet.finish(&queued[0].job_id).await;
        assert_eq!(fleet.jobs("every-five").await.len(), 1);
        assert_eq!(fleet.jobs("every-five").await[0].2, Some(288));
        assert!(fleet.tick("2026-06-02T10:03:30Z").await.is_empty());
        assert_eq!(fleet.jobs("every-five").await.len(), 1);
        fleet.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schedule_coalesces_with_queued_job() {
    for backend in backends() {
        let fleet = Fleet::start(&backend).await;
        fleet.policy("every-five", "*/5 * * * *", "").await;
        assert!(fleet.tick("2026-06-01T10:02:00Z").await.is_empty());

        sqlx::query(
            "INSERT INTO jobs (id, host_id, source, state, spec, queued_at) \
             VALUES ('blocker', $1, 'manual', 'running', $2, 0)",
        )
        .bind(&fleet.env.host)
        .bind(Vec::<u8>::new())
        .execute(fleet.pool())
        .await
        .unwrap();
        let response = fleet
            .env
            .api
            .post("/api/v1/policies/every-five/run", &json!({}))
            .await;
        let body: Value = response.json().await.unwrap();
        let manual = body["jobs"][0]["id"].as_str().unwrap().to_string();
        assert_eq!(body["jobs"][0]["created"], true);

        let queued = fleet.tick("2026-06-01T10:05:10Z").await;
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].job_id, manual);
        assert!(!queued[0].created);
        assert_eq!(
            fleet.jobs("every-five").await,
            vec![(manual.clone(), "manual".to_string(), None)]
        );
        assert_eq!(
            fleet.cursor("every-five").await,
            Some(at("2026-06-01T10:05:00Z").as_millisecond())
        );

        sqlx::query("UPDATE jobs SET state = 'succeeded' WHERE id = 'blocker'")
            .execute(fleet.pool())
            .await
            .unwrap();
        fleet.finish(&manual).await;
        fleet.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schedule_reset_on_change() {
    for backend in backends() {
        let fleet = Fleet::start(&backend).await;
        fleet.policy("nightly", "0 * * * *", "").await;
        assert!(fleet.tick("2026-06-01T10:30:00Z").await.is_empty());

        let yaml = format!(
            "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: nightly\n\
             spec:\n  repository: main\n  selector:\n    matchLabels:\n      agent: host-a\n  \
             paths:\n    - {}\n  schedule:\n    cron: \"*/5 * * * *\"\n    jitter: 0m\n",
            fleet.data.display()
        );
        fleet.apply("nightly", &yaml, false).await;

        assert!(fleet.tick("2026-06-01T10:30:30Z").await.is_empty());
        assert!(fleet.jobs("nightly").await.is_empty());
        assert_eq!(
            fleet.cursor("nightly").await,
            Some(at("2026-06-01T10:30:00Z").as_millisecond())
        );

        let queued = fleet.tick("2026-06-01T10:35:05Z").await;
        assert_eq!(queued.len(), 1);
        fleet.finish(&queued[0].job_id).await;
        fleet.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schedule_jitter_golden() {
    let fifteen = Duration::from_secs(15 * 60);
    let a = jitter_offset("agent-a", "nightly", fifteen);
    let b = jitter_offset("agent-b", "nightly", fifteen);
    assert_eq!(a, Duration::from_millis(760_818));
    assert_eq!(b, Duration::from_millis(418_501));
    assert_ne!(a, b);
    assert_eq!(jitter_offset("agent-a", "nightly", fifteen), a);
    assert_eq!(
        jitter_offset("agent-a", "nightly", Duration::ZERO),
        Duration::ZERO
    );

    for backend in backends() {
        let fleet = Fleet::start(&backend).await;
        let yaml = format!(
            "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: nightly\n\
             spec:\n  repository: main\n  selector:\n    matchLabels:\n      agent: host-a\n  \
             paths:\n    - {}\n  schedule:\n    cron: \"0 * * * *\"\n    jitter: 15m\n",
            fleet.data.display()
        );
        fleet.apply("nightly", &yaml, true).await;
        let offset =
            SignedDuration::try_from(jitter_offset(&fleet.env.host, "nightly", fifteen)).unwrap();
        let slot = at("2026-06-01T10:00:00Z");

        assert!(fleet.tick("2026-06-01T09:30:00Z").await.is_empty());
        let early = slot
            .checked_add(offset)
            .unwrap()
            .checked_sub(SignedDuration::from_secs(1))
            .unwrap();
        assert!(fleet.tick(&early.to_string()).await.is_empty());
        let due = slot
            .checked_add(offset)
            .unwrap()
            .checked_add(SignedDuration::from_secs(1))
            .unwrap();
        let queued = fleet.tick(&due.to_string()).await;
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].origin, Origin::Schedule);
        fleet.finish(&queued[0].job_id).await;
        fleet.stop().await;
    }
}

fn ticks(first: &str, step_minutes: i64, count: i64) -> Vec<String> {
    (0..count)
        .map(|n| {
            at(first)
                .checked_add(SignedDuration::from_mins(step_minutes * n))
                .unwrap()
                .to_string()
        })
        .collect()
}

async fn run_ticks(fleet: &Fleet, policy: &str, times: &[String]) -> Vec<i64> {
    let mut cursors = Vec::new();
    for now in times {
        let queued = fleet.tick(now).await;
        assert!(queued.len() <= 1, "{now}: {queued:?}");
        for job in &queued {
            fleet.finish(&job.job_id).await;
        }
        if let Some(cursor) = fleet.cursor(policy).await
            && cursors.last() != Some(&cursor)
        {
            assert!(cursors.last().is_none_or(|last| *last < cursor), "{now}");
            cursors.push(cursor);
        }
    }
    cursors
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schedule_dst_fall_back_one_job_per_slot() {
    for backend in backends() {
        let fleet = Fleet::start(&backend).await;
        fleet
            .policy("hourly-ny", "@hourly", "    timezone: America/New_York\n")
            .await;

        let mut times = ticks("2026-11-01T04:59:00Z", 15, 13);
        times.push("2026-11-01T08:01:00Z".to_string());
        let cursors = run_ticks(&fleet, "hourly-ny", &times).await;

        assert_eq!(
            cursors,
            ["04:00", "05:00", "07:00", "08:00"]
                .map(|time| at(&format!("2026-11-01T{time}:00Z")).as_millisecond())
        );
        assert_eq!(fleet.jobs("hourly-ny").await.len(), 3);
        fleet.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schedule_dst_spring_forward_single_job() {
    for backend in backends() {
        let fleet = Fleet::start(&backend).await;
        fleet
            .policy("daily-kyiv", "30 3 * * *", "    timezone: Europe/Kyiv\n")
            .await;

        let times = ticks("2026-03-29T00:59:00Z", 15, 3);
        let cursors = run_ticks(&fleet, "daily-kyiv", &times).await;

        assert_eq!(fleet.jobs("daily-kyiv").await.len(), 1);
        assert_eq!(
            cursors.last(),
            Some(&at("2026-03-29T01:00:00Z").as_millisecond())
        );
        fleet.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schedule_catch_up_event_listed() {
    for backend in backends() {
        let fleet = Fleet::start(&backend).await;
        fleet.policy("every-five", "*/5 * * * *", "").await;
        assert!(fleet.tick("2026-06-01T10:02:00Z").await.is_empty());
        let queued = fleet.tick("2026-06-01T10:30:00Z").await;
        assert_eq!(queued[0].origin, Origin::Catchup);
        let id = &queued[0].job_id;
        fleet.finish(id).await;

        let all = fleet.env.events(id, "").await;
        let catch_up: Vec<&Value> = all.iter().filter(|e| e["kind"] == "catch_up").collect();
        assert_eq!(catch_up.len(), 1, "{all:?}");
        assert_eq!(catch_up[0]["seq"], 0);
        assert_eq!(
            catch_up[0]["message"],
            "Running one catch-up backup for 6 missed runs"
        );

        let later = fleet.env.events(id, "?after=0").await;
        assert!(later.iter().all(|e| e["seq"].as_i64().unwrap() > 0));
        assert!(later.iter().all(|e| e["kind"] != "catch_up"));
        fleet.stop().await;
    }
}
