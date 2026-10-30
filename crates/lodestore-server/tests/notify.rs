mod common;

use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use common::{TestServer, backends};
use lodestore_policy::jiff::Timestamp;
use lodestore_server::alerts::{self, model::AlertType};
use lodestore_server::config::{ChannelSpec, ConfigChannel, Secret, SmtpSecurity};
use lodestore_server::db::now_ms;
use lodestore_server::notify;
use serde_json::{Value, json};
use sqlx::{AnyPool, Row};

const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;

const ALL_TYPES: [AlertType; 6] = [
    AlertType::BackupFailed,
    AlertType::NoRecentBackup,
    AlertType::AgentOffline,
    AlertType::CheckFailed,
    AlertType::CertificateExpiring,
    AlertType::AgentUpdateFailed,
];

#[derive(Clone)]
struct Hit {
    path: String,
    authorization: Option<String>,
    body: Vec<u8>,
}

impl Hit {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

#[derive(Clone)]
struct Stub {
    base: String,
    hits: Arc<Mutex<Vec<Hit>>>,
    status: Arc<AtomicU16>,
    reply: Arc<Mutex<String>>,
    delay_ms: Arc<AtomicU64>,
}

impl Stub {
    async fn start() -> Stub {
        let stub = Stub {
            base: String::new(),
            hits: Arc::default(),
            status: Arc::new(AtomicU16::new(200)),
            reply: Arc::new(Mutex::new("ok".to_string())),
            delay_ms: Arc::default(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().fallback(handle).with_state(stub.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Stub { base, ..stub }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn hits(&self) -> Vec<Hit> {
        self.hits.lock().unwrap().clone()
    }

    fn respond(&self, status: u16, body: &str) {
        self.status.store(status, Ordering::SeqCst);
        *self.reply.lock().unwrap() = body.to_string();
    }
}

async fn handle(
    State(stub): State<Stub>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, String) {
    stub.hits.lock().unwrap().push(Hit {
        path: uri.path().to_string(),
        authorization: headers
            .get("authorization")
            .map(|v| v.to_str().unwrap().to_string()),
        body: body.to_vec(),
    });
    let delay = stub.delay_ms.load(Ordering::SeqCst);
    if delay > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
    (
        StatusCode::from_u16(stub.status.load(Ordering::SeqCst)).unwrap(),
        stub.reply.lock().unwrap().clone(),
    )
}

fn webhook(name: &str, url: &str, secret: Option<&str>) -> ConfigChannel {
    ConfigChannel {
        name: name.to_string(),
        enabled: true,
        alerts: ALL_TYPES.to_vec(),
        kind: ChannelSpec::Webhook {
            url: url.to_string(),
            secret: secret.map(Secret::new),
        },
    }
}

fn slack(name: &str, url: &str) -> ConfigChannel {
    ConfigChannel {
        name: name.to_string(),
        enabled: true,
        alerts: ALL_TYPES.to_vec(),
        kind: ChannelSpec::Slack {
            webhook_url: Secret::new(url),
        },
    }
}

fn only(mut channel: ConfigChannel, types: &[AlertType]) -> ConfigChannel {
    channel.alerts = types.to_vec();
    channel
}

fn rfc3339(ms: i64) -> String {
    Timestamp::from_second(ms / 1000).unwrap().to_string()
}

struct Env {
    ts: TestServer,
    repo: String,
}

impl Env {
    async fn start(backend: &common::Backend, channels: Vec<ConfigChannel>) -> Env {
        let ts = TestServer::start_with(backend, |c| {
            c.schedule_tick_secs = 0;
            c.notifications.channels = channels;
        })
        .await;
        let repo = ts.create_local_repository("main").await;
        Env { ts, repo }
    }

    fn pool(&self) -> &AnyPool {
        &self.ts.state().db.pool
    }

    async fn tick(&self, now_ms: i64) {
        alerts::tick(self.ts.state(), now_ms).await.unwrap();
        notify::tick(self.ts.state(), now_ms).await.unwrap();
    }

    async fn host(&self, id: &str, hostname: &str) {
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
        .bind(now_ms() + 200 * HOUR)
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn rolled_back_update(&self, host: &str, reason: &str, finished_ms: i64) {
        sqlx::query(
            "INSERT INTO agent_rollouts (id, target_version, state, canary, wave_size, \
             pause_after, created_by, created_at) VALUES ('r1', '0.2.0', 'done', 1, 10, 0, 'admin', $1)",
        )
        .bind(finished_ms)
        .execute(self.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO agent_updates (id, rollout_id, host_id, arch, from_version, \
             target_version, wave, state, reason, queued_at, finished_at) \
             VALUES ('u1', 'r1', $1, 'x86_64', '0.1.0', '0.2.0', 0, 'rolled_back', $2, $3, $3)",
        )
        .bind(host)
        .bind(reason)
        .bind(finished_ms)
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn policy(&self, name: &str, host: &str) {
        let yaml = format!(
            "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: {name}\n\
             spec:\n  repository: main\n  selector:\n    hosts: [{host}]\n  paths: [/etc]\n"
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
             discovered_at, received_at, received_exact) \
             VALUES ($1, $2, $3, 'h', $4, '[]', '[]', $5, 0, 0, 0, $6, $4, $4, 1)",
        )
        .bind(id)
        .bind(&self.repo)
        .bind(host)
        .bind(received_ms)
        .bind(policy)
        .bind(r#"{"bytes_total": 5000, "bytes_added": 700}"#)
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

    async fn failing_backup(&self, error: &str, finished_ms: i64) {
        self.host("h1", "web-1").await;
        self.policy("daily", "h1").await;
        self.failed_job("j1", "h1", "daily", finished_ms, error)
            .await;
    }

    async fn deliveries(&self) -> Vec<(String, i64, Option<String>)> {
        sqlx::query(
            "SELECT state, attempts, last_error FROM alert_deliveries ORDER BY created_at, id",
        )
        .fetch_all(self.pool())
        .await
        .unwrap()
        .iter()
        .map(|row| {
            (
                row.try_get("state").unwrap(),
                row.try_get("attempts").unwrap(),
                row.try_get("last_error").unwrap(),
            )
        })
        .collect()
    }

    async fn channel_names(&self) -> Vec<String> {
        sqlx::query("SELECT name FROM notification_channels ORDER BY name")
            .fetch_all(self.pool())
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get("name").unwrap())
            .collect()
    }

    async fn stop(self) {
        self.ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notify_webhook_payload_golden() {
    for backend in backends() {
        let stub = Stub::start().await;
        let env = Env::start(&backend, vec![webhook("ops", &stub.url("/hook"), None)]).await;
        env.failing_backup("disk full", now_ms() - HOUR).await;
        let now = now_ms();

        env.tick(now).await;
        let hits = stub.hits();
        assert_eq!(hits.len(), 1, "[{}]", backend.name());
        assert_eq!(hits[0].path, "/hook");
        assert_eq!(hits[0].authorization, None);
        let labels = json!({
            "alertname": "LodestoreBackupFailed",
            "host": "web-1",
            "host_id": "h1",
            "policy": "daily",
            "severity": "critical",
        });
        let annotations = json!({
            "summary": "Backup failed on web-1",
            "description": "Policy daily: disk full.",
        });
        let external = env.ts.config.public_url.clone();
        let golden = json!({
            "version": "4",
            "groupKey": "lodestore:5c0701e2dd789d4b",
            "truncatedAlerts": 0,
            "status": "firing",
            "receiver": "ops",
            "groupLabels": { "alertname": "LodestoreBackupFailed" },
            "commonLabels": labels,
            "commonAnnotations": annotations,
            "externalURL": external,
            "alerts": [{
                "status": "firing",
                "labels": labels,
                "annotations": annotations,
                "startsAt": rfc3339(now),
                "endsAt": "0001-01-01T00:00:00Z",
                "generatorURL": format!("{external}/hosts/h1"),
                "fingerprint": "5c0701e2dd789d4b",
            }],
        });
        assert_eq!(hits[0].json(), golden);

        env.snapshot("s1", "h1", "daily", now).await;
        let later = now + 2 * MINUTE;
        env.tick(later).await;
        let hits = stub.hits();
        assert_eq!(hits.len(), 2);
        let resolved = hits[1].json();
        assert_eq!(resolved["status"], "resolved");
        assert_eq!(resolved["alerts"][0]["status"], "resolved");
        assert_eq!(resolved["alerts"][0]["endsAt"], rfc3339(later));
        assert_eq!(resolved["alerts"][0]["startsAt"], rfc3339(now));
        assert_eq!(resolved["alerts"][0]["fingerprint"], "5c0701e2dd789d4b");
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notify_config_channels_synced() {
    for backend in backends() {
        let mut env = Env::start(
            &backend,
            vec![
                webhook("alpha", "http://127.0.0.1:9/a", None),
                webhook("beta", "http://127.0.0.1:9/b", Some("s3cret")),
            ],
        )
        .await;
        assert_eq!(env.channel_names().await, ["alpha", "beta"]);
        let row = sqlx::query("SELECT source, kind, enabled, secret_id FROM notification_channels WHERE name = 'beta'")
            .fetch_one(env.pool())
            .await
            .unwrap();
        assert_eq!(row.try_get::<String, _>("source").unwrap(), "config");
        assert_eq!(row.try_get::<String, _>("kind").unwrap(), "webhook");
        assert_eq!(row.try_get::<i64, _>("enabled").unwrap(), 1);
        assert_eq!(row.try_get::<Option<String>, _>("secret_id").unwrap(), None);
        let config_json: String =
            sqlx::query("SELECT config_json FROM notification_channels WHERE name = 'beta'")
                .fetch_one(env.pool())
                .await
                .unwrap()
                .try_get("config_json")
                .unwrap();
        assert!(!config_json.contains("s3cret"), "{config_json}");

        env.ts.config.notifications.channels.remove(0);
        env.ts.restart().await;
        env.ts.config.notifications.channels.clear();
        assert_eq!(env.channel_names().await, ["beta"]);
        env.ts.restart().await;
        assert!(env.channel_names().await.is_empty());
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notify_config_channel_clashes_with_ui_channel() {
    for backend in backends() {
        let env = Env::start(&backend, vec![]).await;
        sqlx::query(
            "INSERT INTO notification_channels (id, name, kind, source, enabled, types_json, \
             config_json, created_at, updated_at) VALUES ('c1', 'ops', 'webhook', 'ui', 1, '[]', '{}', 0, 0)",
        )
        .execute(env.pool())
        .await
        .unwrap();
        let mut state = env.ts.state().clone();
        let mut config = (*state.config).clone();
        config.notifications.channels = vec![webhook("ops", "http://127.0.0.1:9/", None)];
        state.config = Arc::new(config);
        let error = notify::sync_config_channels(&state).await.unwrap_err();
        assert!(
            error.to_string().contains(
                "notification channel ops from the server config clashes with a channel created in the UI"
            ),
            "{error:#}"
        );
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notify_link_local_refused() {
    for backend in backends() {
        let env = Env::start(
            &backend,
            vec![webhook("meta", "http://169.254.169.254/latest", None)],
        )
        .await;
        env.failing_backup("disk full", now_ms() - HOUR).await;
        env.tick(now_ms()).await;

        let deliveries = env.deliveries().await;
        assert_eq!(deliveries.len(), 1, "[{}]", backend.name());
        let (state, attempts, error) = &deliveries[0];
        assert_eq!((state.as_str(), *attempts), ("pending", 1));
        assert_eq!(
            error.as_deref(),
            Some("The address 169.254.169.254 is not allowed.")
        );
        let channel = sqlx::query("SELECT last_ok, last_error FROM notification_channels")
            .fetch_one(env.pool())
            .await
            .unwrap();
        assert_eq!(
            channel.try_get::<Option<i64>, _>("last_ok").unwrap(),
            Some(0)
        );
        assert_eq!(
            channel.try_get::<Option<String>, _>("last_error").unwrap(),
            error.clone()
        );
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notify_telegram_request_and_errors() {
    let (url, body) = notify::telegram::request("123:abc", "-100", "hello");
    assert_eq!(url, "https://api.telegram.org/bot123:abc/sendMessage");
    assert_eq!(body, json!({ "chat_id": "-100", "text": "hello" }));

    let error = notify::telegram::parse_response(
        400,
        r#"{"ok":false,"error_code":400,"description":"Bad Request: chat not found"}"#,
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "Telegram: Bad Request: chat not found");
    notify::telegram::parse_response(200, r#"{"ok":true,"result":{}}"#).unwrap();
    let error = notify::telegram::parse_response(502, "<html>bad gateway</html>").unwrap_err();
    assert_eq!(error.to_string(), "HTTP 502: <html>bad gateway</html>");

    let stub = Stub::start().await;
    stub.respond(200, r#"{"ok":true}"#);
    notify::telegram::send_to(true, &stub.base, "123:abc", "-100", "FIRING: x")
        .await
        .unwrap();
    let hits = stub.hits();
    assert_eq!(hits[0].path, "/bot123:abc/sendMessage");
    assert_eq!(
        hits[0].json(),
        json!({ "chat_id": "-100", "text": "FIRING: x" })
    );
    stub.respond(
        400,
        r#"{"ok":false,"description":"Bad Request: chat not found"}"#,
    );
    let error = notify::telegram::send_to(true, &stub.base, "123:abc", "-100", "x")
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "Telegram: Bad Request: chat not found");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notify_secret_not_in_errors() {
    let error =
        notify::telegram::send_to(true, "http://127.0.0.1:1", "123:TOKENVALUE", "-100", "x")
            .await
            .unwrap_err()
            .to_string();
    assert!(!error.contains("TOKENVALUE"), "{error}");
    assert!(!error.contains("127.0.0.1:1/bot"), "{error}");

    for backend in backends() {
        let channels = vec![
            slack("chat", "http://127.0.0.1:1/services/SLACKPATH"),
            webhook("hook", "http://127.0.0.1:1/hook", Some("BEARERVALUE")),
        ];
        let env = Env::start(&backend, channels).await;
        env.failing_backup("disk full", now_ms() - HOUR).await;
        env.tick(now_ms()).await;
        let deliveries = env.deliveries().await;
        assert_eq!(deliveries.len(), 2, "[{}]", backend.name());
        let stored: Vec<String> = sqlx::query("SELECT last_error FROM notification_channels")
            .fetch_all(env.pool())
            .await
            .unwrap()
            .iter()
            .filter_map(|row| row.try_get("last_error").unwrap())
            .collect();
        assert_eq!(stored.len(), 2);
        for text in deliveries
            .iter()
            .filter_map(|(_, _, error)| error.clone())
            .chain(stored)
        {
            assert!(!text.contains("SLACKPATH"), "{text}");
            assert!(!text.contains("BEARERVALUE"), "{text}");
            assert!(!text.contains("127.0.0.1:1"), "{text}");
        }
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notify_slack_and_webhook_bearer() {
    for backend in backends() {
        let chat = Stub::start().await;
        let hook = Stub::start().await;
        let env = Env::start(
            &backend,
            vec![
                slack("chat", &chat.url("/services/T0/B0/XYZ")),
                webhook("hook", &hook.url("/in"), Some("s3cret")),
            ],
        )
        .await;
        env.failing_backup("disk full", now_ms() - HOUR).await;
        let now = now_ms();
        env.tick(now).await;

        let external = env.ts.config.public_url.clone();
        let hits = chat.hits();
        assert_eq!(hits.len(), 1, "[{}]", backend.name());
        assert_eq!(hits[0].path, "/services/T0/B0/XYZ");
        assert_eq!(hits[0].authorization, None);
        assert_eq!(
            hits[0].json(),
            json!({
                "text": format!(
                    "FIRING: Backup failed on web-1\nPolicy daily: disk full.\nOpen in Lodestore: {external}/hosts/h1"
                )
            })
        );
        assert_eq!(
            hook.hits()[0].authorization.as_deref(),
            Some("Bearer s3cret")
        );

        chat.respond(400, "invalid_payload");
        env.host("h2", "web-2").await;
        env.policy("weekly", "h2").await;
        env.failed_job("j2", "h2", "weekly", now - HOUR, "no space")
            .await;
        env.tick(now + MINUTE).await;
        let deliveries = env.deliveries().await;
        assert!(
            deliveries
                .iter()
                .any(|(_, _, error)| error.as_deref() == Some("HTTP 400")),
            "{deliveries:?}"
        );
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notify_agent_update_failed_filter() {
    for backend in backends() {
        let wanted = Stub::start().await;
        let other = Stub::start().await;
        let env = Env::start(
            &backend,
            vec![
                only(
                    webhook("wanted", &wanted.url("/h"), None),
                    &[AlertType::AgentUpdateFailed],
                ),
                only(
                    webhook("other", &other.url("/h"), None),
                    &[AlertType::BackupFailed],
                ),
            ],
        )
        .await;
        env.host("h1", "web-1").await;
        env.rolled_back_update("h1", "Health check failed.", now_ms() - HOUR)
            .await;
        env.tick(now_ms()).await;

        let hits = wanted.hits();
        assert_eq!(hits.len(), 1, "[{}]", backend.name());
        let body = hits[0].json();
        assert_eq!(
            body["groupLabels"]["alertname"],
            "LodestoreAgentUpdateFailed"
        );
        assert_eq!(body["commonLabels"]["severity"], "critical");
        assert_eq!(
            body["commonAnnotations"]["summary"],
            "Agent update failed on web-1"
        );
        assert_eq!(
            body["commonAnnotations"]["description"],
            "Health check failed."
        );
        assert!(other.hits().is_empty());
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notify_type_filter() {
    for backend in backends() {
        let wanted = Stub::start().await;
        let other = Stub::start().await;
        let off = Stub::start().await;
        let mut disabled = webhook("off", &off.url("/h"), None);
        disabled.enabled = false;
        let env = Env::start(
            &backend,
            vec![
                only(
                    webhook("wanted", &wanted.url("/h"), None),
                    &[AlertType::BackupFailed],
                ),
                only(
                    webhook("other", &other.url("/h"), None),
                    &[AlertType::AgentOffline, AlertType::CertificateExpiring],
                ),
                disabled,
            ],
        )
        .await;
        env.failing_backup("disk full", now_ms() - HOUR).await;
        env.tick(now_ms()).await;

        assert_eq!(wanted.hits().len(), 1, "[{}]", backend.name());
        assert!(other.hits().is_empty());
        assert!(off.hits().is_empty());
        assert_eq!(env.deliveries().await.len(), 1);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notify_unreadable_channel_does_not_stop_the_others() {
    for backend in backends() {
        let stub = Stub::start().await;
        let env = Env::start(&backend, vec![webhook("good", &stub.url("/h"), None)]).await;
        sqlx::query(
            "INSERT INTO notification_channels (id, name, kind, source, enabled, types_json, \
             config_json, created_at, updated_at) VALUES ('c1', 'bad', 'webhook', 'ui', 1, \
             '[\"from_the_future\"]', '{}', 0, 0)",
        )
        .execute(env.pool())
        .await
        .unwrap();
        env.failing_backup("disk full", now_ms() - HOUR).await;
        env.tick(now_ms()).await;
        assert_eq!(stub.hits().len(), 1, "[{}]", backend.name());
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notify_slow_channels_are_delivered_side_by_side() {
    for backend in backends() {
        let stubs = [
            Stub::start().await,
            Stub::start().await,
            Stub::start().await,
        ];
        for stub in &stubs {
            stub.delay_ms.store(700, Ordering::SeqCst);
        }
        let channels = stubs
            .iter()
            .enumerate()
            .map(|(i, stub)| webhook(&format!("hook-{i}"), &stub.url("/h"), None))
            .collect();
        let env = Env::start(&backend, channels).await;
        env.failing_backup("disk full", now_ms() - HOUR).await;
        let started = std::time::Instant::now();
        env.tick(now_ms()).await;
        assert!(
            started.elapsed() < std::time::Duration::from_millis(2000),
            "[{}] {:?}",
            backend.name(),
            started.elapsed()
        );
        assert!(stubs.iter().all(|stub| stub.hits().len() == 1));
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notify_old_history_is_pruned() {
    for backend in backends() {
        let stub = Stub::start().await;
        let env = Env::start(&backend, vec![webhook("ops", &stub.url("/h"), None)]).await;
        let channel: String = sqlx::query("SELECT id FROM notification_channels")
            .fetch_one(env.pool())
            .await
            .unwrap()
            .try_get("id")
            .unwrap();
        let now = now_ms();
        let old = now - 91 * DAY;
        for (id, state, resolved_at) in [
            ("old", "resolved", Some(old)),
            ("recent", "resolved", Some(now - DAY)),
            ("open", "firing", None),
        ] {
            sqlx::query(
                "INSERT INTO alerts (id, type, key, severity, state, title, detail, facts_json, \
                 started_at, updated_at, resolved_at) \
                 VALUES ($1, 'agent_offline', $1, 'warning', $2, 't', 'd', '{}', $3, $3, $4)",
            )
            .bind(id)
            .bind(state)
            .bind(old)
            .bind(resolved_at)
            .execute(env.pool())
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO alert_events (id, alert_id, kind, at, fanned_out_at) \
                 VALUES ($1, $2, 'firing', $3, $3)",
            )
            .bind(format!("e-{id}"))
            .bind(id)
            .bind(old)
            .execute(env.pool())
            .await
            .unwrap();
        }
        for (id, event, created) in [
            ("d-old", "e-open", old),
            ("d-new", "e-recent", now - DAY),
            ("d-gone", "e-old", old),
        ] {
            sqlx::query(
                "INSERT INTO alert_deliveries (id, event_id, channel_id, state, attempts, \
                 next_attempt_at, created_at) VALUES ($1, $2, $3, 'failed', 4, $4, $4)",
            )
            .bind(id)
            .bind(event)
            .bind(&channel)
            .bind(created)
            .execute(env.pool())
            .await
            .unwrap();
        }

        notify::tick(env.ts.state(), now).await.unwrap();
        let ids: Vec<String> = sqlx::query("SELECT id FROM alerts ORDER BY id")
            .fetch_all(env.pool())
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get("id").unwrap())
            .collect();
        assert_eq!(ids, ["open", "recent"], "[{}]", backend.name());
        let events: i64 = sqlx::query("SELECT COUNT(*) AS n FROM alert_events")
            .fetch_one(env.pool())
            .await
            .unwrap()
            .try_get("n")
            .unwrap();
        assert_eq!(events, 2);
        let deliveries: Vec<String> = sqlx::query("SELECT id FROM alert_deliveries")
            .fetch_all(env.pool())
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get("id").unwrap())
            .collect();
        assert_eq!(deliveries, ["d-new"]);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notify_retries_then_failed() {
    for backend in backends() {
        let stub = Stub::start().await;
        stub.respond(500, "boom");
        let env = Env::start(&backend, vec![webhook("ops", &stub.url("/h"), None)]).await;
        env.failing_backup("disk full", now_ms() - HOUR).await;
        let t = now_ms();

        env.tick(t).await;
        assert_eq!(stub.hits().len(), 1, "[{}]", backend.name());
        env.tick(t + 30_000).await;
        assert_eq!(stub.hits().len(), 1);
        env.tick(t + MINUTE).await;
        assert_eq!(stub.hits().len(), 2);
        env.tick(t + 5 * MINUTE).await;
        assert_eq!(stub.hits().len(), 2);
        env.tick(t + 6 * MINUTE).await;
        assert_eq!(stub.hits().len(), 3);
        env.tick(t + 35 * MINUTE).await;
        assert_eq!(stub.hits().len(), 3);
        env.tick(t + 36 * MINUTE).await;
        assert_eq!(stub.hits().len(), 4);
        assert_eq!(
            env.deliveries().await,
            [("failed".to_string(), 4, Some("HTTP 500".to_string()))]
        );

        env.tick(t + 2 * HOUR).await;
        assert_eq!(stub.hits().len(), 4);

        stub.respond(200, "ok");
        env.tick(t + 24 * HOUR + MINUTE).await;
        assert_eq!(stub.hits().len(), 5);
        let deliveries = env.deliveries().await;
        assert_eq!(deliveries.len(), 2);
        assert_eq!(deliveries[1], ("sent".to_string(), 1, None));
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notify_silenced_host_sends_nothing() {
    for backend in backends() {
        let stub = Stub::start().await;
        let env = Env::start(&backend, vec![webhook("ops", &stub.url("/h"), None)]).await;
        env.failing_backup("disk full", now_ms() - HOUR).await;
        let client = common::bearer_client(&env.ts.issue_token(&["alerts:write"]).await);
        let silence = format!("{}/api/v1/hosts/h1/silence", env.ts.rest_url());

        let response = client
            .put(&silence)
            .json(&json!({ "duration": "day" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200, "[{}]", backend.name());
        env.tick(now_ms()).await;
        assert!(stub.hits().is_empty());
        assert!(env.deliveries().await.is_empty());

        let response = client.delete(&silence).send().await.unwrap();
        assert_eq!(response.status().as_u16(), 204);
        env.tick(now_ms() + MINUTE).await;
        env.tick(now_ms() + 2 * MINUTE).await;
        let hits = stub.hits();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].json()["status"], "firing");
        env.stop().await;
    }
}

fn email(name: &str, port: u16, to: &[&str]) -> ConfigChannel {
    ConfigChannel {
        name: name.to_string(),
        enabled: true,
        alerts: ALL_TYPES.to_vec(),
        kind: ChannelSpec::Email {
            smtp_host: "127.0.0.1".to_string(),
            port,
            security: SmtpSecurity::Starttls,
            username: Some("lodestore".to_string()),
            password: Some(Secret::new("SMTPPASSWORD")),
            from: "lodestore@example.test".to_string(),
            to: to.iter().map(ToString::to_string).collect(),
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notify_email_no_recipient() {
    for backend in backends() {
        let env = Env::start(&backend, vec![email("mail", 1, &[])]).await;
        env.failing_backup("disk full", now_ms() - HOUR).await;
        env.tick(now_ms()).await;
        let deliveries = env.deliveries().await;
        assert_eq!(deliveries.len(), 1, "[{}]", backend.name());
        assert_eq!(
            deliveries[0].2.as_deref(),
            Some("Enter a recipient or set an admin email in Settings, User.")
        );
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notify_email_connection_error_redacted() {
    for backend in backends() {
        let env = Env::start(&backend, vec![email("mail", 1, &["ops@example.test"])]).await;
        env.failing_backup("disk full", now_ms() - HOUR).await;
        env.tick(now_ms()).await;
        let deliveries = env.deliveries().await;
        assert_eq!(deliveries.len(), 1, "[{}]", backend.name());
        let (state, attempts, error) = &deliveries[0];
        assert_eq!((state.as_str(), *attempts), ("pending", 1));
        let error = error.as_deref().unwrap();
        assert!(error.starts_with("SMTP: "), "{error}");
        assert!(!error.contains("SMTPPASSWORD"), "{error}");
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notify_reminder_text() {
    for backend in backends() {
        let stub = Stub::start().await;
        let env = Env::start(&backend, vec![slack("chat", &stub.url("/s"))]).await;
        env.failing_backup("disk full", now_ms() - HOUR).await;
        let t = now_ms();
        env.tick(t).await;
        env.tick(t + DAY + MINUTE).await;

        let hits = stub.hits();
        assert_eq!(hits.len(), 2, "[{}]", backend.name());
        let first = hits[0].json()["text"].as_str().unwrap().to_string();
        let second = hits[1].json()["text"].as_str().unwrap().to_string();
        assert!(
            first.starts_with("FIRING: Backup failed on web-1\n"),
            "{first}"
        );
        assert!(
            second.starts_with("STILL FIRING: Backup failed on web-1\n"),
            "{second}"
        );
        env.stop().await;
    }
}
