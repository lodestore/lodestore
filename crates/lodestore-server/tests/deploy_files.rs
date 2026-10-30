mod common;

use std::collections::BTreeSet;
use std::path::Path;

use common::{Backend, TestServer, bearer_client};
use lodestore_server::alerts::model::AlertType;
use lodestore_server::alerts::text::alertname;
use lodestore_server::db::now_ms;
use serde_json::Value;
use sqlx::AnyPool;

const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;
const STATS: &str = r#"{"bytes_total": 5000, "bytes_added": 700}"#;

fn deploy_file(path: &str) -> String {
    let full = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../deploy")
        .join(path);
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("reading {}: {e}", full.display()))
}

fn metric_names(text: &str) -> BTreeSet<String> {
    let is_name = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_';
    let mut found = BTreeSet::new();
    for (at, _) in text.match_indices("lodestore_") {
        if text[..at].chars().next_back().is_some_and(is_name) {
            continue;
        }
        let name: String = text[at..].chars().take_while(|c| is_name(*c)).collect();
        found.insert(name);
    }
    found
}

fn exposed_names(exposition: &str) -> BTreeSet<String> {
    exposition
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .filter_map(|line| line.split(['{', ' ']).next())
        .map(str::to_owned)
        .collect()
}

async fn seed(pool: &AnyPool, repository: &str) {
    let now = now_ms();
    sqlx::query(
        "INSERT INTO agents (id, hostname, repository_id, labels_json, os, arch, version, \
         status, enrolled_at, cert_serial, cert_issued_at, cert_not_after, last_seen_at) \
         VALUES ('h1', 'web-1', $1, '[]', 'linux', 'x86_64', '0.1.0', 'active', $2, 'serial', $2, $3, $4)",
    )
    .bind(repository)
    .bind(now - 30 * DAY)
    .bind(now + 300 * DAY)
    .bind(now)
    .execute(pool)
    .await
    .unwrap();

    let yaml = "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: daily\n\
                spec:\n  repository: main\n  selector:\n    hosts: [h1]\n  paths: [/etc]\n  \
                schedule:\n    cron: \"0 2 * * *\"\n    jitter: 0m\n";
    let policy = lodestore_policy::parse_yaml(yaml).unwrap();
    sqlx::query(
        "INSERT INTO policies (id, name, repository_id, yaml, spec_json, revision, \
         created_at, updated_at) VALUES ('daily', 'daily', $1, $2, $3, 1, $4, $4)",
    )
    .bind(repository)
    .bind(yaml)
    .bind(serde_json::to_string(&policy).unwrap())
    .bind(now - 30 * DAY)
    .execute(pool)
    .await
    .unwrap();

    let done = now - 3 * HOUR;
    sqlx::query(
        "INSERT INTO snapshots (id, repository_id, host_id, hostname, time_ms, paths_json, \
         tags_json, policy_name, incomplete, errors_total, warnings_total, stats_json, \
         discovered_at, received_at, received_exact) \
         VALUES ('s1', $1, 'h1', 'web-1', $2, '[]', '[]', 'daily', 0, 0, 0, $3, $2, $2, 1)",
    )
    .bind(repository)
    .bind(done)
    .bind(STATS)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO jobs (id, host_id, policy_name, source, state, spec, queued_at, \
         started_at, finished_at) VALUES ('j1', 'h1', 'daily', 'schedule', 'succeeded', $1, $2, $2, $3)",
    )
    .bind(Vec::<u8>::new())
    .bind(done - 300_000)
    .bind(done)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO repo_usage (repository_id, physical_bytes, logical_bytes, packs, objects, \
         counted_at) VALUES ($1, 1000, 5000, 4, 9, $2)",
    )
    .bind(repository)
    .bind(now - HOUR)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO check_runs (id, repository_id, level, origin, started_by, state, \
         findings_total, created_at, started_at, finished_at) \
         VALUES ('c1', $1, 'structure', 'manual', 'test', 'done', 0, $2, $2, $3)",
    )
    .bind(repository)
    .bind(now - 2 * HOUR)
    .bind(now - 2 * HOUR + 90_000)
    .execute(pool)
    .await
    .unwrap();
}

async fn live_names() -> BTreeSet<String> {
    let ts = TestServer::start_with(&Backend::Sqlite, |c| c.schedule_tick_secs = 0).await;
    let repository = ts.create_local_repository("main").await;
    seed(&ts.state().db.pool, &repository).await;
    let token = ts.issue_token(&["metrics:read"]).await;
    let response = bearer_client(&token)
        .get(format!("{}/metrics", ts.rest_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let names = exposed_names(&response.text().await.unwrap());
    ts.stop().await;
    names
}

fn missing(used: &BTreeSet<String>, live: &BTreeSet<String>) -> Vec<String> {
    used.difference(live).cloned().collect()
}

fn rules_document() -> Value {
    yaml_serde::from_str(&deploy_file("prometheus/lodestore-rules.yml")).expect("valid YAML")
}

fn rules_of(document: &Value) -> Vec<&Value> {
    document["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .flat_map(|group| group["rules"].as_array().expect("rules"))
        .collect()
}

#[test]
fn deploy_files_rules_structure() {
    let document = rules_document();
    let groups: Vec<&str> = document["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        groups,
        [
            "lodestore-backups",
            "lodestore-agents",
            "lodestore-repositories",
            "lodestore-certificates"
        ]
    );

    let rules = rules_of(&document);
    assert_eq!(rules.len(), 6);
    for (kind, severity) in [
        (AlertType::BackupFailed, "critical"),
        (AlertType::NoRecentBackup, "warning"),
        (AlertType::AgentOffline, "warning"),
        (AlertType::CheckFailed, "critical"),
        (AlertType::CertificateExpiring, "warning"),
        (AlertType::AgentUpdateFailed, "critical"),
    ] {
        let name = alertname(kind);
        let rule = rules
            .iter()
            .find(|r| r["alert"] == name)
            .unwrap_or_else(|| panic!("no rule {name}"));
        assert_eq!(rule["labels"]["severity"], severity, "{name}");
        assert!(!rule["expr"].as_str().unwrap().trim().is_empty(), "{name}");
        assert!(
            !rule["annotations"]["summary"]
                .as_str()
                .unwrap_or_default()
                .is_empty(),
            "{name} has no summary"
        );
        let expected_for = (kind == AlertType::AgentOffline).then_some("15m");
        assert_eq!(rule["for"].as_str(), expected_for, "{name}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deploy_files_metric_names_exist() {
    let document = rules_document();
    let used: BTreeSet<String> = rules_of(&document)
        .iter()
        .flat_map(|r| metric_names(r["expr"].as_str().unwrap()))
        .collect();
    assert!(used.len() >= 5, "{used:?}");
    let live = live_names().await;
    assert_eq!(missing(&used, &live), Vec::<String>::new());
}

fn dashboard_document() -> Value {
    serde_json::from_str(&deploy_file("grafana/lodestore.json")).expect("valid JSON")
}

fn dashboard_panels(dashboard: &Value) -> &Vec<Value> {
    dashboard["panels"].as_array().expect("panels")
}

fn dashboard_exprs(dashboard: &Value) -> Vec<&str> {
    dashboard_panels(dashboard)
        .iter()
        .flat_map(|panel| panel["targets"].as_array().into_iter().flatten())
        .map(|target| target["expr"].as_str().expect("expr"))
        .collect()
}

#[test]
fn deploy_files_dashboard_structure() {
    let dashboard = dashboard_document();
    assert_eq!(dashboard["title"], "Lodestore");
    assert!(!dashboard["uid"].as_str().unwrap_or_default().is_empty());
    assert!(dashboard["schemaVersion"].as_u64().unwrap() >= 38);
    assert_eq!(dashboard["time"]["from"], "now-7d");
    assert_eq!(dashboard["refresh"], "1m");

    let variables = dashboard["templating"]["list"].as_array().unwrap();
    let variable = |name: &str| {
        variables
            .iter()
            .find(|v| v["name"] == name)
            .unwrap_or_else(|| panic!("no variable {name}"))
    };
    assert_eq!(variable("datasource")["type"], "datasource");
    assert_eq!(variable("datasource")["query"], "prometheus");
    for name in ["host", "repository"] {
        assert_eq!(variable(name)["multi"], true, "{name}");
        assert_eq!(variable(name)["includeAll"], true, "{name}");
    }

    let panels = dashboard_panels(&dashboard);
    let rows: Vec<&str> = panels
        .iter()
        .filter(|p| p["type"] == "row")
        .map(|p| p["title"].as_str().unwrap())
        .collect();
    assert_eq!(rows, ["Fleet", "Backups", "Repositories", "Jobs"]);

    let mut ids = BTreeSet::new();
    for panel in panels.iter().filter(|p| p["type"] != "row") {
        let title = panel["title"].as_str().unwrap();
        assert!(ids.insert(panel["id"].as_u64().unwrap()), "{title}: id");
        assert!(
            !panel["description"].as_str().unwrap_or_default().is_empty(),
            "{title} has no description"
        );
        let unit = panel["fieldConfig"]["defaults"]["unit"].as_str();
        let override_unit = panel["fieldConfig"]["overrides"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|o| o["properties"].as_array().unwrap())
            .any(|p| p["id"] == "unit");
        assert!(
            unit.is_some_and(|u| !u.is_empty()) || override_unit,
            "{title} has no unit"
        );
        assert_ne!(panel["options"]["colorMode"], "background", "{title}");
    }
    for title in [
        "Healthy",
        "Overdue",
        "Failed",
        "Offline",
        "Storage used",
        "Deduplication",
        "Last successful backup",
        "Backup duration",
        "Bytes added",
        "Check results",
        "Certificate expiry (days)",
        "Jobs running",
    ] {
        assert!(
            panels.iter().any(|p| p["title"] == title),
            "no panel {title}"
        );
    }
}

#[test]
fn deploy_files_dashboard_no_dropped_surfaces() {
    let dashboard = dashboard_document();
    for expr in dashboard_exprs(&dashboard) {
        assert!(!expr.to_lowercase().contains("https"), "{expr}");
        assert!(!expr.contains("rate("), "{expr}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deploy_files_dashboard_metrics_exist() {
    let dashboard = dashboard_document();
    let used: BTreeSet<String> = dashboard_exprs(&dashboard)
        .into_iter()
        .flat_map(metric_names)
        .collect();
    assert!(used.len() >= 10, "{used:?}");
    let live = live_names().await;
    assert_eq!(missing(&used, &live), Vec::<String>::new());
}
