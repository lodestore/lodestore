mod common;

use common::{Api, TestServer, backends, bearer_client};
use lodestore_server::alerts;
use lodestore_server::db::now_ms;
use serde_json::{Value, json};
use sqlx::{AnyPool, Row};

const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;
const DAILY: &str = "  schedule:\n    cron: \"0 2 * * *\"\n    jitter: 0m\n";
const STATS: &str = r#"{"bytes_total": 5000, "bytes_added": 700}"#;

struct Env {
    ts: TestServer,
    api: Api,
    repo: String,
}

impl Env {
    async fn start(backend: &common::Backend) -> Env {
        let ts = TestServer::start_with(backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        let api = ts.api().await;
        Env { ts, api, repo }
    }

    fn pool(&self) -> &AnyPool {
        &self.ts.state().db.pool
    }

    fn started(&self) -> i64 {
        self.ts.state().started_at_ms
    }

    async fn tick(&self, now_ms: i64) -> alerts::reconcile::Reconciled {
        alerts::tick(self.ts.state(), now_ms).await.unwrap()
    }

    async fn agent(&self, id: &str, hostname: &str, last_seen_ms: i64, cert_not_after_ms: i64) {
        sqlx::query(
            "INSERT INTO agents (id, hostname, repository_id, labels_json, os, arch, version, \
             status, enrolled_at, cert_serial, cert_issued_at, cert_not_after, last_seen_at) \
             VALUES ($1, $2, $3, '[]', 'linux', 'x86_64', '0.1.0', 'active', $4, 'serial', $4, $5, $6)",
        )
        .bind(id)
        .bind(hostname)
        .bind(&self.repo)
        .bind(now_ms() - 30 * DAY)
        .bind(cert_not_after_ms)
        .bind(last_seen_ms)
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn host(&self, id: &str, hostname: &str) {
        self.agent(id, hostname, now_ms() + 200 * HOUR, now_ms() + 300 * DAY)
            .await;
    }

    async fn policy(&self, name: &str, host: &str, extra: &str) {
        let yaml = format!(
            "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: {name}\n\
             spec:\n  repository: main\n  selector:\n    hosts: [{host}]\n  paths: [/etc]\n{extra}"
        );
        let policy = lodestore_policy::parse_yaml(&yaml).unwrap();
        sqlx::query(
            "INSERT INTO policies (id, name, repository_id, yaml, spec_json, revision, \
             created_at, updated_at) VALUES ($1, $1, $2, $3, $4, 1, $5, $5)",
        )
        .bind(name)
        .bind(&self.repo)
        .bind(&yaml)
        .bind(serde_json::to_string(&policy).unwrap())
        .bind(now_ms() - 30 * DAY)
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn snapshot(&self, id: &str, host: &str, policy: &str, received_ms: i64) {
        sqlx::query(
            "INSERT INTO snapshots (id, repository_id, host_id, hostname, time_ms, paths_json, \
             tags_json, policy_name, incomplete, errors_total, warnings_total, stats_json, \
             discovered_at, received_at, received_exact, verified_at, verified_by) \
             VALUES ($1, $2, $3, 'h', $4, '[]', '[]', $5, 0, 0, 0, $6, $4, $4, 1, $4, 'server')",
        )
        .bind(id)
        .bind(&self.repo)
        .bind(host)
        .bind(received_ms)
        .bind(policy)
        .bind(STATS)
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn failed_job(&self, id: &str, host: &str, policy: &str, finished_ms: i64, error: &str) {
        sqlx::query(
            "INSERT INTO jobs (id, host_id, policy_name, source, state, spec, queued_at, \
             finished_at, error_message) VALUES ($1, $2, $3, 'schedule', 'failed', $4, $5, $5, $6)",
        )
        .bind(id)
        .bind(host)
        .bind(policy)
        .bind(Vec::<u8>::new())
        .bind(finished_ms)
        .bind(error)
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn alerts(&self, state: &str) -> Vec<Value> {
        let response = self.api.get(&format!("/api/v1/alerts?state={state}")).await;
        assert_eq!(response.status().as_u16(), 200);
        let body: Value = response.json().await.unwrap();
        body["alerts"].as_array().unwrap().clone()
    }

    async fn alert_id(&self, kind: &str) -> String {
        sqlx::query("SELECT id FROM alerts WHERE type = $1 ORDER BY started_at DESC, id DESC")
            .bind(kind)
            .fetch_one(self.pool())
            .await
            .unwrap()
            .try_get("id")
            .unwrap()
    }

    async fn events(&self, alert_id: &str) -> Vec<String> {
        sqlx::query("SELECT kind FROM alert_events WHERE alert_id = $1 ORDER BY at, id")
            .bind(alert_id)
            .fetch_all(self.pool())
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get("kind").unwrap())
            .collect()
    }

    async fn event_count(&self) -> i64 {
        sqlx::query("SELECT COUNT(*) AS n FROM alert_events")
            .fetch_one(self.pool())
            .await
            .unwrap()
            .try_get("n")
            .unwrap()
    }

    async fn notified_at(&self, alert_id: &str) -> Option<i64> {
        sqlx::query("SELECT notified_at FROM alerts WHERE id = $1")
            .bind(alert_id)
            .fetch_one(self.pool())
            .await
            .unwrap()
            .try_get("notified_at")
            .unwrap()
    }

    async fn put_silence(&self, host: &str) -> u16 {
        reqwest::Client::new()
            .put(format!(
                "{}/api/v1/hosts/{host}/silence",
                self.ts.rest_url()
            ))
            .header("cookie", self.api.cookie())
            .header("x-csrf-token", &self.api.csrf)
            .json(&json!({ "duration": "day" }))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    async fn resume(&self, host: &str) {
        let client = bearer_client(&self.ts.issue_token(&["alerts:write"]).await);
        let response = client
            .delete(format!(
                "{}/api/v1/hosts/{host}/silence",
                self.ts.rest_url()
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 204);
    }

    async fn silence_row(&self, host: &str, until_ms: Option<i64>) {
        sqlx::query(
            "INSERT INTO alert_silences (host_id, until_ms, created_by, created_at) \
             VALUES ($1, $2, 'admin', 0)",
        )
        .bind(host)
        .bind(until_ms)
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn agent_update(
        &self,
        id: &str,
        host: &str,
        state: &str,
        target: &str,
        reason: Option<&str>,
        finished_ms: i64,
    ) {
        sqlx::query(
            "INSERT INTO agent_rollouts (id, target_version, state, canary, wave_size, \
             pause_after, created_by, created_at) VALUES ($1, $2, 'done', 1, 10, 0, 'admin', $3)",
        )
        .bind(format!("r-{id}"))
        .bind(target)
        .bind(finished_ms)
        .execute(self.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO agent_updates (id, rollout_id, host_id, arch, from_version, \
             target_version, wave, state, reason, queued_at, finished_at) \
             VALUES ($1, $2, $3, 'x86_64', '0.1.0', $4, 0, $5, $6, $7, $7)",
        )
        .bind(id)
        .bind(format!("r-{id}"))
        .bind(host)
        .bind(target)
        .bind(state)
        .bind(reason)
        .bind(finished_ms)
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn check_run(
        &self,
        id: &str,
        level: &str,
        state: &str,
        findings: i64,
        error: Option<&str>,
        finished_ms: i64,
    ) {
        let outcome = match (state, findings) {
            ("done", _) => Some("passed"),
            ("failed", 0) => Some("stopped"),
            ("failed", _) => Some("problems"),
            _ => None,
        };
        sqlx::query(
            "INSERT INTO check_runs (id, repository_id, level, origin, started_by, state, \
             outcome, findings_total, error, created_at, finished_at, report_json) \
             VALUES ($1, $2, $3, 'schedule', 'schedule', $4, $5, $6, $7, $8, $8, '{}')",
        )
        .bind(id)
        .bind(&self.repo)
        .bind(level)
        .bind(state)
        .bind(outcome)
        .bind(findings)
        .bind(error)
        .bind(finished_ms)
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn stop(self) {
        self.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_backup_failed_coalesces() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        env.host("h1", "web-1").await;
        env.policy("daily", "h1", "").await;
        let now = now_ms();
        env.failed_job("j1", "h1", "daily", now - HOUR, "disk full")
            .await;

        let first = env.tick(now).await;
        assert_eq!(first.fired, 1, "[{}]", backend.name());
        let alerts = env.alerts("active").await;
        assert_eq!(alerts.len(), 1);
        let alert = &alerts[0];
        assert_eq!(alert["type"], "backup_failed");
        assert_eq!(alert["severity"], "critical");
        assert_eq!(alert["title"], "Backup failed on web-1");
        assert_eq!(alert["detail"], "Policy daily: disk full.");
        assert_eq!(alert["policy"], "daily");
        assert_eq!(alert["facts"]["error"], "disk full");
        let id = alert["id"].as_str().unwrap().to_string();
        assert_eq!(env.events(&id).await, ["firing"]);

        env.failed_job("j2", "h1", "daily", now - 30 * MINUTE, "disk still full")
            .await;
        let second = env.tick(now + MINUTE).await;
        assert_eq!(second, Default::default());
        let alerts = env.alerts("active").await;
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0]["id"], id.as_str());
        assert_eq!(alerts[0]["facts"]["error"], "disk still full");
        assert_eq!(alerts[0]["detail"], "Policy daily: disk still full.");
        assert_eq!(env.events(&id).await, ["firing"]);

        env.snapshot("s1", "h1", "daily", now).await;
        let third = env.tick(now + 2 * MINUTE).await;
        assert_eq!(third.resolved, 1);
        assert!(env.alerts("active").await.is_empty());
        let resolved = env.alerts("resolved").await;
        assert_eq!(resolved[0]["id"], id.as_str());
        assert_eq!(env.events(&id).await, ["firing", "resolved"]);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_unreadable_policy_keeps_its_alerts() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        env.host("h1", "web-1").await;
        env.policy("daily", "h1", "").await;
        let now = now_ms();
        env.failed_job("j1", "h1", "daily", now - HOUR, "disk full")
            .await;
        assert_eq!(env.tick(now).await.fired, 1, "[{}]", backend.name());
        let id = env.alert_id("backup_failed").await;

        sqlx::query("UPDATE policies SET spec_json = 'not json' WHERE name = 'daily'")
            .execute(env.pool())
            .await
            .unwrap();
        let out = env.tick(now + MINUTE).await;
        assert_eq!(out, Default::default());
        assert_eq!(env.alerts("active").await.len(), 1);

        let spec = lodestore_policy::parse_yaml(
            "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: daily\n\
             spec:\n  repository: main\n  selector:\n    hosts: [h1]\n  paths: [/etc]\n",
        )
        .unwrap();
        sqlx::query("UPDATE policies SET spec_json = $1 WHERE name = 'daily'")
            .bind(serde_json::to_string(&spec).unwrap())
            .execute(env.pool())
            .await
            .unwrap();
        let out = env.tick(now + 2 * MINUTE).await;
        assert_eq!(out, Default::default());
        assert_eq!(env.events(&id).await, ["firing"]);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_no_recent_backup() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        env.host("h1", "web-1").await;
        env.policy("daily", "h1", DAILY).await;
        let now = now_ms();
        let success = now - 3 * DAY;
        env.snapshot("s1", "h1", "daily", success).await;

        env.tick(now).await;
        let alerts = env.alerts("active").await;
        assert_eq!(alerts.len(), 1, "[{}]", backend.name());
        let alert = &alerts[0];
        assert_eq!(alert["type"], "no_recent_backup");
        assert_eq!(alert["severity"], "warning");
        assert_eq!(alert["title"], "No recent backup on web-1");
        assert!(
            alert["detail"]
                .as_str()
                .unwrap()
                .ends_with("Expected at least every 2 days."),
            "{alert}"
        );
        assert_eq!(alert["facts"]["last_success_at"], success);
        assert_eq!(alert["facts"]["limit_secs"], 2 * 86_400);

        env.snapshot("s2", "h1", "daily", now).await;
        let resolved = env.tick(now + MINUTE).await;
        assert_eq!(resolved.resolved, 1);
        assert!(env.alerts("active").await.is_empty());
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_agent_offline_threshold() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        let now = env.started() + HOUR;
        let cert = now_ms() + 300 * DAY;
        env.agent("h1", "web-1", now - 16 * MINUTE, cert).await;
        env.agent("h2", "web-2", now - 10 * MINUTE, cert).await;

        env.tick(now).await;
        let alerts = env.alerts("active").await;
        assert_eq!(alerts.len(), 1, "[{}]", backend.name());
        assert_eq!(alerts[0]["type"], "agent_offline");
        assert_eq!(alerts[0]["title"], "web-1 is offline");
        assert_eq!(alerts[0]["host"]["id"], "h1");
        assert_eq!(alerts[0]["facts"]["last_seen_at"], now - 16 * MINUTE);
        assert!(
            alerts[0]["detail"]
                .as_str()
                .unwrap()
                .starts_with("Last contact ")
        );

        sqlx::query("UPDATE agents SET last_seen_at = $1 WHERE id = 'h1'")
            .bind(now + MINUTE)
            .execute(env.pool())
            .await
            .unwrap();
        let back = env.tick(now + 2 * MINUTE).await;
        assert_eq!(back.resolved, 1);
        assert!(env.alerts("active").await.is_empty());
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_restart_grace() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        let started = env.started();
        env.agent("h1", "web-1", started - DAY, now_ms() + 300 * DAY)
            .await;

        env.tick(started + MINUTE).await;
        assert!(
            env.alerts("active").await.is_empty(),
            "[{}]",
            backend.name()
        );
        env.tick(started + 16 * MINUTE).await;
        let alerts = env.alerts("active").await;
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0]["type"], "agent_offline");
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_cert_expiring_agent_and_grpc() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        let now = now_ms();
        env.agent("h1", "web-1", now, now + 10 * DAY).await;
        env.agent("h2", "web-2", now, now - HOUR).await;
        env.agent("h3", "web-3", now, now + 100 * DAY).await;
        env.ts
            .state()
            .pki
            .set_grpc_not_after(now / 1000 + 3 * 86_400);

        env.tick(now).await;
        let alerts = env.alerts("active").await;
        assert_eq!(alerts.len(), 3, "{alerts:?} [{}]", backend.name());
        let find = |title: &str| {
            alerts
                .iter()
                .find(|a| a["title"] == title)
                .unwrap_or_else(|| panic!("no alert {title}: {alerts:?}"))
        };
        let soon = find("Agent certificate on web-1 expires in 10 days");
        assert_eq!(soon["cert"], "agent");
        assert_eq!(soon["severity"], "warning");
        assert_eq!(soon["host"]["id"], "h1");
        assert_eq!(soon["detail"], "The agent is offline and cannot renew it.");
        let expired = find("Agent certificate on web-2 has expired");
        assert_eq!(expired["severity"], "critical");
        let grpc = find("Server gRPC certificate expires in 3 days");
        assert_eq!(grpc["cert"], "grpc");
        assert_eq!(grpc["host"], Value::Null);
        assert_eq!(grpc["facts"]["external"], false);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_reminder_every_24h() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        env.host("h1", "web-1").await;
        env.policy("daily", "h1", "").await;
        let t0 = now_ms();
        env.failed_job("j1", "h1", "daily", t0 - HOUR, "boom").await;

        assert_eq!(env.tick(t0).await.fired, 1, "[{}]", backend.name());
        let id = env.alert_id("backup_failed").await;
        assert_eq!(env.tick(t0 + HOUR).await, Default::default());
        assert_eq!(env.events(&id).await, ["firing"]);

        let reminder = env.tick(t0 + 25 * HOUR).await;
        assert_eq!(reminder.reminded, 1);
        assert_eq!(env.events(&id).await, ["firing", "reminder"]);
        env.tick(t0 + 48 * HOUR).await;
        assert_eq!(env.events(&id).await, ["firing", "reminder"]);

        env.snapshot("s1", "h1", "daily", t0 + 48 * HOUR + MINUTE)
            .await;
        env.tick(t0 + 49 * HOUR).await;
        env.tick(t0 + 50 * HOUR).await;
        assert_eq!(env.events(&id).await, ["firing", "reminder", "resolved"]);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_revoked_host_resolves() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        env.host("h1", "web-1").await;
        env.policy("daily", "h1", "").await;
        let now = now_ms();
        env.failed_job("j1", "h1", "daily", now - HOUR, "boom")
            .await;
        env.tick(now).await;
        assert_eq!(env.alerts("active").await.len(), 1, "[{}]", backend.name());

        sqlx::query("UPDATE agents SET status = 'revoked' WHERE id = 'h1'")
            .execute(env.pool())
            .await
            .unwrap();
        let after = env.tick(now + MINUTE).await;
        assert_eq!(after.resolved, 1);
        assert!(env.alerts("active").await.is_empty());
        let id = env.alert_id("backup_failed").await;
        assert_eq!(env.events(&id).await, ["firing", "resolved"]);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_silence_needs_alerts_write() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        env.host("h1", "web-1").await;
        let url = format!("{}/api/v1/hosts/h1/silence", env.ts.rest_url());
        let silence = |token: String| {
            let url = url.clone();
            async move {
                let client = bearer_client(&token);
                let put = client
                    .put(&url)
                    .json(&json!({ "duration": "day" }))
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16();
                let delete = client.delete(&url).send().await.unwrap().status().as_u16();
                (put, delete)
            }
        };

        let hosts = env.ts.issue_token(&["hosts:write"]).await;
        assert_eq!(silence(hosts).await, (403, 403), "[{}]", backend.name());
        let alerts = env.ts.issue_token(&["alerts:write"]).await;
        assert_eq!(silence(alerts).await, (200, 204), "[{}]", backend.name());
        assert_eq!(env.put_silence("h1").await, 200);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_silence_suppresses_events() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        env.host("h1", "web-1").await;
        env.policy("daily", "h1", "").await;
        let now = now_ms();
        env.failed_job("j1", "h1", "daily", now - HOUR, "boom")
            .await;
        assert_eq!(env.put_silence("h1").await, 200, "[{}]", backend.name());

        let out = env.tick(now).await;
        assert_eq!(out, Default::default());
        let alerts = env.alerts("active").await;
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0]["silenced"], true);
        assert_eq!(env.event_count().await, 0);
        let id = env.alert_id("backup_failed").await;
        assert_eq!(env.notified_at(&id).await, None);

        env.tick(now + 20 * HOUR).await;
        assert_eq!(env.event_count().await, 0);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_silence_resume_notifies() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        env.host("h1", "web-1").await;
        env.policy("p1", "h1", "").await;
        env.policy("p2", "h1", "").await;
        let now = now_ms();
        env.failed_job("j1", "h1", "p1", now - HOUR, "one").await;
        env.failed_job("j2", "h1", "p2", now - HOUR, "two").await;
        assert_eq!(env.put_silence("h1").await, 200, "[{}]", backend.name());
        env.tick(now).await;
        assert_eq!(env.event_count().await, 0);

        env.snapshot("s1", "h1", "p2", now).await;
        env.tick(now + MINUTE).await;
        assert_eq!(env.alerts("active").await.len(), 1);
        assert_eq!(env.event_count().await, 0);

        env.resume("h1").await;
        let out = env.tick(now + 2 * MINUTE).await;
        assert_eq!(out.fired, 1);
        let active = env.alerts("active").await;
        assert_eq!(active.len(), 1);
        assert_eq!(active[0]["silenced"], false);
        let id = active[0]["id"].as_str().unwrap();
        assert_eq!(env.events(id).await, ["firing"]);
        assert_eq!(env.event_count().await, 1);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_silence_expired_notifies() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        env.host("h1", "web-1").await;
        env.policy("daily", "h1", "").await;
        let now = now_ms();
        env.failed_job("j1", "h1", "daily", now - HOUR, "boom")
            .await;
        env.silence_row("h1", Some(now + HOUR)).await;

        env.tick(now).await;
        assert_eq!(env.event_count().await, 0, "[{}]", backend.name());
        let out = env.tick(now + 2 * HOUR).await;
        assert_eq!(out.fired, 1);
        let id = env.alert_id("backup_failed").await;
        assert_eq!(env.events(&id).await, ["firing"]);
        assert_eq!(env.notified_at(&id).await, Some(now + 2 * HOUR));
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_server_cert_not_silenced() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        env.host("h1", "web-1").await;
        env.host("h2", "web-2").await;
        env.silence_row("h1", None).await;
        env.silence_row("h2", None).await;
        let now = now_ms();
        env.ts
            .state()
            .pki
            .set_grpc_not_after(now / 1000 + 3 * 86_400);

        let out = env.tick(now).await;
        assert_eq!(out.fired, 1, "[{}]", backend.name());
        let id = env.alert_id("certificate_expiring").await;
        assert_eq!(env.events(&id).await, ["firing"]);
        assert!(env.notified_at(&id).await.is_some());
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_check_failed_raises_and_resolves() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        let now = now_ms();
        env.check_run("c1", "packs", "failed", 3, None, now - HOUR)
            .await;

        let first = env.tick(now).await;
        assert_eq!(first.fired, 1, "[{}]", backend.name());
        let alerts = env.alerts("active").await;
        assert_eq!(alerts.len(), 1);
        let alert = &alerts[0];
        assert_eq!(alert["type"], "check_failed");
        assert_eq!(alert["severity"], "critical");
        assert_eq!(alert["title"], "Pack reading check failed on main");
        assert_eq!(alert["check_level"], "packs");
        assert_eq!(alert["repository"]["id"], env.repo.as_str());
        assert_eq!(alert["repository"]["name"], "main");
        assert_eq!(alert["facts"]["findings"], 3);
        let id = alert["id"].as_str().unwrap().to_string();
        assert_eq!(env.events(&id).await, ["firing"]);
        let key: String = sqlx::query("SELECT key FROM alerts WHERE id = $1")
            .bind(&id)
            .fetch_one(env.pool())
            .await
            .unwrap()
            .get("key");
        assert_eq!(key, format!("check_failed:{}:packs", env.repo));

        assert_eq!(env.tick(now + MINUTE).await, Default::default());

        env.check_run("c2", "packs", "done", 0, None, now + HOUR)
            .await;
        let third = env.tick(now + HOUR + MINUTE).await;
        assert_eq!(third.resolved, 1);
        assert!(env.alerts("active").await.is_empty());
        assert_eq!(env.events(&id).await, ["firing", "resolved"]);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_snapshot_quarantined_fires_and_resolves() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        let now = now_ms();
        sqlx::query(
            "INSERT INTO gc_quarantine (repository_id, snapshot_id, object_key, reason, \
             confirmable, detected_at) VALUES ($1, 's1', 'snapshots/ab', 'decryption failed', 1, $2)",
        )
        .bind(&env.repo)
        .bind(now)
        .execute(env.pool())
        .await
        .unwrap();

        assert_eq!(env.tick(now).await.fired, 1, "[{}]", backend.name());
        let alerts = env.alerts("active").await;
        assert_eq!(alerts.len(), 1);
        let alert = &alerts[0];
        assert_eq!(alert["type"], "snapshot_quarantined");
        assert_eq!(alert["severity"], "critical");
        assert_eq!(alert["repository"]["id"], env.repo.as_str());
        assert_eq!(alert["facts"]["findings"], 1);
        assert_eq!(alert["facts"]["error"], "snapshots/ab: decryption failed");
        let id = alert["id"].as_str().unwrap().to_string();
        let key: String = sqlx::query("SELECT key FROM alerts WHERE id = $1")
            .bind(&id)
            .fetch_one(env.pool())
            .await
            .unwrap()
            .get("key");
        assert_eq!(key, format!("snapshot_quarantined:{}", env.repo));
        assert_eq!(env.tick(now + MINUTE).await, Default::default());

        sqlx::query("UPDATE gc_quarantine SET confirmed_at = $1, confirmed_by = 'admin'")
            .bind(now + HOUR)
            .execute(env.pool())
            .await
            .unwrap();
        assert_eq!(env.tick(now + HOUR + MINUTE).await.resolved, 1);
        assert!(env.alerts("active").await.is_empty());
        assert_eq!(env.events(&id).await, ["firing", "resolved"]);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_repository_unavailable_fires_and_resolves() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        let now = now_ms();
        sqlx::query("UPDATE repositories SET open_error = 'config hash mismatch' WHERE id = $1")
            .bind(&env.repo)
            .execute(env.pool())
            .await
            .unwrap();

        assert_eq!(env.tick(now).await.fired, 1, "[{}]", backend.name());
        let alerts = env.alerts("active").await;
        assert_eq!(alerts.len(), 1);
        let alert = &alerts[0];
        assert_eq!(alert["type"], "repository_unavailable");
        assert_eq!(alert["severity"], "critical");
        assert_eq!(alert["title"], "Repository main is unavailable");
        assert_eq!(alert["facts"]["error"], "config hash mismatch");
        let id = alert["id"].as_str().unwrap().to_string();

        sqlx::query("UPDATE repositories SET open_error = NULL WHERE id = $1")
            .bind(&env.repo)
            .execute(env.pool())
            .await
            .unwrap();
        assert_eq!(env.tick(now + MINUTE).await.resolved, 1);
        assert!(env.alerts("active").await.is_empty());
        assert_eq!(env.events(&id).await, ["firing", "resolved"]);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_check_cancel_keeps_alert() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        let now = now_ms();
        env.check_run("c1", "structure", "failed", 2, None, now - HOUR)
            .await;
        env.tick(now).await;
        env.check_run("c2", "structure", "aborted", 0, None, now + HOUR)
            .await;

        let after = env.tick(now + HOUR + MINUTE).await;
        assert_eq!(after, Default::default(), "[{}]", backend.name());
        let alerts = env.alerts("active").await;
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0]["title"], "Structure check failed on main");
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_check_levels_separate() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        let now = now_ms();
        env.check_run("c1", "structure", "failed", 1, None, now - 2 * HOUR)
            .await;
        env.check_run("c2", "packs", "failed", 4, None, now - HOUR)
            .await;

        let out = env.tick(now).await;
        assert_eq!(out.fired, 2, "[{}]", backend.name());
        assert_eq!(env.alerts("active").await.len(), 2);

        env.check_run("c3", "packs", "done", 0, None, now + HOUR)
            .await;
        let resolved = env.tick(now + HOUR + MINUTE).await;
        assert_eq!(resolved.resolved, 1);
        let active = env.alerts("active").await;
        assert_eq!(active.len(), 1);
        assert_eq!(active[0]["check_level"], "structure");
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_check_cut_short_does_not_raise() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        let now = now_ms();
        env.check_run(
            "c1",
            "structure",
            "failed",
            0,
            Some("The server restarted during the check."),
            now - HOUR,
        )
        .await;
        sqlx::query("UPDATE check_runs SET report_json = NULL WHERE id = 'c1'")
            .execute(env.pool())
            .await
            .unwrap();

        let out = env.tick(now).await;
        assert_eq!(out.fired, 0, "[{}]", backend.name());
        assert!(env.alerts("active").await.is_empty());
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_check_stopped_raises() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        let now = now_ms();
        env.check_run(
            "c1",
            "restore",
            "failed",
            0,
            Some("Stopped at the size limit."),
            now - HOUR,
        )
        .await;

        let out = env.tick(now).await;
        assert_eq!(out.fired, 1, "[{}]", backend.name());
        let alerts = env.alerts("active").await;
        assert_eq!(alerts[0]["title"], "Test restore check failed on main");
        assert_eq!(alerts[0]["facts"]["error"], "Stopped at the size limit.");
        assert!(
            alerts[0]["detail"]
                .as_str()
                .unwrap()
                .starts_with("Stopped at the size limit."),
            "{}",
            alerts[0]
        );
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_check_not_silenced() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        env.host("h1", "web-1").await;
        env.silence_row("h1", None).await;
        let now = now_ms();
        env.check_run("c1", "structure", "failed", 1, None, now - HOUR)
            .await;

        let out = env.tick(now).await;
        assert_eq!(out.fired, 1, "[{}]", backend.name());
        let id = env.alert_id("check_failed").await;
        assert_eq!(env.events(&id).await, ["firing"]);
        assert!(env.notified_at(&id).await.is_some());
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_agent_update_failed_raises_and_resolves() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        env.host("h1", "web-1").await;
        let now = now_ms();
        env.agent_update(
            "u1",
            "h1",
            "rolled_back",
            "0.2.0",
            Some("Signature check failed: key not trusted on this host."),
            now - HOUR,
        )
        .await;

        let first = env.tick(now).await;
        assert_eq!(first.fired, 1, "[{}]", backend.name());
        let alerts = env.alerts("active").await;
        assert_eq!(alerts.len(), 1);
        let alert = &alerts[0];
        assert_eq!(alert["type"], "agent_update_failed");
        assert_eq!(alert["severity"], "critical");
        assert_eq!(alert["title"], "Agent update failed on web-1");
        assert_eq!(
            alert["facts"]["error"],
            "Signature check failed: key not trusted on this host."
        );
        let id = alert["id"].as_str().unwrap().to_string();
        let key: String = sqlx::query("SELECT key FROM alerts WHERE id = $1")
            .bind(&id)
            .fetch_one(env.pool())
            .await
            .unwrap()
            .get("key");
        assert_eq!(key, "agent_update_failed:h1");
        assert_eq!(env.tick(now + MINUTE).await, Default::default());

        env.agent_update("u2", "h1", "succeeded", "0.2.0", None, now + HOUR)
            .await;
        sqlx::query("UPDATE agents SET version = '0.2.0' WHERE id = 'h1'")
            .execute(env.pool())
            .await
            .unwrap();
        let after = env.tick(now + HOUR + MINUTE).await;
        assert_eq!(after.resolved, 1);
        assert!(env.alerts("active").await.is_empty());
        assert_eq!(env.events(&id).await, ["firing", "resolved"]);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_agent_update_failed_clears_on_target_version() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        env.host("h1", "web-1").await;
        let now = now_ms();
        env.agent_update("u1", "h1", "rolled_back", "0.2.0", None, now - HOUR)
            .await;
        assert_eq!(env.tick(now).await.fired, 1, "[{}]", backend.name());

        sqlx::query("UPDATE agents SET version = '0.2.0' WHERE id = 'h1'")
            .execute(env.pool())
            .await
            .unwrap();
        assert_eq!(env.tick(now + MINUTE).await.resolved, 1);
        assert!(env.alerts("active").await.is_empty());
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_agent_update_failed_clears_above_the_target() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        env.host("h1", "web-1").await;
        let now = now_ms();
        env.agent_update("u1", "h1", "rolled_back", "0.2.0", None, now - HOUR)
            .await;
        assert_eq!(env.tick(now).await.fired, 1, "[{}]", backend.name());

        sqlx::query("UPDATE agents SET version = '0.10.0' WHERE id = 'h1'")
            .execute(env.pool())
            .await
            .unwrap();
        assert_eq!(env.tick(now + MINUTE).await.resolved, 1);
        assert!(env.alerts("active").await.is_empty());
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_agent_update_failed_stays_on_an_unreadable_version() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        env.host("h1", "web-1").await;
        let now = now_ms();
        env.agent_update("u1", "h1", "rolled_back", "0.2.0", None, now - HOUR)
            .await;
        assert_eq!(env.tick(now).await.fired, 1, "[{}]", backend.name());

        for version in ["", "custom-build"] {
            sqlx::query("UPDATE agents SET version = $1 WHERE id = 'h1'")
                .bind(version)
                .execute(env.pool())
                .await
                .unwrap();
            assert_eq!(env.tick(now + MINUTE).await.resolved, 0, "{version:?}");
            assert_eq!(env.alerts("active").await.len(), 1);
        }
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_agent_update_failed_silenced_and_revoked() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        env.host("h1", "web-1").await;
        env.host("h2", "web-2").await;
        let now = now_ms();
        env.agent_update("u1", "h1", "rolled_back", "0.2.0", None, now - HOUR)
            .await;
        env.agent_update("u2", "h2", "rolled_back", "0.2.0", None, now - HOUR)
            .await;
        sqlx::query("UPDATE agents SET status = 'revoked' WHERE id = 'h2'")
            .execute(env.pool())
            .await
            .unwrap();
        assert_eq!(env.put_silence("h1").await, 200, "[{}]", backend.name());

        assert_eq!(env.tick(now).await, Default::default());
        let alerts = env.alerts("active").await;
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0]["silenced"], true);
        assert_eq!(alerts[0]["host"]["id"], "h1");
        assert_eq!(env.event_count().await, 0);
        let id = env.alert_id("agent_update_failed").await;
        assert_eq!(env.notified_at(&id).await, None);
        env.stop().await;
    }
}
