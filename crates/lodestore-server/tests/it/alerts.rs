use crate::common;

use common::{Api, TestServer, backends, bearer_client};
use lodestore_proto::agent::v1::{GuestKind, ProxmoxFacts, ProxmoxGuest, SourceFacts};
use lodestore_server::alerts;
use lodestore_server::db::now_ms;
use lodestore_server::sources;
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

    async fn guest_policy(&self, name: &str, host: &str) {
        self.vm_policy(name, host, "101").await;
    }

    async fn vm_policy(&self, name: &str, host: &str, vmids: &str) {
        let yaml = format!(
            "apiVersion: lodestore.io/v1alpha2\nkind: BackupPolicy\nmetadata:\n  name: {name}\n\
             spec:\n  repository: main\n  selector:\n    hosts: [{host}]\n  \
             source:\n    type: proxmox-vm\n    vmids: [{vmids}]\n{DAILY}"
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

    async fn guest_snapshot(&self, id: &str, host: &str, policy: &str, guest: &str, at: i64) {
        self.snapshot(id, host, policy, at).await;
        sqlx::query(
            "UPDATE snapshots SET source_type = 'proxmox-vm', source_ref = $1, \
             source_label = $2 WHERE id = $3",
        )
        .bind(guest)
        .bind(format!("label of {guest}"))
        .bind(id)
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn facts(&self, host: &str, vmids: &[u32], at: i64) {
        self.facts_of(host, vmids, false, at).await;
    }

    async fn facts_of(&self, host: &str, vmids: &[u32], truncated: bool, at: i64) {
        let facts = SourceFacts {
            truncated,
            proxmox: Some(ProxmoxFacts {
                cluster: "pve-prod".to_string(),
                node: "pve1".to_string(),
                guests: vmids
                    .iter()
                    .map(|vmid| ProxmoxGuest {
                        kind: GuestKind::Vm as i32,
                        vmid: *vmid,
                        name: format!("vm-{vmid}"),
                        uuid: format!("{vmid:08}-1111-4111-8111-111111111111"),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }),
            ..Default::default()
        };
        sources::record(self.ts.state(), host, &facts, &mut None, at)
            .await
            .unwrap();
    }

    async fn guest_key(&self, host: &str, vmid: u32) -> String {
        let stored = sources::load(self.ts.state(), host).await.unwrap().unwrap();
        let guests = stored.discovery.proxmox.unwrap().guests;
        guests.into_iter().find(|g| g.vmid == vmid).unwrap().key
    }

    async fn firing_keys(&self, kind: &str) -> Vec<String> {
        sqlx::query("SELECT key FROM alerts WHERE type = $1 AND state = 'firing' ORDER BY key")
            .bind(kind)
            .fetch_all(self.pool())
            .await
            .unwrap()
            .iter()
            .map(|r| r.try_get("key").unwrap())
            .collect()
    }

    async fn alert_count(&self, kind: &str) -> i64 {
        sqlx::query("SELECT COUNT(*) AS n FROM alerts WHERE type = $1")
            .bind(kind)
            .fetch_one(self.pool())
            .await
            .unwrap()
            .try_get("n")
            .unwrap()
    }

    async fn gone_rows(&self) -> Vec<(String, i64)> {
        sqlx::query("SELECT source_ref, gone_at FROM source_instances_gone ORDER BY source_ref")
            .fetch_all(self.pool())
            .await
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r.try_get("source_ref").unwrap(),
                    r.try_get("gone_at").unwrap(),
                )
            })
            .collect()
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_certificate_rejected_opens_and_resolves() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        let now = now_ms();
        let registry = &env.ts.state().registry;
        let ip = Some("10.0.0.7".parse().unwrap());
        env.host("a2", "gone").await;
        sqlx::query("UPDATE agents SET status = 'revoked' WHERE id = 'a2'")
            .execute(env.pool())
            .await
            .unwrap();
        registry.note_cert_rejected("a2", "dead", ip);
        assert_eq!(env.tick(now).await.fired, 0, "[{}]", backend.name());

        env.host("a1", "web-1").await;
        registry.note_cert_rejected("a1", "abc", ip);
        assert_eq!(env.tick(now).await.fired, 1, "[{}]", backend.name());
        let alerts = env.alerts("active").await;
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0]["type"], "agent_certificate_rejected");
        assert_eq!(alerts[0]["severity"], "critical");
        assert_eq!(alerts[0]["host"]["id"], "a1");
        assert_eq!(alerts[0]["facts"]["serial"], "abc");
        assert_eq!(alerts[0]["facts"]["ip"], "10.0.0.7");
        let id = alerts[0]["id"].as_str().unwrap().to_string();

        registry.note_cert_rejected("a1", "def", ip);
        assert_eq!(env.tick(now + MINUTE).await.fired, 0);
        let alerts = env.alerts("active").await;
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0]["facts"]["serial"], "def");

        assert_eq!(env.tick(now + DAY + MINUTE).await.resolved, 1);
        assert!(env.alerts("active").await.is_empty());
        assert_eq!(env.events(&id).await, ["firing", "resolved"]);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_security_alert_survives_a_restart_until_its_day_is_over() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        let now = now_ms();
        env.host("a1", "web-1").await;
        sqlx::query(
            "INSERT INTO alerts (id, type, key, severity, state, host_id, title, detail, \
             facts_json, started_at, updated_at, notified_at) \
             VALUES ('s1', 'agent_session_conflict', 'agent_session_conflict:a1', 'warning', \
             'firing', 'a1', 't', 'd', '{\"ip\":\"10.0.0.8\",\"previous_ip\":\"10.0.0.7\"}', \
             $1, $1, $1)",
        )
        .bind(now - 2 * HOUR)
        .execute(env.pool())
        .await
        .unwrap();

        let held = env.tick(now).await;
        assert_eq!((held.fired, held.resolved), (0, 0), "[{}]", backend.name());
        let alerts = env.alerts("active").await;
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0]["facts"]["previous_ip"], "10.0.0.7");

        assert_eq!(env.tick(now + 21 * HOUR).await.resolved, 0);
        assert_eq!(env.tick(now + 23 * HOUR).await.resolved, 1);
        assert!(env.alerts("active").await.is_empty());
        env.stop().await;
    }
}

fn guests(n: usize) -> Vec<String> {
    (0..n)
        .map(|i| format!("proxmox-vm/pve1/{i:08}-0000-4000-8000-000000000000"))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_dead_man_per_source_instance() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        env.host("h1", "web-1").await;
        env.policy("daily", "h1", DAILY).await;
        env.guest_policy("vms", "h1").await;
        let now = now_ms();
        env.snapshot("f1", "h1", "daily", now - HOUR).await;
        let refs = guests(40);
        for (i, guest) in refs.iter().enumerate() {
            let at = if i == 17 { now - 3 * DAY } else { now - HOUR };
            env.guest_snapshot(&format!("g{i}"), "h1", "vms", guest, at)
                .await;
        }
        let thresholds = alerts::model::load_thresholds(env.ts.state())
            .await
            .unwrap();
        let overdue = |inputs: &alerts::inputs::Inputs| {
            alerts::evaluate::evaluate(inputs, now, env.started(), &thresholds)
                .into_iter()
                .filter(|c| c.kind == alerts::model::AlertType::NoRecentBackup)
                .map(|c| c.key)
                .collect::<Vec<_>>()
        };
        let want = vec![format!("no_recent_backup:h1:vms:{}", refs[17])];

        let listed = refs.clone();
        let inputs = alerts::inputs::gather_with(env.ts.state(), now, |policy, _| {
            policy
                .spec
                .source
                .is_some()
                .then(|| alerts::inputs::Discovered {
                    keys: listed.clone(),
                    complete: true,
                })
        })
        .await
        .unwrap();
        assert_eq!(overdue(&inputs), want, "[{}]", backend.name());
        let pair = inputs.hosts[0]
            .pairs
            .iter()
            .find(|p| p.source_ref == refs[17])
            .unwrap();
        assert_eq!(
            pair.source_label.as_deref(),
            Some(format!("label of {}", refs[17]).as_str())
        );

        let unknown = alerts::inputs::gather(env.ts.state(), now).await.unwrap();
        assert_eq!(
            overdue(&unknown),
            want,
            "[{}] without discovery",
            backend.name()
        );
        env.stop().await;
    }
}

async fn files_host_with_two_guests(env: &Env, now: i64) -> (String, String) {
    env.host("h1", "web-1").await;
    env.policy("daily", "h1", DAILY).await;
    env.snapshot("f1", "h1", "daily", now - MINUTE).await;
    let refs = guests(2);
    env.guest_snapshot("g1", "h1", "daily", &refs[0], now - 30 * DAY)
        .await;
    env.guest_snapshot("g2", "h1", "daily", &refs[1], now - MINUTE)
        .await;
    (refs[0].clone(), refs[1].clone())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_instance_no_longer_selected_goes_quiet() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        let now = now_ms();
        files_host_with_two_guests(&env, now).await;
        env.snapshot("f2", "h1", "daily", now + 30 * DAY).await;

        env.tick(now).await;
        env.tick(now + 30 * DAY).await;
        for kind in ["backup_failed", "no_recent_backup"] {
            let found = sqlx::query("SELECT COUNT(*) AS n FROM alerts WHERE type = $1")
                .bind(kind)
                .fetch_one(env.pool())
                .await
                .unwrap();
            assert_eq!(
                found.try_get::<i64, _>("n").unwrap(),
                0,
                "[{}] {kind}",
                backend.name()
            );
        }

        let fleet = env.api.get("/api/v1/fleet").await;
        let fleet: Value = fleet.json().await.unwrap();
        assert_eq!(
            fleet["hosts"][0]["state"],
            "healthy",
            "[{}]",
            backend.name()
        );

        let token = env.ts.issue_token(&["metrics:read"]).await;
        let scrape = bearer_client(&token)
            .get(format!("{}/metrics", env.ts.rest_url()))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let overdue: Vec<_> = scrape
            .lines()
            .filter(|l| l.starts_with("lodestore_backup_overdue{"))
            .collect();
        assert_eq!(overdue.len(), 1, "{overdue:?}");
        assert!(overdue[0].ends_with(" 0"), "{overdue:?}");
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_gone_moment_is_stored_once() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        let t0 = now_ms();
        let (g1, g2) = files_host_with_two_guests(&env, t0).await;

        env.tick(t0).await;
        let want = vec![(g1.clone(), t0), (g2.clone(), t0)];
        assert_eq!(env.gone_rows().await, want, "[{}]", backend.name());
        env.tick(t0 + HOUR).await;
        env.tick(t0 + 2 * DAY).await;
        assert_eq!(env.gone_rows().await, want, "[{}]", backend.name());

        let inputs = alerts::inputs::gather(env.ts.state(), t0 + 2 * DAY)
            .await
            .unwrap();
        for pair in &inputs.hosts[0].pairs {
            if pair.source_ref.is_empty() {
                assert!(pair.expected);
                assert_eq!(pair.gone_at_ms, None);
            } else {
                assert!(!pair.expected);
                assert_eq!(pair.gone_at_ms, Some(t0), "{}", pair.source_ref);
            }
        }
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_still_evaluate_when_the_gone_moment_cannot_be_stored() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        let now = now_ms();
        env.host("h1", "web-1").await;
        env.policy("daily", "h1", DAILY).await;
        env.guest_snapshot("g1", "h1", "daily", &guests(1)[0], now - DAY)
            .await;
        env.failed_job("j1", "h1", "daily", now - HOUR, "disk full")
            .await;
        // A view that is readable but not writable on both backends.
        for sql in [
            "ALTER TABLE source_instances_gone RENAME TO source_instances_gone_rw",
            "CREATE VIEW source_instances_gone AS SELECT DISTINCT * FROM source_instances_gone_rw",
        ] {
            sqlx::query(sql).execute(env.pool()).await.unwrap();
        }

        let out = alerts::tick(env.ts.state(), now).await;
        assert!(out.is_ok(), "[{}] {out:?}", backend.name());
        let active = env.alerts("active").await;
        assert!(
            active.iter().any(|a| a["type"] == "backup_failed"),
            "[{}] {active:?}",
            backend.name()
        );
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_gone_moment_clears_when_expected_again() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        env.host("h1", "web-1").await;
        env.guest_policy("vms", "h1").await;
        let now = now_ms();
        let refs = guests(2);
        env.guest_snapshot("g1", "h1", "vms", &refs[0], now - MINUTE)
            .await;
        env.guest_snapshot("g2", "h1", "vms", &refs[1], now - MINUTE)
            .await;
        let (a, b) = (refs[0].clone(), refs[1].clone());

        let step = |listed: Vec<String>, at: i64| {
            let state = env.ts.state();
            async move {
                let mut inputs = alerts::inputs::gather_with(state, at, |_, _| {
                    Some(alerts::inputs::Discovered {
                        keys: listed.clone(),
                        complete: true,
                    })
                })
                .await
                .unwrap();
                alerts::inputs::record_gone(state, &mut inputs.hosts, at)
                    .await
                    .unwrap();
                inputs.hosts.remove(0).pairs
            }
        };
        let pairs = step(vec![a.clone()], now + MINUTE).await;
        assert_eq!(
            env.gone_rows().await,
            [(b.clone(), now + MINUTE)],
            "[{}]",
            backend.name()
        );
        assert_eq!(
            pairs.iter().find(|p| p.source_ref == b).unwrap().gone_at_ms,
            Some(now + MINUTE)
        );

        let pairs = step(vec![a.clone(), b.clone()], now + 2 * MINUTE).await;
        assert!(env.gone_rows().await.is_empty(), "[{}]", backend.name());
        assert_eq!(
            pairs.iter().find(|p| p.source_ref == b).unwrap().gone_at_ms,
            None
        );

        step(vec![a], now + 3 * MINUTE).await;
        assert_eq!(
            env.gone_rows().await,
            [(b, now + 3 * MINUTE)],
            "[{}]",
            backend.name()
        );
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_gone_instance_opens_one_event() {
    for backend in backends() {
        let name = backend.name();
        let env = Env::start(&backend).await;
        let t0 = now_ms();
        env.host("h1", "web-1").await;
        env.policy("daily", "h1", DAILY).await;
        env.snapshot("f1", "h1", "daily", t0 - MINUTE).await;
        let guest = guests(1).remove(0);
        env.guest_snapshot("g1", "h1", "daily", &guest, t0 - 30 * DAY)
            .await;
        let rows = |state: &'static str| {
            let env = &env;
            async move {
                env.alerts(state)
                    .await
                    .into_iter()
                    .filter(|a| a["type"] == "source_instance_gone")
                    .map(|a| a["id"].as_str().unwrap().to_string())
                    .collect::<Vec<_>>()
            }
        };

        env.tick(t0).await;
        assert!(rows("active").await.is_empty(), "[{name}]");
        env.tick(t0 + HOUR).await;
        let first = rows("active").await;
        assert_eq!(first.len(), 1, "[{name}]");
        let stored: String =
            sqlx::query("SELECT key FROM alerts WHERE type = 'source_instance_gone'")
                .fetch_one(env.pool())
                .await
                .unwrap()
                .try_get("key")
                .unwrap();
        assert_eq!(
            stored,
            format!("source_instance_gone:h1:daily:{guest}"),
            "[{name}]"
        );
        for at in [t0 + 2 * HOUR, t0 + 23 * HOUR] {
            env.tick(at).await;
            assert_eq!(rows("active").await, first, "[{name}] {at}");
        }

        env.tick(t0 + 25 * HOUR).await;
        assert!(rows("active").await.is_empty(), "[{name}]");
        assert_eq!(rows("resolved").await, first, "[{name}]");
        for at in [t0 + 2 * DAY, t0 + 30 * DAY] {
            env.tick(at).await;
            assert!(rows("active").await.is_empty(), "[{name}] {at}");
            assert_eq!(rows("resolved").await, first, "[{name}] {at}");
        }

        sqlx::query("DELETE FROM policies WHERE name = 'daily'")
            .execute(env.pool())
            .await
            .unwrap();
        env.guest_policy("daily", "h1").await;
        let t1 = t0 + 31 * DAY;
        env.tick(t1).await;
        assert!(env.gone_rows().await.is_empty(), "[{name}]");
        assert!(rows("active").await.is_empty(), "[{name}]");

        sqlx::query("DELETE FROM policies WHERE name = 'daily'")
            .execute(env.pool())
            .await
            .unwrap();
        env.policy("daily", "h1", DAILY).await;
        env.tick(t1 + HOUR).await;
        assert!(rows("active").await.is_empty(), "[{name}]");
        env.tick(t1 + 2 * HOUR).await;
        let second = rows("active").await;
        assert_eq!(second.len(), 1, "[{name}]");
        assert_ne!(second, first, "[{name}]");
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_expect_discovered_instances() {
    for backend in backends() {
        let name = backend.name();
        let env = Env::start(&backend).await;
        env.host("h1", "web-1").await;
        env.vm_policy("vms", "h1", "101, 103").await;
        let now = now_ms();
        env.facts("h1", &[101, 103], now).await;
        let (a, b) = (
            env.guest_key("h1", 101).await,
            env.guest_key("h1", 103).await,
        );

        env.tick(now).await;
        let mut want = vec![
            format!("no_recent_backup:h1:vms:{a}"),
            format!("no_recent_backup:h1:vms:{b}"),
        ];
        want.sort();
        assert_eq!(env.firing_keys("no_recent_backup").await, want, "[{name}]");

        env.facts("h1", &[], now + MINUTE).await;
        env.tick(now + MINUTE).await;
        assert_eq!(
            env.firing_keys("no_recent_backup").await,
            ["no_recent_backup:h1:vms"],
            "[{name}]"
        );
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_refused_job_alerts_through_the_stub() {
    for backend in backends() {
        let name = backend.name();
        let env = Env::start(&backend).await;
        env.host("h1", "web-1").await;
        env.guest_policy("vms", "h1").await;
        let now = now_ms();
        env.facts("h1", &[101], now).await;
        env.failed_job(
            "j1",
            "h1",
            "vms",
            now - HOUR,
            "The agent is too old for this source.",
        )
        .await;

        env.tick(now).await;
        assert_eq!(
            env.firing_keys("backup_failed").await,
            ["backup_failed:h1:vms"],
            "[{name}]"
        );
        let active = env.alerts("active").await;
        let failed = active
            .iter()
            .find(|a| a["type"] == "backup_failed")
            .unwrap();
        assert_eq!(
            failed["facts"]["error"], "The agent is too old for this source.",
            "[{name}]"
        );

        let key = env.guest_key("h1", 101).await;
        env.guest_snapshot("g1", "h1", "vms", &key, now - MINUTE)
            .await;
        env.tick(now + MINUTE).await;
        assert!(
            env.firing_keys("backup_failed").await.is_empty(),
            "[{name}]"
        );
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_gone_needs_an_hour_and_rearms_after_a_day() {
    for backend in backends() {
        let name = backend.name();
        let env = Env::start(&backend).await;
        env.host("h1", "web-1").await;
        env.guest_policy("vms", "h1").await;
        let t = now_ms();
        env.facts("h1", &[101], t).await;
        let key = env.guest_key("h1", 101).await;
        env.guest_snapshot("g1", "h1", "vms", &key, t - MINUTE)
            .await;
        let gone_event = "source_instance_gone";

        let step = |vmids: &'static [u32], at: i64| {
            let env = &env;
            async move {
                env.facts("h1", vmids, at).await;
                env.tick(at).await;
            }
        };
        step(&[105], t).await;
        assert_eq!(env.gone_rows().await.len(), 1, "[{name}]");
        assert!(env.firing_keys(gone_event).await.is_empty(), "[{name}]");

        step(&[105], t + 61 * MINUTE).await;
        assert_eq!(env.firing_keys(gone_event).await.len(), 1, "[{name}]");

        step(&[101], t + 2 * HOUR).await;
        assert!(env.firing_keys(gone_event).await.is_empty(), "[{name}]");
        let returned = sqlx::query("SELECT returned_at FROM source_instances_gone")
            .fetch_one(env.pool())
            .await
            .unwrap();
        assert_eq!(
            returned.try_get::<Option<i64>, _>("returned_at").unwrap(),
            Some(t + 2 * HOUR),
            "[{name}]"
        );

        step(&[105], t + 3 * HOUR).await;
        step(&[105], t + 5 * HOUR).await;
        assert!(env.firing_keys(gone_event).await.is_empty(), "[{name}]");
        assert_eq!(env.alert_count(gone_event).await, 1, "[{name}]");

        step(&[101], t + 26 * HOUR).await;
        assert!(env.gone_rows().await.is_empty(), "[{name}]");

        step(&[105], t + 27 * HOUR).await;
        assert!(env.firing_keys(gone_event).await.is_empty(), "[{name}]");
        step(&[105], t + 28 * HOUR + MINUTE).await;
        assert_eq!(env.firing_keys(gone_event).await.len(), 1, "[{name}]");
        assert_eq!(env.alert_count(gone_event).await, 2, "[{name}]");

        env.host("h2", "web-2").await;
        env.vm_policy("vms2", "h2", "101").await;
        let base = t + 30 * HOUR;
        env.facts("h2", &[101], base).await;
        let key2 = env.guest_key("h2", 101).await;
        env.guest_snapshot("g2", "h2", "vms2", &key2, base - MINUTE)
            .await;
        env.tick(base + 10 * HOUR).await;
        let rows =
            sqlx::query("SELECT COUNT(*) AS n FROM source_instances_gone WHERE host_id = 'h2'")
                .fetch_one(env.pool())
                .await
                .unwrap();
        assert_eq!(
            rows.try_get::<i64, _>("n").unwrap(),
            0,
            "[{name}] stale facts"
        );
        env.stop().await;
    }
}

/// A guest the agent stops reporting while its list is incomplete (a SMBIOS
/// uuid shared with another VM, or any other guest left out) is neither
/// declared gone nor dropped: it stays expected, so its missed backups alert.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_guest_left_out_of_an_incomplete_list_still_alerts() {
    for backend in backends() {
        let name = backend.name();
        let env = Env::start(&backend).await;
        env.host("h1", "web-1").await;
        env.vm_policy("vms", "h1", "101, 102").await;
        let t = now_ms();
        env.facts("h1", &[101, 102], t).await;
        let (victim, other) = (
            env.guest_key("h1", 101).await,
            env.guest_key("h1", 102).await,
        );
        env.guest_snapshot("g1", "h1", "vms", &victim, t - MINUTE)
            .await;
        env.guest_snapshot("g2", "h1", "vms", &other, t - MINUTE)
            .await;
        env.tick(t).await;
        assert!(
            env.firing_keys("no_recent_backup").await.is_empty(),
            "[{name}]"
        );

        let mut at = t;
        while at < t + 3 * DAY {
            at += 6 * HOUR;
            env.facts_of("h1", &[102], true, at).await;
            env.guest_snapshot(&format!("o{at}"), "h1", "vms", &other, at - MINUTE)
                .await;
            env.tick(at).await;
        }
        assert!(env.gone_rows().await.is_empty(), "[{name}]");
        assert_eq!(
            env.firing_keys("no_recent_backup").await,
            [format!("no_recent_backup:h1:vms:{victim}")],
            "[{name}]"
        );
        env.stop().await;
    }
}

impl Env {
    async fn ct_policy(&self, name: &str, host: &str, vmids: &str) {
        let yaml = format!(
            "apiVersion: lodestore.io/v1alpha2\nkind: BackupPolicy\nmetadata:\n  name: {name}\n\
             spec:\n  repository: main\n  selector:\n    hosts: [{host}]\n  \
             source:\n    type: proxmox-ct\n    vmids: [{vmids}]\n{DAILY}"
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

    async fn ct_facts(&self, host: &str, cts: &[u32], truncated: bool, at: i64) {
        let facts = SourceFacts {
            truncated,
            proxmox: Some(ProxmoxFacts {
                cluster: "pve-prod".to_string(),
                node: "pve1".to_string(),
                guests: cts
                    .iter()
                    .map(|vmid| ProxmoxGuest {
                        kind: GuestKind::Ct as i32,
                        vmid: *vmid,
                        name: "dns".to_string(),
                        uuid: format!("{vmid:08}-1111-4111-8111-111111111111"),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }),
            ..Default::default()
        };
        sources::record(self.ts.state(), host, &facts, &mut None, at)
            .await
            .unwrap();
    }

    async fn ct_snapshot(&self, id: &str, host: &str, policy: &str, vmid: u32, at: i64) -> String {
        let key = self.guest_key(host, vmid).await;
        self.snapshot(id, host, policy, at).await;
        sqlx::query(
            "UPDATE snapshots SET source_type = 'proxmox-ct', source_ref = $1, \
             source_label = $2 WHERE id = $3",
        )
        .bind(&key)
        .bind(format!("CT {vmid} · dns"))
        .bind(id)
        .execute(self.pool())
        .await
        .unwrap();
        key
    }

    async fn unprotected_rows(&self) -> Vec<(String, String)> {
        sqlx::query("SELECT host_id, source_ref FROM guests_unprotected ORDER BY source_ref")
            .fetch_all(self.pool())
            .await
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r.try_get("host_id").unwrap(),
                    r.try_get("source_ref").unwrap(),
                )
            })
            .collect()
    }

    async fn of_type(&self, state: &str, kind: &str) -> Vec<Value> {
        self.alerts(state)
            .await
            .into_iter()
            .filter(|a| a["type"] == kind)
            .collect()
    }
}

/// CT 200 was backed up by a policy that selects another container now.
async fn unprotected_ct(env: &Env, t0: i64) -> String {
    env.host("h1", "web-1").await;
    env.ct_policy("cts", "h1", "201").await;
    env.ct_facts("h1", &[200], false, t0).await;
    env.ct_snapshot("s1", "h1", "cts", 200, t0 - DAY).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_unprotected_opens_after_hour() {
    for backend in backends() {
        let name = backend.name();
        let env = Env::start(&backend).await;
        let t0 = now_ms();
        let key = unprotected_ct(&env, t0).await;

        env.tick(t0).await;
        assert_eq!(
            env.unprotected_rows().await,
            [("h1".to_string(), key.clone())],
            "[{name}]"
        );
        env.ct_facts("h1", &[200], false, t0 + 59 * MINUTE).await;
        env.tick(t0 + 59 * MINUTE).await;
        assert!(env.of_type("active", "guest_unprotected").await.is_empty());

        env.ct_facts("h1", &[200], false, t0 + HOUR).await;
        env.tick(t0 + HOUR).await;
        let open = env.of_type("active", "guest_unprotected").await;
        assert_eq!(open.len(), 1, "[{name}]");
        assert_eq!(open[0]["severity"], "warning");
        assert_eq!(
            open[0]["title"],
            "CT 200 · dns on web-1 is not backed up by any policy"
        );
        assert_eq!(open[0]["facts"]["source_label"], "CT 200 · dns");
        assert_eq!(open[0]["policy"], "cts");
        assert_eq!(
            env.firing_keys("guest_unprotected").await,
            [format!("guest_unprotected:h1:{key}")]
        );
        assert!(
            open[0]["detail"]
                .as_str()
                .unwrap()
                .ends_with("by cts. Add it to a policy or silence the alert."),
            "[{name}] {}",
            open[0]["detail"]
        );
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_unprotected_new_guest_no_alert() {
    for backend in backends() {
        let name = backend.name();
        let env = Env::start(&backend).await;
        let t0 = now_ms();
        env.host("h1", "web-1").await;

        for at in [t0, t0 + HOUR, t0 + 3 * HOUR] {
            env.ct_facts("h1", &[300], false, at).await;
            env.tick(at).await;
            assert!(env.unprotected_rows().await.is_empty(), "[{name}]");
            assert_eq!(env.alert_count("guest_unprotected").await, 0, "[{name}]");
        }
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_unprotected_closes_on_selection() {
    for backend in backends() {
        let name = backend.name();
        let env = Env::start(&backend).await;
        let t0 = now_ms();
        unprotected_ct(&env, t0).await;
        env.tick(t0).await;
        env.ct_facts("h1", &[200], false, t0 + HOUR).await;
        env.tick(t0 + HOUR).await;
        assert_eq!(env.of_type("active", "guest_unprotected").await.len(), 1);

        env.ct_policy("both", "h1", "200").await;
        env.ct_facts("h1", &[200], false, t0 + 2 * HOUR).await;
        env.tick(t0 + 2 * HOUR).await;
        assert!(
            env.of_type("active", "guest_unprotected").await.is_empty(),
            "[{name}]"
        );
        assert_eq!(
            env.of_type("resolved", "guest_unprotected").await.len(),
            1,
            "[{name}]"
        );
        assert!(env.unprotected_rows().await.is_empty(), "[{name}]");
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_unprotected_unknown_list_keeps_state() {
    for backend in backends() {
        let name = backend.name();
        let env = Env::start(&backend).await;
        let t0 = now_ms();
        unprotected_ct(&env, t0).await;
        env.tick(t0).await;
        env.ct_facts("h1", &[200], false, t0 + HOUR).await;
        env.tick(t0 + HOUR).await;
        assert_eq!(env.of_type("active", "guest_unprotected").await.len(), 1);

        env.tick(t0 + 3 * HOUR).await;
        assert_eq!(
            env.of_type("active", "guest_unprotected").await.len(),
            1,
            "[{name}] a stale list closes nothing"
        );
        env.ct_facts("h1", &[], true, t0 + 4 * HOUR).await;
        env.tick(t0 + 4 * HOUR).await;
        assert_eq!(
            env.of_type("active", "guest_unprotected").await.len(),
            1,
            "[{name}] a cut list closes nothing"
        );
        assert_eq!(env.unprotected_rows().await.len(), 1, "[{name}]");
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_unprotected_unknown_list_opens_nothing() {
    for backend in backends() {
        let name = backend.name();
        let env = Env::start(&backend).await;
        let t0 = now_ms();
        unprotected_ct(&env, t0).await;
        env.tick(t0 + 2 * HOUR).await;
        assert!(env.unprotected_rows().await.is_empty(), "[{name}] stale");
        env.ct_facts("h1", &[200], true, t0 + 3 * HOUR).await;
        env.tick(t0 + 3 * HOUR).await;
        assert!(env.unprotected_rows().await.is_empty(), "[{name}] cut");
        assert_eq!(env.alert_count("guest_unprotected").await, 0, "[{name}]");
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_unprotected_gone_closes() {
    for backend in backends() {
        let name = backend.name();
        let env = Env::start(&backend).await;
        let t0 = now_ms();
        unprotected_ct(&env, t0).await;
        env.tick(t0).await;
        env.ct_facts("h1", &[200], false, t0 + HOUR).await;
        env.tick(t0 + HOUR).await;
        assert_eq!(env.of_type("active", "guest_unprotected").await.len(), 1);

        env.ct_facts("h1", &[201], false, t0 + 2 * HOUR).await;
        env.tick(t0 + 2 * HOUR).await;
        assert!(
            env.of_type("active", "guest_unprotected").await.is_empty(),
            "[{name}]"
        );
        assert!(env.unprotected_rows().await.is_empty(), "[{name}]");
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_unprotected_silenced() {
    for backend in backends() {
        let name = backend.name();
        let env = Env::start(&backend).await;
        let t0 = now_ms();
        unprotected_ct(&env, t0).await;
        env.silence_row("h1", None).await;
        env.tick(t0).await;
        env.ct_facts("h1", &[200], false, t0 + HOUR).await;
        env.tick(t0 + HOUR).await;
        let open = env.of_type("active", "guest_unprotected").await;
        assert_eq!(open.len(), 1, "[{name}]");
        assert_eq!(open[0]["silenced"], true, "[{name}]");
        let id = open[0]["id"].as_str().unwrap();
        assert!(env.events(id).await.is_empty(), "[{name}]");
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_unprotected_vm_too() {
    for backend in backends() {
        let name = backend.name();
        let env = Env::start(&backend).await;
        let t0 = now_ms();
        env.host("h1", "web-1").await;
        env.facts("h1", &[100], t0).await;
        let key = env.guest_key("h1", 100).await;
        env.failed_job("j1", "h1", "vms", t0 - DAY, "outdated")
            .await;
        sqlx::query("UPDATE jobs SET source_ref = $1 WHERE id = 'j1'")
            .bind(&key)
            .execute(env.pool())
            .await
            .unwrap();

        env.tick(t0).await;
        env.facts("h1", &[100], t0 + HOUR).await;
        env.tick(t0 + HOUR).await;
        let open = env.of_type("active", "guest_unprotected").await;
        assert_eq!(open.len(), 1, "[{name}]");
        assert_eq!(
            open[0]["title"],
            "VM 100 · vm-100 on web-1 is not backed up by any policy"
        );
        assert!(
            open[0]["detail"]
                .as_str()
                .unwrap()
                .starts_with("It was backed up before."),
            "[{name}]"
        );
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sources_alert_open_flag() {
    for backend in backends() {
        let name = backend.name();
        let env = Env::start(&backend).await;
        let t0 = now_ms();
        unprotected_ct(&env, t0).await;
        env.ct_facts("h1", &[200, 300], false, t0).await;
        env.tick(t0).await;
        env.ct_facts("h1", &[200, 300], false, t0 + HOUR).await;
        env.tick(t0 + HOUR).await;

        let response = env.api.get("/api/v1/hosts/h1/sources").await;
        assert_eq!(response.status().as_u16(), 200);
        let body: Value = response.json().await.unwrap();
        let flags: Vec<(u64, bool)> = body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                (
                    i["vmid"].as_u64().unwrap(),
                    i["alert_open"].as_bool().unwrap(),
                )
            })
            .collect();
        assert_eq!(flags, [(200, true), (300, false)], "[{name}]");
        env.stop().await;
    }
}

impl Env {
    async fn pve_facts(
        &self,
        host: &str,
        (cluster, node): (&str, &str),
        members: &[(&str, &str)],
        plugin: (bool, bool),
        truncated: bool,
        at: i64,
    ) {
        let facts = SourceFacts {
            truncated,
            proxmox: Some(ProxmoxFacts {
                cluster: cluster.to_string(),
                node: node.to_string(),
                nodes: members
                    .iter()
                    .map(|(name, marker)| lodestore_proto::agent::v1::ClusterNode {
                        name: name.to_string(),
                        online: true,
                        plugin_marker: marker.to_string(),
                    })
                    .collect(),
                plugin_installed: plugin.0,
                plugin_current: plugin.1,
                ..Default::default()
            }),
            ..Default::default()
        };
        sources::record(self.ts.state(), host, &facts, &mut None, at)
            .await
            .unwrap();
    }

    async fn plugin_alerts(&self, state: &str) -> Vec<(String, String)> {
        sqlx::query("SELECT key, host_id FROM alerts WHERE type = 'pve_plugin_missing' AND state = $1 ORDER BY key")
            .bind(state)
            .fetch_all(self.pool())
            .await
            .unwrap()
            .iter()
            .map(|r| (r.try_get("key").unwrap(), r.try_get("host_id").unwrap()))
            .collect()
    }
}

const MARKER: &str = "0.5.0 1 abc 9.2.33";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pve_plugin_missing_node_without_marker() {
    for backend in backends() {
        let name = backend.name();
        let env = Env::start(&backend).await;
        let t0 = now_ms();
        env.host("h1", "pve-01").await;
        env.host("h2", "pve-02").await;
        let members =
            |third: &'static str| [("pve-01", MARKER), ("pve-02", MARKER), ("pve-03", third)];
        for (host, node) in [("h1", "pve-01"), ("h2", "pve-02")] {
            env.pve_facts(host, ("c1", node), &members(""), (true, true), false, t0)
                .await;
        }
        env.tick(t0).await;
        assert_eq!(
            env.plugin_alerts("firing").await,
            [("pve_plugin_missing:c1:pve-03".to_string(), "h1".to_string())],
            "[{name}]"
        );
        let open = env.of_type("active", "pve_plugin_missing").await;
        assert_eq!(open.len(), 1, "[{name}]");
        assert_eq!(open[0]["severity"], "warning");
        assert_eq!(
            open[0]["title"],
            "The Lodestore plugin is missing on pve-03 in c1"
        );
        assert_eq!(
            open[0]["detail"],
            "Run lodestore-agent pve install on pve-03. A node without the plugin removes the Lodestore storage for the whole cluster the next time it writes storage.cfg."
        );

        for (host, node) in [("h1", "pve-01"), ("h2", "pve-02")] {
            env.pve_facts(
                host,
                ("c1", node),
                &members(MARKER),
                (true, true),
                false,
                t0 + MINUTE,
            )
            .await;
        }
        env.tick(t0 + MINUTE).await;
        assert!(env.plugin_alerts("firing").await.is_empty(), "[{name}]");
        assert_eq!(env.plugin_alerts("resolved").await.len(), 1, "[{name}]");
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pve_plugin_missing_host_unhealthy() {
    for backend in backends() {
        let name = backend.name();
        let env = Env::start(&backend).await;
        let t0 = now_ms();
        env.host("h2", "pve-02").await;
        let members = [("pve-01", MARKER), ("pve-02", MARKER)];
        env.pve_facts("h2", ("c1", "pve-02"), &members, (false, false), false, t0)
            .await;
        env.tick(t0).await;
        let open = env.of_type("active", "pve_plugin_missing").await;
        assert_eq!(open.len(), 1, "[{name}]");
        assert_eq!(
            open[0]["title"],
            "The Lodestore plugin is missing on pve-02 in c1"
        );

        env.pve_facts(
            "h2",
            ("c1", "pve-02"),
            &members,
            (true, false),
            false,
            t0 + MINUTE,
        )
        .await;
        env.tick(t0 + MINUTE).await;
        let open = env.of_type("active", "pve_plugin_missing").await;
        assert_eq!(open.len(), 1, "[{name}]");
        assert_eq!(
            open[0]["title"],
            "The Lodestore plugin is outdated on pve-02 in c1"
        );

        env.pve_facts(
            "h2",
            ("c1", "pve-02"),
            &members,
            (true, true),
            false,
            t0 + 2 * MINUTE,
        )
        .await;
        env.tick(t0 + 2 * MINUTE).await;
        assert!(env.plugin_alerts("firing").await.is_empty(), "[{name}]");
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pve_plugin_missing_unknown_list() {
    for backend in backends() {
        let name = backend.name();
        let env = Env::start(&backend).await;
        let t0 = now_ms();
        env.host("h1", "pve-01").await;
        let members = [("pve-01", MARKER), ("pve-03", "")];
        env.pve_facts("h1", ("c1", "pve-01"), &members, (true, true), false, t0)
            .await;
        env.tick(t0).await;
        assert_eq!(env.plugin_alerts("firing").await.len(), 1, "[{name}]");

        env.tick(t0 + HOUR).await;
        assert_eq!(
            env.plugin_alerts("firing").await.len(),
            1,
            "[{name}] stale facts close nothing"
        );
        let healed = [("pve-01", MARKER), ("pve-03", MARKER)];
        env.pve_facts(
            "h1",
            ("c1", "pve-01"),
            &healed,
            (true, true),
            true,
            t0 + 2 * HOUR,
        )
        .await;
        env.tick(t0 + 2 * HOUR).await;
        assert_eq!(
            env.plugin_alerts("firing").await.len(),
            1,
            "[{name}] cut facts close nothing"
        );
        env.pve_facts(
            "h1",
            ("c1", "pve-01"),
            &healed,
            (true, true),
            false,
            t0 + 3 * HOUR,
        )
        .await;
        env.tick(t0 + 3 * HOUR).await;
        assert!(env.plugin_alerts("firing").await.is_empty(), "[{name}]");

        let other = t0 + 4 * HOUR;
        env.pve_facts("h1", ("c1", "pve-01"), &members, (true, true), true, other)
            .await;
        env.tick(other).await;
        env.tick(other + 2 * HOUR).await;
        assert!(
            env.plugin_alerts("firing").await.is_empty(),
            "[{name}] unknown facts open nothing"
        );
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_instance_overdue_ignores_fresh_files_snapshot() {
    for backend in backends() {
        let name = backend.name();
        let env = Env::start(&backend).await;
        let now = now_ms();
        env.host("h1", "web-1").await;
        env.policy("daily", "h1", DAILY).await;
        env.snapshot("f1", "h1", "daily", now - MINUTE).await;
        env.ct_policy("cts", "h1", "200").await;
        env.ct_facts("h1", &[200], false, now).await;
        let key = env.ct_snapshot("s1", "h1", "cts", 200, now - 3 * DAY).await;

        env.tick(now).await;
        assert_eq!(
            env.firing_keys("no_recent_backup").await,
            [format!("no_recent_backup:h1:cts:{key}")],
            "[{name}]"
        );
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_last_snapshot_of_a_host_is_the_files_one() {
    for backend in backends() {
        let name = backend.name();
        let env = Env::start(&backend).await;
        let now = now_ms();
        env.host("h1", "web-1").await;
        env.policy("daily", "h1", DAILY).await;
        env.snapshot("f1", "h1", "daily", now - 2 * HOUR).await;
        env.ct_policy("cts", "h1", "200").await;
        env.ct_facts("h1", &[200], false, now).await;
        env.ct_snapshot("s1", "h1", "cts", 200, now - MINUTE).await;

        let inputs = alerts::inputs::gather(env.ts.state(), now).await.unwrap();
        let last = inputs.hosts[0].last_snapshot.as_ref().unwrap();
        assert_eq!(last.time_ms, now - 2 * HOUR, "[{name}]");

        env.host("h2", "ct-only").await;
        env.ct_policy("cts2", "h2", "200").await;
        env.ct_facts("h2", &[200], false, now).await;
        env.ct_snapshot("s2", "h2", "cts2", 200, now - MINUTE).await;
        let inputs = alerts::inputs::gather(env.ts.state(), now).await.unwrap();
        let only = inputs.hosts.iter().find(|h| h.host_id == "h2").unwrap();
        assert_eq!(
            only.last_snapshot.as_ref().unwrap().time_ms,
            now - MINUTE,
            "[{name}]"
        );
        env.stop().await;
    }
}
