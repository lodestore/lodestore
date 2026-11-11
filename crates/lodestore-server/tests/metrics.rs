mod common;

use common::{TestServer, backends, bearer_client};
use lodestore_server::db::now_ms;
use serde_json::{Value, json};
use sqlx::AnyPool;

const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;
const DAILY: &str = "  schedule:\n    cron: \"0 2 * * *\"\n    jitter: 0m\n";
const STATS: &str = r#"{"bytes_total": 5000, "bytes_added": 700}"#;

struct Stand {
    ts: TestServer,
    repo: String,
}

impl Stand {
    async fn start(backend: &common::Backend) -> Stand {
        let ts = TestServer::start_with(backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        Stand { ts, repo }
    }

    fn pool(&self) -> &AnyPool {
        &self.ts.state().db.pool
    }

    async fn agent(&self, id: &str, hostname: &str) {
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
        .bind(now_ms())
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn agent_update(&self, id: &str, host: &str, state: &str, target: &str) {
        sqlx::query(
            "INSERT INTO agent_rollouts (id, target_version, state, canary, wave_size, \
             pause_after, created_by, created_at) VALUES ($1, $2, 'done', 1, 10, 0, 'admin', $3)",
        )
        .bind(format!("r-{id}"))
        .bind(target)
        .bind(now_ms())
        .execute(self.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO agent_updates (id, rollout_id, host_id, arch, from_version, \
             target_version, wave, state, reason, queued_at, finished_at) \
             VALUES ($1, $2, $3, 'x86_64', '0.1.0', $4, 0, $5, 'rolled back', $6, $6)",
        )
        .bind(id)
        .bind(format!("r-{id}"))
        .bind(host)
        .bind(target)
        .bind(state)
        .bind(now_ms())
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn policy(&self, name: &str, host: &str) {
        let yaml = format!(
            "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: {name}\n\
             spec:\n  repository: main\n  selector:\n    hosts: [{host}]\n  paths: [/etc]\n{DAILY}"
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

    async fn scrape(&self) -> String {
        let token = self.ts.issue_token(&["metrics:read"]).await;
        let response = bearer_client(&token)
            .get(format!("{}/metrics", self.ts.rest_url()))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        response.text().await.unwrap()
    }

    async fn stop(self) {
        self.ts.stop().await;
    }
}

fn value(text: &str, series: &str) -> f64 {
    let prefix = format!("{series} ");
    text.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no series {series} in:\n{text}"))
        .parse()
        .unwrap()
}

fn has(text: &str, series: &str) -> bool {
    let prefix = format!("{series} ");
    text.lines().any(|line| line.starts_with(&prefix))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_requires_scope() {
    for backend in backends() {
        let stand = Stand::start(&backend).await;
        let url = format!("{}/metrics", stand.ts.rest_url());

        let anonymous = reqwest::get(&url).await.unwrap();
        assert_eq!(anonymous.status(), 401, "[{}]", backend.name());

        let wrong = stand.ts.issue_token(&["jobs:read"]).await;
        let response = bearer_client(&wrong).get(&url).send().await.unwrap();
        assert_eq!(response.status(), 403);

        let right = stand.ts.issue_token(&["metrics:read"]).await;
        let response = bearer_client(&right).get(&url).send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.headers()["content-type"],
            "text/plain; version=0.0.4; charset=utf-8"
        );
        assert!(
            response
                .text()
                .await
                .unwrap()
                .contains("lodestore_build_info")
        );
        stand.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_refuses_revoked_and_expired_tokens() {
    for backend in backends() {
        let stand = Stand::start(&backend).await;
        let url = format!("{}/metrics", stand.ts.rest_url());
        let api = stand.ts.api().await;
        let issue = |name: &'static str| {
            let api = &api;
            async move {
                let created: Value = api
                    .post(
                        "/api/v1/tokens",
                        &json!({ "name": name, "scopes": ["metrics:read"], "expires_in_days": null }),
                    )
                    .await
                    .json()
                    .await
                    .unwrap();
                (
                    created["item"]["id"].as_str().unwrap().to_string(),
                    created["token"].as_str().unwrap().to_string(),
                )
            }
        };
        let scrape = |token: String| {
            let url = url.clone();
            async move {
                let response = bearer_client(&token).get(&url).send().await.unwrap();
                response.status().as_u16()
            }
        };

        let (revoked, revoked_secret) = issue("revoked").await;
        assert_eq!(scrape(revoked_secret.clone()).await, 200);
        let response = api
            .post(&format!("/api/v1/tokens/{revoked}/revoke"), &json!({}))
            .await;
        assert_eq!(response.status(), 204);
        assert_eq!(scrape(revoked_secret).await, 401, "[{}]", backend.name());

        let (expired, expired_secret) = issue("expired").await;
        assert_eq!(scrape(expired_secret.clone()).await, 200);
        sqlx::query("UPDATE api_tokens SET expires_at = 1 WHERE id = $1")
            .bind(&expired)
            .execute(stand.pool())
            .await
            .unwrap();
        assert_eq!(scrape(expired_secret).await, 401, "[{}]", backend.name());
        stand.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_host_and_backup_series() {
    for backend in backends() {
        let stand = Stand::start(&backend).await;
        stand.agent("h-late", "web-1").await;
        stand.policy("daily", "h-late").await;
        let success = now_ms() - 3 * DAY;
        stand.snapshot("s1", "h-late", "daily", success).await;

        let text = stand.scrape().await;
        assert_eq!(value(&text, r#"lodestore_hosts{state="overdue"}"#), 1.0);
        for state in ["healthy", "failed", "offline", "not_protected"] {
            assert_eq!(
                value(&text, &format!(r#"lodestore_hosts{{state="{state}"}}"#)),
                0.0
            );
        }
        let pair = r#"{host_id="h-late",host="web-1",policy="daily"}"#;
        assert_eq!(
            value(&text, &format!("lodestore_backup_overdue{pair}")),
            1.0
        );
        assert_eq!(
            value(&text, &format!("lodestore_backup_last_status{pair}")),
            1.0
        );
        assert_eq!(
            value(
                &text,
                &format!("lodestore_backup_overdue_after_seconds{pair}")
            ),
            48.0 * 3600.0
        );
        let at = value(
            &text,
            &format!("lodestore_backup_last_success_timestamp_seconds{pair}"),
        );
        assert!((at - success as f64 / 1000.0).abs() < 1.0, "{at}");
        let host = r#"{host_id="h-late",host="web-1"}"#;
        assert_eq!(value(&text, &format!("lodestore_host_agent_up{host}")), 0.0);
        assert!(has(
            &text,
            &format!("lodestore_host_cert_expiry_timestamp_seconds{host}")
        ));
        assert!(has(
            &text,
            &format!("lodestore_host_agent_last_seen_timestamp_seconds{host}")
        ));
        stand.stop().await;
    }
}

impl Stand {
    async fn job(&self, id: &str, host: &str, policy: &str, state: &str, span: (i64, i64)) {
        let (started, finished) = span;
        sqlx::query(
            "INSERT INTO jobs (id, host_id, policy_name, source, state, spec, queued_at, \
             started_at, finished_at) VALUES ($1, $2, $3, 'schedule', $4, $5, $6, $6, $7)",
        )
        .bind(id)
        .bind(host)
        .bind(policy)
        .bind(state)
        .bind(Vec::<u8>::new())
        .bind(started)
        .bind((state != "running").then_some(finished))
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn usage(&self, physical: i64, logical: i64) {
        sqlx::query(
            "INSERT INTO repo_usage (repository_id, physical_bytes, logical_bytes, packs, \
             objects, counted_at) VALUES ($1, $2, $3, 4, 9, $4)",
        )
        .bind(&self.repo)
        .bind(physical)
        .bind(logical)
        .bind(now_ms() - HOUR)
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn pack(&self, id: &str, checked_at: Option<i64>) {
        sqlx::query(
            "INSERT INTO pack_checks (repository_id, pack_id, first_seen_at, checked_at, bad) \
             VALUES ($1, $2, $3, $4, 0)",
        )
        .bind(&self.repo)
        .bind(id)
        .bind(now_ms() - 40 * DAY)
        .bind(checked_at)
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn check_run(&self, id: &str, level: &str, state: &str, findings: i64, ended_ago: i64) {
        let finished = now_ms() - ended_ago;
        sqlx::query(
            "INSERT INTO check_runs (id, repository_id, level, origin, started_by, state, \
             findings_total, created_at, started_at, finished_at) \
             VALUES ($1, $2, $3, 'manual', 'test', $4, $5, $6, $6, $7)",
        )
        .bind(id)
        .bind(&self.repo)
        .bind(level)
        .bind(state)
        .bind(findings)
        .bind(finished - 90_000)
        .bind(finished)
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn gc_cycle(&self, id: &str, state: &str, finished_ago: i64) {
        let finished = now_ms() - finished_ago;
        sqlx::query(
            "INSERT INTO gc_cycles (id, repository_id, state, dry_run, origin, started_by, \
             settings_json, created_at, finished_at) VALUES ($1, $2, $3, 0, 'schedule', 'test', \
             '{}', $4, $4)",
        )
        .bind(id)
        .bind(&self.repo)
        .bind(state)
        .bind(finished)
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn firing_alert(&self, id: &str, kind: &str, host: &str) {
        sqlx::query(
            "INSERT INTO alerts (id, type, key, severity, state, host_id, title, detail, \
             facts_json, started_at, updated_at) VALUES ($1, $2, $3, 'critical', 'firing', $4, \
             'T', 'D', '{}', $5, $5)",
        )
        .bind(id)
        .bind(kind)
        .bind(format!("{kind}:{host}"))
        .bind(host)
        .bind(now_ms())
        .execute(self.pool())
        .await
        .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_jobs_series() {
    for backend in backends() {
        let stand = Stand::start(&backend).await;
        stand.agent("h1", "web-1").await;
        let now = now_ms();
        stand
            .job("j1", "h1", "daily", "running", (now - HOUR, now))
            .await;
        stand
            .job(
                "j2",
                "h1",
                "daily",
                "failed",
                (now - 3 * HOUR, now - 2 * HOUR),
            )
            .await;
        stand
            .job(
                "j3",
                "h1",
                "daily",
                "succeeded",
                (now - 31 * HOUR, now - 30 * HOUR),
            )
            .await;
        stand.gc_cycle("g1", "done", 1000).await;
        stand.gc_cycle("g2", "sweeping", 0).await;

        let text = stand.scrape().await;
        let name = backend.name();
        assert_eq!(
            value(&text, r#"lodestore_jobs{kind="backup",state="running"}"#),
            1.0,
            "{name}"
        );
        assert_eq!(
            value(&text, r#"lodestore_jobs{kind="backup",state="succeeded"}"#),
            1.0
        );
        assert_eq!(
            value(&text, r#"lodestore_jobs{kind="backup",state="queued"}"#),
            0.0
        );
        assert_eq!(
            value(&text, r#"lodestore_jobs{kind="gc",state="running"}"#),
            1.0
        );
        assert_eq!(
            value(
                &text,
                r#"lodestore_jobs_finished_24h{kind="backup",state="failed"}"#
            ),
            1.0
        );
        assert_eq!(
            value(
                &text,
                r#"lodestore_jobs_finished_24h{kind="backup",state="succeeded"}"#
            ),
            0.0
        );
        assert_eq!(
            value(
                &text,
                r#"lodestore_jobs_finished_24h{kind="gc",state="succeeded"}"#
            ),
            1.0
        );
        assert!(!has(
            &text,
            r#"lodestore_jobs_finished_24h{kind="backup",state="running"}"#
        ));
        stand.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_backup_details() {
    for backend in backends() {
        let stand = Stand::start(&backend).await;
        stand.agent("h1", "web-1").await;
        stand.policy("daily", "h1").await;
        let done = now_ms() - 3 * HOUR;
        stand.snapshot("s1", "h1", "daily", done).await;
        stand
            .job("j1", "h1", "daily", "succeeded", (done - 300_000, done))
            .await;

        let pair = r#"{host_id="h1",host="web-1",policy="daily"}"#;
        let text = stand.scrape().await;
        assert_eq!(
            value(
                &text,
                &format!("lodestore_backup_last_duration_seconds{pair}")
            ),
            300.0
        );
        assert_eq!(
            value(&text, &format!("lodestore_backup_last_bytes_added{pair}")),
            700.0
        );
        assert_eq!(
            value(&text, &format!("lodestore_backup_last_bytes_total{pair}")),
            5000.0
        );
        assert_eq!(
            value(&text, &format!("lodestore_backup_last_status{pair}")),
            1.0
        );
        assert_eq!(
            value(&text, &format!("lodestore_backup_overdue{pair}")),
            0.0
        );

        stand
            .job(
                "j2",
                "h1",
                "daily",
                "failed",
                (now_ms() - HOUR, now_ms() - HOUR / 2),
            )
            .await;
        let text = stand.scrape().await;
        assert_eq!(
            value(&text, &format!("lodestore_backup_last_status{pair}")),
            0.0
        );
        assert_eq!(value(&text, r#"lodestore_hosts{state="failed"}"#), 1.0);
        stand.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_repository_series() {
    for backend in backends() {
        let stand = Stand::start(&backend).await;
        let text = stand.scrape().await;
        assert!(text.contains("# TYPE lodestore_repository_physical_bytes gauge"));
        assert!(!text.contains(r#"repository="main""#));

        stand.usage(1000, 3400).await;
        stand.pack("p1", Some(now_ms() - DAY)).await;
        stand.pack("p2", Some(now_ms() - 40 * DAY)).await;
        stand.pack("p3", None).await;
        stand.pack("p4", None).await;
        let text = stand.scrape().await;
        let repo = r#"{repository="main"}"#;
        assert_eq!(
            value(&text, &format!("lodestore_repository_physical_bytes{repo}")),
            1000.0
        );
        assert_eq!(
            value(&text, &format!("lodestore_repository_logical_bytes{repo}")),
            3400.0
        );
        assert_eq!(
            value(&text, &format!("lodestore_repository_dedup_ratio{repo}")),
            3.4
        );
        let counted = value(
            &text,
            &format!("lodestore_repository_counted_timestamp_seconds{repo}"),
        );
        assert!((counted - (now_ms() - HOUR) as f64 / 1000.0).abs() < 5.0);
        assert_eq!(
            value(&text, &format!("lodestore_pack_coverage_ratio{repo}")),
            0.25
        );
        stand.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_check_series() {
    for backend in backends() {
        let stand = Stand::start(&backend).await;
        stand.check_run("c1", "packs", "done", 0, 2 * DAY).await;
        stand.check_run("c2", "packs", "failed", 2, HOUR).await;

        let text = stand.scrape().await;
        let packs = r#"{repository="main",level="packs"}"#;
        assert_eq!(
            value(&text, &format!("lodestore_check_last_status{packs}")),
            0.0
        );
        assert_eq!(
            value(&text, &format!("lodestore_check_findings{packs}")),
            2.0
        );
        assert_eq!(
            value(
                &text,
                &format!("lodestore_check_last_duration_seconds{packs}")
            ),
            90.0
        );
        let ok = value(
            &text,
            &format!("lodestore_check_last_success_timestamp_seconds{packs}"),
        );
        assert!((ok - (now_ms() - 2 * DAY) as f64 / 1000.0).abs() < 5.0);
        assert!(!text.contains(r#"level="structure""#));
        stand.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_cert_and_alerts() {
    for backend in backends() {
        let stand = Stand::start(&backend).await;
        stand.agent("h1", "web-1").await;
        let (not_after, _) =
            lodestore_server::grpc::server_cert_not_after(stand.ts.state()).unwrap();
        let text = stand.scrape().await;
        assert_eq!(
            value(
                &text,
                r#"lodestore_server_cert_expiry_timestamp_seconds{cert="grpc"}"#
            ),
            not_after as f64 / 1000.0
        );
        assert_eq!(
            value(
                &text,
                r#"lodestore_alerts_active{type="backup_failed",silenced="false"}"#
            ),
            0.0
        );

        stand.firing_alert("a1", "backup_failed", "h1").await;
        let text = stand.scrape().await;
        assert_eq!(
            value(
                &text,
                r#"lodestore_alerts_active{type="backup_failed",silenced="false"}"#
            ),
            1.0
        );

        sqlx::query(
            "INSERT INTO alert_silences (host_id, until_ms, created_by, created_at) \
             VALUES ('h1', NULL, 'test', $1)",
        )
        .bind(now_ms())
        .execute(stand.pool())
        .await
        .unwrap();
        let text = stand.scrape().await;
        assert_eq!(
            value(
                &text,
                r#"lodestore_alerts_active{type="backup_failed",silenced="true"}"#
            ),
            1.0
        );
        assert_eq!(
            value(
                &text,
                r#"lodestore_alerts_active{type="backup_failed",silenced="false"}"#
            ),
            0.0
        );
        stand.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_agent_version_and_update_failed() {
    for backend in backends() {
        let stand = Stand::start(&backend).await;
        stand.agent("h1", "web-1").await;
        stand.agent("h2", "db-1").await;
        stand.agent("h3", "gone-1").await;
        sqlx::query("UPDATE agents SET version = '0.2.0' WHERE id = 'h2'")
            .execute(stand.pool())
            .await
            .unwrap();
        sqlx::query("UPDATE agents SET status = 'revoked' WHERE id = 'h3'")
            .execute(stand.pool())
            .await
            .unwrap();
        stand.agent_update("u1", "h1", "rolled_back", "0.2.0").await;

        let text = stand.scrape().await;
        for (id, host, version) in [("h1", "web-1", "0.1.0"), ("h2", "db-1", "0.2.0")] {
            let series = format!(
                r#"lodestore_host_agent_info{{host_id="{id}",host="{host}",version="{version}"}}"#
            );
            assert_eq!(value(&text, &series), 1.0, "{text}");
        }
        assert!(!text.contains("gone-1"), "{text}");
        assert_eq!(
            value(
                &text,
                r#"lodestore_host_agent_update_failed{host_id="h1",host="web-1"}"#
            ),
            1.0
        );
        assert_eq!(
            value(
                &text,
                r#"lodestore_host_agent_update_failed{host_id="h2",host="db-1"}"#
            ),
            0.0
        );
        assert_eq!(
            value(
                &text,
                r#"lodestore_alerts_active{type="agent_update_failed",silenced="false"}"#
            ),
            0.0
        );
        assert_eq!(
            value(
                &text,
                r#"lodestore_alerts_active{type="snapshot_quarantined",silenced="false"}"#
            ),
            0.0
        );
        assert_eq!(
            value(
                &text,
                r#"lodestore_alerts_active{type="repository_unavailable",silenced="false"}"#
            ),
            0.0
        );
        stand.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_unique_and_typed() {
    for backend in backends() {
        let stand = Stand::start(&backend).await;
        stand.agent("h1", r#"we"b\1"#).await;
        stand.agent("h2", "web-2").await;
        stand.agent("h3", "web-2").await;
        stand.policy("daily", "h1").await;
        stand.snapshot("s1", "h1", "daily", now_ms() - HOUR).await;
        stand.usage(10, 30).await;
        stand.check_run("c1", "structure", "done", 0, HOUR).await;

        let text = stand.scrape().await;
        assert!(text.contains(r#"host="we\"b\\1""#), "{text}");

        let mut seen = std::collections::HashSet::new();
        let mut typed = std::collections::HashSet::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                let (name, kind) = rest.split_once(' ').unwrap();
                assert_eq!(kind, "gauge");
                typed.insert(name.to_owned());
            } else if !line.starts_with('#') {
                let (series, number) = line.rsplit_once(' ').unwrap();
                number.parse::<f64>().unwrap_or_else(|_| panic!("{line}"));
                assert!(seen.insert(series.to_owned()), "duplicate series {series}");
                let name = series.split('{').next().unwrap();
                assert!(typed.contains(name), "no TYPE before {series}");
            }
        }
        assert!(seen.len() > 20, "{text}");
        stand.stop().await;
    }
}

fn free_addr() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

async fn serve_direct(
    metrics_listen: Option<std::net::SocketAddr>,
) -> (lodestore_server::ServerHandle, tempfile::TempDir) {
    use lodestore_server::config::{Config, GrpcConfig, Secret};
    use lodestore_server::{Listeners, serve};
    use tokio::net::TcpListener;

    let dir = tempfile::tempdir().unwrap();
    let loopback = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
    let rest = TcpListener::bind(loopback).await.unwrap();
    let grpc = TcpListener::bind(loopback).await.unwrap();
    let rest_addr = rest.local_addr().unwrap();
    let grpc_addr = grpc.local_addr().unwrap();
    let data_dir = dir.path().join("data");
    let config = Config {
        listen: rest_addr,
        public_url: format!("http://{rest_addr}"),
        grpc: GrpcConfig {
            listen: grpc_addr,
            public_address: grpc_addr.to_string(),
            ..GrpcConfig::default()
        },
        grpc_public_address_defaulted: false,
        database_url: lodestore_server::config::Secret::new(format!(
            "sqlite://{}/lodestore.db?mode=rwc",
            data_dir.display()
        )),
        data_dir,
        unlock_key: Some(Secret::new(data_encoding::HEXLOWER.encode(&[7u8; 32]))),
        admin_password: Some(Secret::new(common::ADMIN_PASSWORD)),
        agent_dir: dir.path().join("agents"),
        schedule_tick_secs: 0,
        metrics_listen,
        ..Config::default()
    };
    let handle = serve(config, Listeners { rest, grpc }).await.unwrap();
    (handle, dir)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_listen_without_auth() {
    for backend in backends() {
        let addr = free_addr();
        let ts = TestServer::start_with(&backend, |c| {
            c.schedule_tick_secs = 0;
            c.metrics_listen = Some(addr);
        })
        .await;

        let open = reqwest::get(format!("http://{addr}/metrics"))
            .await
            .unwrap();
        assert_eq!(open.status(), 200, "[{}]", backend.name());
        assert_eq!(
            open.headers()["content-type"],
            "text/plain; version=0.0.4; charset=utf-8"
        );
        assert!(open.text().await.unwrap().contains("lodestore_build_info"));

        let main = reqwest::get(format!("{}/metrics", ts.rest_url()))
            .await
            .unwrap();
        assert_eq!(main.status(), 401);
        let other = reqwest::get(format!("http://{addr}/api/v1/fleet"))
            .await
            .unwrap();
        assert_eq!(other.status(), 404);

        ts.stop().await;
        assert!(
            reqwest::get(format!("http://{addr}/metrics"))
                .await
                .is_err()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_listen_stays_closed_in_demo() {
    let addr = free_addr();
    let ts = TestServer::start_with(&backends().remove(0), |c| {
        c.schedule_tick_secs = 0;
        c.demo_mode = true;
        c.metrics_listen = Some(addr);
    })
    .await;
    let refused = reqwest::get(format!("http://{addr}/metrics")).await;
    assert!(
        refused.is_err_and(|e| e.is_connect()),
        "the metrics listener is open"
    );
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_listen_off_by_default() {
    let (handle, _dir) = serve_direct(None).await;
    assert_eq!(handle.metrics_addr, None);
    handle.shutdown().await;

    let (handle, _dir) = serve_direct(Some("127.0.0.1:0".parse().unwrap())).await;
    let addr = handle.metrics_addr.expect("a metrics address");
    assert_ne!(addr.port(), 0);
    let response = reqwest::get(format!("http://{addr}/metrics"))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    handle.shutdown().await;
}
