mod common;

use common::{Api, TestServer, backends, bearer_client};
use lodestore_server::db::now_ms;
use lodestore_server::fleet;
use reqwest::Method;
use serde_json::{Value, json};
use sqlx::AnyPool;

const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;
const DAILY: &str = "  schedule:\n    cron: \"0 2 * * *\"\n    jitter: 0m\n";
const STATS: &str = r#"{"bytes_total": 5000, "bytes_added": 700}"#;

struct Fleet {
    ts: TestServer,
    api: Api,
    repo: String,
}

impl Fleet {
    async fn start(backend: &common::Backend) -> Fleet {
        let ts = TestServer::start_with(backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        let api = ts.api().await;
        Fleet { ts, api, repo }
    }

    fn pool(&self) -> &AnyPool {
        &self.ts.state().db.pool
    }

    async fn agent(&self, id: &str, hostname: &str, last_seen_ms: i64) {
        sqlx::query(
            "INSERT INTO agents (id, hostname, repository_id, labels_json, os, arch, version, \
             status, enrolled_at, cert_serial, cert_issued_at, cert_not_after, last_seen_at) \
             VALUES ($1, $2, $3, '[]', 'linux', 'x86_64', '0.1.0', 'active', $4, 'serial', $4, $5, $6)",
        )
        .bind(id)
        .bind(hostname)
        .bind(&self.repo)
        .bind(now_ms() - 30 * DAY)
        .bind(now_ms() + 300 * DAY)
        .bind(last_seen_ms)
        .execute(self.pool())
        .await
        .unwrap();
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

    async fn job(
        &self,
        id: &str,
        host: &str,
        policy: &str,
        kind: &str,
        state: &str,
        finished_ms: Option<i64>,
    ) {
        sqlx::query(
            "INSERT INTO jobs (id, host_id, policy_name, kind, source, state, spec, queued_at, \
             finished_at) VALUES ($1, $2, $3, $4, 'schedule', $5, $6, $7, $8)",
        )
        .bind(id)
        .bind(host)
        .bind(policy)
        .bind(kind)
        .bind(state)
        .bind(Vec::<u8>::new())
        .bind(finished_ms.unwrap_or(0))
        .bind(finished_ms)
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let response = self.api.get(path).await;
        let status = response.status().as_u16();
        (status, response.json().await.unwrap())
    }

    async fn stop(self) {
        self.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fleet_overdue_host() {
    for backend in backends() {
        let fleet = Fleet::start(&backend).await;
        let seen = now_ms();
        fleet.agent("h-late", "web-1", seen).await;
        fleet.policy("daily", "h-late", DAILY).await;
        let success = now_ms() - 3 * DAY;
        fleet.snapshot("s1", "h-late", "daily", success).await;

        let (status, body) = fleet.get("/api/v1/fleet").await;
        assert_eq!(status, 200, "{body} [{}]", backend.name());
        assert_eq!(body["totals"]["hosts"], 1);
        assert_eq!(body["totals"]["overdue"], 1);
        let host = &body["hosts"][0];
        assert_eq!(host["hostname"], "web-1");
        assert_eq!(host["state"], "overdue");
        assert_eq!(host["last_success_at"], success);
        assert_eq!(host["overdue_after_secs"], 48 * 3600);
        assert_eq!(host["has_policy"], true);
        assert_eq!(host["last_snapshot"]["bytes_total"], 5000);
        assert_eq!(host["last_snapshot"]["bytes_added"], 700);
        let worst = &host["worst"];
        assert_eq!(worst["policy"], "daily");
        assert_eq!(worst["problem"], "overdue");
        assert_eq!(worst["limit_secs"], 48 * 3600);
        assert_eq!(worst["error"], Value::Null);
        let age = worst["age_secs"].as_u64().unwrap();
        assert!((3 * 86_400..3 * 86_400 + 120).contains(&age), "{age}");
        fleet.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fleet_recent_runs() {
    for backend in backends() {
        let fleet = Fleet::start(&backend).await;
        let now = now_ms();
        fleet.agent("h-a", "web-1", now).await;
        fleet.agent("h-b", "web-2", now).await;
        fleet.agent("h-none", "web-3", now).await;
        let states = [
            "failed",
            "succeeded",
            "succeeded",
            "succeeded",
            "warnings",
            "interrupted",
            "cancelled",
            "not_started",
            "succeeded",
            "failed",
            "succeeded",
            "succeeded",
        ];
        for (i, state) in states.iter().enumerate() {
            let at = now - (20 - i as i64) * HOUR;
            fleet
                .job(&format!("a{i}"), "h-a", "daily", "backup", state, Some(at))
                .await;
        }
        fleet
            .job("a-run", "h-a", "daily", "backup", "running", None)
            .await;
        fleet
            .job(
                "a-restore",
                "h-a",
                "daily",
                "restore",
                "succeeded",
                Some(now),
            )
            .await;
        fleet
            .job("b0", "h-b", "weekly", "backup", "failed", Some(now - HOUR))
            .await;

        let name = backend.name();
        let (status, body) = fleet.get("/api/v1/fleet").await;
        assert_eq!(status, 200, "{body} [{name}]");
        let by_name = |host: &str| {
            body["hosts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|h| h["hostname"] == host)
                .unwrap()
                .clone()
        };
        let a = by_name("web-1");
        let runs = a["recent_runs"].as_array().unwrap();
        assert_eq!(runs.len(), 10, "[{name}]");
        let outcomes: Vec<_> = runs
            .iter()
            .map(|r| r["outcome"].as_str().unwrap())
            .collect();
        assert_eq!(
            outcomes,
            [
                "succeeded",
                "succeeded",
                "succeeded",
                "failed",
                "skipped",
                "skipped",
                "succeeded",
                "failed",
                "succeeded",
                "succeeded"
            ],
            "[{name}]"
        );
        assert_eq!(runs[9]["finished_at"], now - 9 * HOUR, "[{name}]");
        assert_eq!(runs[0]["finished_at"], now - 18 * HOUR, "[{name}]");
        assert_eq!(runs[9]["policy"], "daily");
        assert_eq!(by_name("web-2")["recent_runs"].as_array().unwrap().len(), 1);
        assert_eq!(by_name("web-2")["recent_runs"][0]["policy"], "weekly");
        assert_eq!(by_name("web-3")["recent_runs"], json!([]));

        let (status, one) = fleet.get("/api/v1/fleet/hosts/h-a").await;
        assert_eq!(status, 200, "{one} [{name}]");
        assert_eq!(one["recent_runs"], a["recent_runs"], "[{name}]");
        fleet.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fleet_states_sum() {
    for backend in backends() {
        let fleet = Fleet::start(&backend).await;
        let now = now_ms();
        for (id, name, seen) in [
            ("h-ok", "a-ok", now),
            ("h-bare", "b-bare", now),
            ("h-offline", "c-offline", now - 3 * HOUR),
            ("h-overdue", "d-overdue", now),
            ("h-failed", "e-failed", now),
        ] {
            fleet.agent(id, name, seen).await;
        }
        for (host, policy, age) in [
            ("h-ok", "p-ok", HOUR),
            ("h-offline", "p-offline", HOUR),
            ("h-overdue", "p-overdue", 3 * DAY),
            ("h-failed", "p-failed", HOUR),
        ] {
            fleet.policy(policy, host, DAILY).await;
            fleet
                .snapshot(&format!("s-{host}"), host, policy, now - age)
                .await;
        }
        fleet
            .failed_job("j1", "h-failed", "p-failed", now - HOUR / 2, "disk full")
            .await;

        let overview = fleet::overview(fleet.ts.state(), now, 0).await.unwrap();
        let states: Vec<_> = overview
            .hosts
            .iter()
            .map(|h| serde_json::to_value(h.state).unwrap())
            .collect();
        assert_eq!(
            states,
            ["failed", "overdue", "offline", "not_protected", "healthy"],
            "[{}]",
            backend.name()
        );
        let t = &overview.totals;
        assert_eq!(
            (
                t.hosts,
                t.failed,
                t.overdue,
                t.offline,
                t.not_protected,
                t.healthy
            ),
            (5, 1, 1, 1, 1, 1)
        );
        assert_eq!(
            t.hosts,
            t.healthy + t.overdue + t.failed + t.offline + t.not_protected
        );
        let worst = overview.hosts[0].worst.as_ref().unwrap();
        assert_eq!(worst.error.as_deref(), Some("disk full"));
        assert_eq!(overview.hosts[2].worst.as_ref().map(|w| &w.policy), None);
        assert!(!overview.hosts[3].has_policy);
        fleet.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fleet_restart_grace_keeps_agent_online() {
    for backend in backends() {
        let fleet = Fleet::start(&backend).await;
        let now = now_ms();
        fleet.agent("h1", "web-1", now - 3 * HOUR).await;
        fleet.policy("daily", "h1", DAILY).await;
        fleet.snapshot("s1", "h1", "daily", now - HOUR).await;

        let fresh = fleet::overview(fleet.ts.state(), now, now - 60_000)
            .await
            .unwrap();
        assert_eq!(fresh.totals.offline, 0, "[{}]", backend.name());
        assert_eq!(fresh.totals.healthy, 1);
        let later = fleet::overview(fleet.ts.state(), now, now - HOUR)
            .await
            .unwrap();
        assert_eq!(later.totals.offline, 1);
        fleet.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fleet_manual_only_not_protected() {
    for backend in backends() {
        let fleet = Fleet::start(&backend).await;
        fleet.agent("h1", "web-1", now_ms()).await;
        fleet.policy("manual", "h1", "").await;
        let success = now_ms() - HOUR;
        fleet.snapshot("s1", "h1", "manual", success).await;

        let (status, body) = fleet.get("/api/v1/fleet/hosts/h1").await;
        assert_eq!(status, 200, "{body} [{}]", backend.name());
        assert_eq!(body["state"], "not_protected");
        assert_eq!(body["has_policy"], true);
        assert_eq!(body["last_success_at"], success);
        assert_eq!(body["overdue_after_secs"], Value::Null);
        assert_eq!(body["worst"], Value::Null);
        fleet.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fleet_storage_from_repo_usage() {
    for backend in backends() {
        let fleet = Fleet::start(&backend).await;
        let (_, empty) = fleet.get("/api/v1/fleet").await;
        let storage = &empty["storage"];
        assert_eq!(storage["repositories"], 1, "[{}]", backend.name());
        assert_eq!(storage["counted"], 0);
        assert_eq!(storage["physical_bytes"], Value::Null);
        assert_eq!(storage["dedup_ratio"], Value::Null);

        sqlx::query(
            "INSERT INTO repo_usage (repository_id, physical_bytes, logical_bytes, packs, \
             objects, counted_at) VALUES ($1, 1000, 0, 0, 3, 41)",
        )
        .bind(&fleet.repo)
        .execute(fleet.pool())
        .await
        .unwrap();
        let (_, no_snapshots) = fleet.get("/api/v1/fleet").await;
        assert_eq!(no_snapshots["storage"]["physical_bytes"], 1000);
        assert_eq!(no_snapshots["storage"]["dedup_ratio"], Value::Null);
        sqlx::query("DELETE FROM repo_usage WHERE repository_id = $1")
            .bind(&fleet.repo)
            .execute(fleet.pool())
            .await
            .unwrap();

        sqlx::query(
            "INSERT INTO repo_usage (repository_id, physical_bytes, logical_bytes, packs, \
             objects, counted_at) VALUES ($1, 1000, 3000, 2, 10, 42)",
        )
        .bind(&fleet.repo)
        .execute(fleet.pool())
        .await
        .unwrap();
        let (_, body) = fleet.get("/api/v1/fleet").await;
        let storage = &body["storage"];
        assert_eq!(storage["counted"], 1);
        assert_eq!(storage["physical_bytes"], 1000);
        assert_eq!(storage["logical_bytes"], 3000);
        assert_eq!(storage["dedup_ratio"], 3.0);
        assert_eq!(storage["counted_at"], 42);
        fleet.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fleet_host_404() {
    for backend in backends() {
        let fleet = Fleet::start(&backend).await;
        fleet.agent("h1", "web-1", now_ms()).await;
        fleet.agent("h2", "web-2", now_ms()).await;
        sqlx::query("UPDATE agents SET status = 'revoked' WHERE id = 'h2'")
            .execute(fleet.pool())
            .await
            .unwrap();

        let (status, body) = fleet.get("/api/v1/fleet/hosts/h1").await;
        assert_eq!(status, 200, "[{}]", backend.name());
        assert_eq!(body["id"], "h1");
        for id in ["nope", "h2"] {
            let (status, body) = fleet.get(&format!("/api/v1/fleet/hosts/{id}")).await;
            assert_eq!(status, 404, "{id} [{}]", backend.name());
            assert_eq!(body["code"], "not_found");
        }
        let (_, all) = fleet.get("/api/v1/fleet").await;
        assert_eq!(all["totals"]["hosts"], 1);
        fleet.stop().await;
    }
}

struct NewAlert<'a> {
    id: &'a str,
    kind: &'a str,
    severity: &'a str,
    state: &'a str,
    host: Option<&'a str>,
    repository: bool,
    cert: Option<&'a str>,
    title: &'a str,
    started_at: i64,
    resolved_at: Option<i64>,
}

impl<'a> NewAlert<'a> {
    fn firing(id: &'a str, kind: &'a str, title: &'a str, started_at: i64) -> Self {
        NewAlert {
            id,
            kind,
            severity: "critical",
            state: "firing",
            host: None,
            repository: false,
            cert: None,
            title,
            started_at,
            resolved_at: None,
        }
    }

    fn host(self, host: &'a str) -> Self {
        NewAlert {
            host: Some(host),
            ..self
        }
    }

    fn resolved(self, at: i64) -> Self {
        NewAlert {
            state: "resolved",
            resolved_at: Some(at),
            ..self
        }
    }
}

impl Fleet {
    async fn alert(&self, alert: NewAlert<'_>) {
        sqlx::query(
            "INSERT INTO alerts (id, type, key, severity, state, host_id, repository_id, \
             policy_name, check_level, cert, title, detail, facts_json, started_at, updated_at, \
             resolved_at) VALUES ($1, $2, $3, $4, $5, $6, $7, NULL, $8, $9, $10, 'Details', \
             '{\"error\": \"boom\"}', $11, $11, $12)",
        )
        .bind(alert.id)
        .bind(alert.kind)
        .bind(format!("{}:{}", alert.kind, alert.id))
        .bind(alert.severity)
        .bind(alert.state)
        .bind(alert.host)
        .bind(alert.repository.then(|| self.repo.clone()))
        .bind(alert.repository.then_some("structure"))
        .bind(alert.cert)
        .bind(alert.title)
        .bind(alert.started_at)
        .bind(alert.resolved_at)
        .execute(self.pool())
        .await
        .unwrap();
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

    async fn put_silence(&self, host: &str, duration: &str) -> (u16, Value) {
        let response = reqwest::Client::new()
            .put(format!(
                "{}/api/v1/hosts/{host}/silence",
                self.ts.rest_url()
            ))
            .header("cookie", self.api.cookie())
            .header("x-csrf-token", &self.api.csrf)
            .json(&json!({ "duration": duration }))
            .send()
            .await
            .unwrap();
        (
            response.status().as_u16(),
            response.json().await.unwrap_or(Value::Null),
        )
    }
}

async fn bearer(ts: &TestServer, method: Method, path: &str, scopes: &[&str]) -> (u16, Value) {
    let client = bearer_client(&ts.issue_token(scopes).await);
    let response = client
        .request(method, format!("{}{path}", ts.rest_url()))
        .json(&json!({ "duration": "hour" }))
        .send()
        .await
        .unwrap();
    (
        response.status().as_u16(),
        response.json().await.unwrap_or(Value::Null),
    )
}

fn ids(body: &Value) -> Vec<&str> {
    body["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect()
}

async fn two_hosts(fleet: &Fleet) {
    fleet.agent("h1", "web-1", now_ms()).await;
    fleet.agent("h2", "web-2", now_ms()).await;
}

async fn four_alerts(fleet: &Fleet) {
    let now = now_ms();
    for alert in [
        NewAlert::firing(
            "a1",
            "backup_failed",
            "Backup failed on web-1",
            now - 3 * HOUR,
        )
        .host("h1"),
        NewAlert::firing("a2", "agent_offline", "web-2 is offline", now - 2 * HOUR).host("h2"),
        NewAlert {
            cert: Some("grpc"),
            severity: "warning",
            ..NewAlert::firing(
                "a3",
                "certificate_expiring",
                "Server certificate expires soon",
                now - HOUR,
            )
        },
        NewAlert::firing(
            "a4",
            "backup_failed",
            "Backup failed on web-1",
            now - 10 * HOUR,
        )
        .host("h1")
        .resolved(now - 9 * HOUR),
    ] {
        fleet.alert(alert).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_api_list_and_count() {
    for backend in backends() {
        let fleet = Fleet::start(&backend).await;
        two_hosts(&fleet).await;
        four_alerts(&fleet).await;

        let (status, body) = fleet.get("/api/v1/alerts").await;
        assert_eq!(status, 200, "{body} [{}]", backend.name());
        assert_eq!(ids(&body), ["a3", "a2", "a1"]);
        assert_eq!(body["total"], 3);
        let first = &body["alerts"][2];
        assert_eq!(first["title"], "Backup failed on web-1");
        assert_eq!(first["detail"], "Details");
        assert_eq!(first["type"], "backup_failed");
        assert_eq!(first["severity"], "critical");
        assert_eq!(first["state"], "firing");
        assert_eq!(first["host"], json!({ "id": "h1", "hostname": "web-1" }));
        assert_eq!(first["facts"]["error"], "boom");
        assert_eq!(first["silenced"], false);
        let cert = &body["alerts"][0];
        assert_eq!(cert["cert"], "grpc");
        assert_eq!(cert["host"], Value::Null);

        let (_, all) = fleet.get("/api/v1/alerts?state=all").await;
        assert_eq!(ids(&all), ["a3", "a2", "a1", "a4"]);
        assert_eq!(all["alerts"][3]["state"], "resolved");
        assert!(all["alerts"][3]["resolved_at"].is_i64());
        let (_, resolved) = fleet.get("/api/v1/alerts?state=resolved").await;
        assert_eq!(ids(&resolved), ["a4"]);

        let (_, count) = fleet.get("/api/v1/alerts/count").await;
        assert_eq!(count, json!({ "active": 3, "unsilenced": 3 }));

        for path in ["/api/v1/alerts", "/api/v1/alerts/count"] {
            let (denied, body) = bearer(&fleet.ts, Method::GET, path, &["jobs:read"]).await;
            assert_eq!(denied, 403, "{path} {body}");
            let (allowed, _) = bearer(&fleet.ts, Method::GET, path, &["hosts:read"]).await;
            assert_eq!(allowed, 200, "{path}");
        }
        fleet.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_api_filters() {
    for backend in backends() {
        let fleet = Fleet::start(&backend).await;
        two_hosts(&fleet).await;
        four_alerts(&fleet).await;

        let (_, by_host) = fleet.get("/api/v1/alerts?state=all&host_id=h1").await;
        assert_eq!(ids(&by_host), ["a1", "a4"], "[{}]", backend.name());
        assert_eq!(by_host["total"], 2);
        let (_, active_host) = fleet.get("/api/v1/alerts?host_id=h2").await;
        assert_eq!(ids(&active_host), ["a2"]);

        let (_, page) = fleet.get("/api/v1/alerts?limit=1").await;
        assert_eq!(ids(&page), ["a3"]);
        assert_eq!(page["total"], 3);
        let (_, page) = fleet.get("/api/v1/alerts?limit=1&before=a3").await;
        assert_eq!(ids(&page), ["a2"]);
        let (_, page) = fleet.get("/api/v1/alerts?limit=5&before=a2").await;
        assert_eq!(ids(&page), ["a1"]);
        let (_, page) = fleet.get("/api/v1/alerts?before=a1").await;
        assert!(ids(&page).is_empty());
        let (_, page) = fleet
            .get("/api/v1/alerts?state=all&host_id=h1&limit=1&before=a1")
            .await;
        assert_eq!(ids(&page), ["a4"]);

        let same = now_ms() + HOUR;
        for id in ["t1", "t2", "t3"] {
            fleet
                .alert(NewAlert::firing(id, "backup_failed", "Tied", same).host("h2"))
                .await;
        }
        let (_, first) = fleet.get("/api/v1/alerts?limit=2").await;
        assert_eq!(ids(&first), ["t3", "t2"]);
        let (_, second) = fleet.get("/api/v1/alerts?limit=2&before=t2").await;
        assert_eq!(ids(&second), ["t1", "a3"]);
        fleet.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_silence_api_scopes() {
    for backend in backends() {
        let fleet = Fleet::start(&backend).await;
        two_hosts(&fleet).await;
        let path = "/api/v1/hosts/h1/silence";

        for method in [Method::PUT, Method::DELETE] {
            for scope in ["hosts:read", "hosts:write"] {
                let (status, _) = bearer(&fleet.ts, method.clone(), path, &[scope]).await;
                assert_eq!(status, 403, "{method} {scope} [{}]", backend.name());
            }
        }

        let before = now_ms();
        let (status, hour) = bearer(&fleet.ts, Method::PUT, path, &["alerts:write"]).await;
        assert_eq!(status, 200, "{hour}");
        assert_eq!(hour["host_id"], "h1");
        assert_eq!(hour["created_by"], "token:test");
        let until = hour["until"].as_i64().unwrap();
        assert!(
            (before + HOUR..before + HOUR + 60_000).contains(&until),
            "{until}"
        );

        let (status, forever) = fleet.put_silence("h1", "forever").await;
        assert_eq!(status, 200);
        assert_eq!(forever["until"], Value::Null);
        assert_eq!(forever["created_by"], "admin");
        let (_, day) = fleet.put_silence("h1", "day").await;
        let until = day["until"].as_i64().unwrap();
        assert!((before + 24 * HOUR..before + 24 * HOUR + 60_000).contains(&until));
        let (_, week) = fleet.put_silence("h1", "week").await;
        assert!(week["until"].as_i64().unwrap() > before + 6 * DAY);
        let rows = sqlx::query("SELECT host_id FROM alert_silences")
            .fetch_all(fleet.pool())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);

        let (status, body) = fleet.put_silence("nope", "hour").await;
        assert_eq!(status, 404, "{body}");
        let (status, _) = bearer(
            &fleet.ts,
            Method::DELETE,
            "/api/v1/hosts/nope/silence",
            &["alerts:write"],
        )
        .await;
        assert_eq!(status, 404);
        let (status, _) = bearer(&fleet.ts, Method::DELETE, path, &["alerts:write"]).await;
        assert_eq!(status, 204);
        let rows = sqlx::query("SELECT host_id FROM alert_silences")
            .fetch_all(fleet.pool())
            .await
            .unwrap();
        assert!(rows.is_empty());
        fleet.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_silence_expired() {
    for backend in backends() {
        let fleet = Fleet::start(&backend).await;
        two_hosts(&fleet).await;
        four_alerts(&fleet).await;
        let now = now_ms();
        fleet.silence_row("h1", Some(now - HOUR)).await;
        fleet.silence_row("h2", Some(now + HOUR)).await;

        let (_, body) = fleet.get("/api/v1/alerts").await;
        let by_id = |id: &str| {
            body["alerts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["id"] == id)
                .unwrap()
                .clone()
        };
        assert_eq!(by_id("a1")["silenced"], false, "[{}]", backend.name());
        assert_eq!(by_id("a1")["silenced_until"], Value::Null);
        assert_eq!(by_id("a2")["silenced"], true);
        assert_eq!(by_id("a2")["silenced_until"], now + HOUR);
        let (_, count) = fleet.get("/api/v1/alerts/count").await;
        assert_eq!(count, json!({ "active": 3, "unsilenced": 2 }));

        let (_, fleet_body) = fleet.get("/api/v1/fleet").await;
        let host = |name: &str| {
            fleet_body["hosts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|h| h["hostname"] == name)
                .unwrap()
                .clone()
        };
        assert_eq!(host("web-1")["silenced"], false);
        assert_eq!(host("web-2")["silenced"], true);
        fleet.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alerts_api_repo_and_server_not_silenced() {
    for backend in backends() {
        let fleet = Fleet::start(&backend).await;
        two_hosts(&fleet).await;
        four_alerts(&fleet).await;
        fleet
            .alert(NewAlert {
                repository: true,
                ..NewAlert::firing(
                    "a5",
                    "check_failed",
                    "Integrity check failed in main",
                    now_ms(),
                )
            })
            .await;
        fleet.silence_row("h1", None).await;

        let (_, body) = fleet.get("/api/v1/alerts").await;
        let alerts = body["alerts"].as_array().unwrap();
        let find = |id: &str| alerts.iter().find(|a| a["id"] == id).unwrap();
        assert_eq!(find("a1")["silenced"], true, "[{}]", backend.name());
        assert_eq!(find("a1")["silenced_until"], Value::Null);
        assert_eq!(find("a2")["silenced"], false);
        assert_eq!(find("a3")["silenced"], false);
        let repo = find("a5");
        assert_eq!(repo["silenced"], false);
        assert_eq!(repo["repository"]["name"], "main");
        assert_eq!(repo["check_level"], "structure");
        assert_eq!(repo["host"], Value::Null);
        let (_, count) = fleet.get("/api/v1/alerts/count").await;
        assert_eq!(count, json!({ "active": 4, "unsilenced": 3 }));

        let (status, _) = bearer(
            &fleet.ts,
            Method::DELETE,
            "/api/v1/hosts/h1/silence",
            &["alerts:write"],
        )
        .await;
        assert_eq!(status, 204);
        let (_, after) = fleet.get("/api/v1/alerts").await;
        assert!(
            after["alerts"]
                .as_array()
                .unwrap()
                .iter()
                .all(|a| a["silenced"] == false)
        );
        fleet.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fleet_silenced_host_keeps_state() {
    for backend in backends() {
        let fleet = Fleet::start(&backend).await;
        fleet.agent("h1", "web-1", now_ms()).await;
        fleet.policy("daily", "h1", DAILY).await;
        fleet
            .snapshot("s1", "h1", "daily", now_ms() - 3 * DAY)
            .await;

        let (status, silence) = fleet.put_silence("h1", "hour").await;
        assert_eq!(status, 200, "{silence} [{}]", backend.name());
        let (_, body) = fleet.get("/api/v1/fleet").await;
        let host = &body["hosts"][0];
        assert_eq!(host["state"], "overdue");
        assert_eq!(host["silenced"], true);
        assert_eq!(host["silenced_until"], silence["until"]);
        assert_eq!(body["totals"]["overdue"], 1);
        let (_, one) = fleet.get("/api/v1/fleet/hosts/h1").await;
        assert_eq!(one["silenced"], true);

        let (status, _) = bearer(
            &fleet.ts,
            Method::DELETE,
            "/api/v1/hosts/h1/silence",
            &["alerts:write"],
        )
        .await;
        assert_eq!(status, 204);
        let (_, after) = fleet.get("/api/v1/fleet/hosts/h1").await;
        assert_eq!(after["silenced"], false);
        assert_eq!(after["silenced_until"], Value::Null);
        assert_eq!(after["state"], "overdue");
        fleet.stop().await;
    }
}
