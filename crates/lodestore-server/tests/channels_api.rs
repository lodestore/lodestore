mod common;

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use common::{Api, TestServer, backends, bearer_client};
use lodestore_server::alerts::model::AlertType;
use lodestore_server::config::{ChannelSpec, ConfigChannel, Secret};
use reqwest::Method;
use serde_json::{Value, json};

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
}

impl Stub {
    async fn start() -> Stub {
        let stub = Stub {
            base: String::new(),
            hits: Arc::default(),
            status: Arc::new(AtomicU16::new(200)),
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
    (
        StatusCode::from_u16(stub.status.load(Ordering::SeqCst)).unwrap(),
        "ok".to_string(),
    )
}

struct Rest {
    api: Api,
    base: String,
}

impl Rest {
    async fn new(ts: &TestServer) -> Rest {
        Rest {
            api: ts.api().await,
            base: ts.rest_url(),
        }
    }

    async fn send(&self, method: Method, path: &str, body: Option<&Value>) -> (u16, Value, String) {
        let mut request = reqwest::Client::new()
            .request(method, format!("{}{path}", self.base))
            .header("cookie", self.api.cookie())
            .header("x-csrf-token", &self.api.csrf);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::Null),
            text,
        )
    }

    async fn get(&self, path: &str) -> (u16, Value, String) {
        self.send(Method::GET, path, None).await
    }

    async fn post(&self, path: &str, body: &Value) -> (u16, Value, String) {
        self.send(Method::POST, path, Some(body)).await
    }
}

const CHANNELS: &str = "/api/v1/notifications/channels";

fn webhook(name: &str, url: &str, secret: Option<&str>) -> Value {
    json!({
        "type": "webhook",
        "name": name,
        "enabled": true,
        "settings": { "url": url },
        "secret": secret,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channels_api_webhook_create_and_test() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let rest = Rest::new(&ts).await;
        let stub = Stub::start().await;

        let (status, created, text) = rest
            .post(
                CHANNELS,
                &webhook("hook", &stub.url("/hook"), Some("s3cret-token")),
            )
            .await;
        assert_eq!(status, 201, "[{}] {text}", backend.name());
        assert_eq!(created["type"], "webhook");
        assert_eq!(created["source"], "ui");
        assert_eq!(created["secret_set"], true);
        assert_eq!(created["settings"]["url"], stub.url("/hook"));
        assert_eq!(created["alert_types"].as_array().unwrap().len(), 8);
        assert!(created["last_delivery"].is_null());
        let id = created["id"].as_str().unwrap().to_string();

        let (_, list, text) = rest.get(CHANNELS).await;
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["id"], id.as_str());
        assert!(!text.contains("s3cret-token"));

        let (status, tested, text) = rest
            .post(&format!("{CHANNELS}/test"), &json!({ "channel_id": id }))
            .await;
        assert_eq!(status, 200, "{text}");
        assert_eq!(tested["ok"], true, "{tested}");
        let hits = stub.hits();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "/hook");
        assert_eq!(
            hits[0].authorization.as_deref(),
            Some("Bearer s3cret-token")
        );
        assert_eq!(
            hits[0].json()["alerts"][0]["labels"]["alertname"],
            "LodestoreTest"
        );

        let (_, shown, _) = rest.get(&format!("{CHANNELS}/{id}")).await;
        assert_eq!(shown["last_delivery"]["ok"], true);
        assert!(shown["last_delivery"]["error"].is_null());

        let draft = webhook("hook", &stub.url("/draft"), None);
        let (_, tested, _) = rest
            .post(
                &format!("{CHANNELS}/test"),
                &json!({ "channel_id": id, "channel": draft }),
            )
            .await;
        assert_eq!(tested["ok"], true);
        let hits = stub.hits();
        assert_eq!(hits[1].path, "/draft");
        assert_eq!(
            hits[1].authorization.as_deref(),
            Some("Bearer s3cret-token")
        );

        stub.status.store(500, Ordering::SeqCst);
        let (_, tested, _) = rest
            .post(&format!("{CHANNELS}/test"), &json!({ "channel_id": id }))
            .await;
        assert_eq!(tested["ok"], false);
        assert!(
            tested["message"].as_str().unwrap().contains("500"),
            "{tested}"
        );
        let (_, shown, _) = rest.get(&format!("{CHANNELS}/{id}")).await;
        assert_eq!(shown["last_delivery"]["ok"], false);

        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channels_api_refuses_private_targets_by_default() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| {
            c.notifications.allow_private_targets = false;
        })
        .await;
        let rest = Rest::new(&ts).await;
        let stub = Stub::start().await;
        stub.status.store(500, Ordering::SeqCst);

        let inputs = [
            webhook("hook", &stub.url("/hook"), None),
            json!({ "type": "slack", "name": "sl", "enabled": true,
                "secret": stub.url("/slack").replace("http://", "https://") }),
            json!({ "type": "email", "name": "mail", "enabled": true,
                "settings": { "smtp_host": "127.0.0.1", "port": 1, "from": "a@example.test",
                    "recipients": ["b@example.test"] } }),
        ];
        for input in &inputs {
            let (status, tested, text) = rest
                .post(&format!("{CHANNELS}/test"), &json!({ "channel": input }))
                .await;
            assert_eq!(status, 200, "[{}] {text}", backend.name());
            assert_eq!(tested["ok"], false, "{text}");
            assert!(
                tested["message"].as_str().unwrap().contains("not allowed"),
                "{text}"
            );
        }
        assert!(stub.hits().is_empty());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channels_api_stored_secret_stays_with_its_address() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let rest = Rest::new(&ts).await;
        let stub = Stub::start().await;
        let elsewhere = Stub::start().await;

        let (_, created, _) = rest
            .post(
                CHANNELS,
                &webhook("hook", &stub.url("/hook"), Some("s3cret")),
            )
            .await;
        let id = created["id"].as_str().unwrap().to_string();

        let draft = webhook("hook", &elsewhere.url("/hook"), None);
        let (status, body, text) = rest
            .post(
                &format!("{CHANNELS}/test"),
                &json!({ "channel_id": id, "channel": draft }),
            )
            .await;
        assert_eq!(status, 422, "[{}] {text}", backend.name());
        assert_eq!(body["errors"][0]["path"], "secret", "{text}");
        assert!(elsewhere.hits().is_empty());

        let (status, body, text) = rest
            .send(
                Method::PUT,
                &format!("{CHANNELS}/{id}"),
                Some(&webhook("hook", &elsewhere.url("/hook"), None)),
            )
            .await;
        assert_eq!(status, 422, "{text}");
        assert_eq!(body["errors"][0]["path"], "secret", "{text}");

        let (status, _, text) = rest
            .send(
                Method::PUT,
                &format!("{CHANNELS}/{id}"),
                Some(&webhook("hook", &elsewhere.url("/hook"), Some("fresh"))),
            )
            .await;
        assert_eq!(status, 200, "{text}");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channels_api_secret_never_returned() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let rest = Rest::new(&ts).await;
        let stub = Stub::start().await;
        let secret = "TOP-SECRET-VALUE";

        let inputs = [
            webhook("hook", &stub.url("/h"), Some(secret)),
            json!({ "type": "telegram", "name": "tg", "enabled": true,
                "settings": { "chat_id": "42" }, "secret": secret }),
            json!({ "type": "slack", "name": "sl", "enabled": true,
                "secret": format!("https://hooks.slack.test/{secret}") }),
            json!({ "type": "email", "name": "mail", "enabled": true,
                "settings": { "smtp_host": "127.0.0.1", "port": 1, "from": "a@example.test",
                    "recipients": ["b@example.test"] }, "secret": secret }),
        ];
        let mut ids = Vec::new();
        for input in &inputs {
            let (status, body, text) = rest.post(CHANNELS, input).await;
            assert_eq!(status, 201, "[{}] {text}", backend.name());
            assert!(!text.contains(secret), "{text}");
            assert_eq!(body["secret_set"], true);
            ids.push(body["id"].as_str().unwrap().to_string());
        }
        let (_, _, text) = rest.get(CHANNELS).await;
        assert!(!text.contains(secret), "{text}");
        for id in &ids {
            let (_, _, text) = rest.get(&format!("{CHANNELS}/{id}")).await;
            assert!(!text.contains(secret), "{text}");
        }
        for id in [&ids[0], &ids[3]] {
            let (_, _, text) = rest
                .post(&format!("{CHANNELS}/test"), &json!({ "channel_id": id }))
                .await;
            assert!(!text.contains(secret), "{text}");
        }

        let mut bad = webhook("", "ftp://nope", Some(secret));
        bad["alert_types"] = json!([]);
        let (status, _, text) = rest.post(CHANNELS, &bad).await;
        assert_eq!(status, 422);
        assert!(!text.contains(secret), "{text}");

        let sealed: i64 = {
            use sqlx::Row;
            sqlx::query("SELECT COUNT(*) AS n FROM secrets WHERE kind = 'notification-channel'")
                .fetch_one(&ts.state().db.pool)
                .await
                .unwrap()
                .try_get("n")
                .unwrap()
        };
        assert_eq!(sealed, 4);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channels_api_session_only() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let token = ts
            .issue_token(&[
                "hosts:read",
                "hosts:write",
                "policies:read",
                "policies:write",
                "jobs:read",
                "jobs:run",
                "metrics:read",
                "snapshots:read",
                "snapshots:delete",
            ])
            .await;
        let client = bearer_client(&token);
        let base = ts.rest_url();
        let body = webhook("hook", "http://127.0.0.1:1/x", None);

        let list = client
            .get(format!("{base}{CHANNELS}"))
            .send()
            .await
            .unwrap();
        assert_eq!(list.status(), 401, "[{}]", backend.name());
        let create = client
            .post(format!("{base}{CHANNELS}"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(create.status(), 401);
        let test = client
            .post(format!("{base}{CHANNELS}/test"))
            .json(&json!({ "channel": body }))
            .send()
            .await
            .unwrap();
        assert_eq!(test.status(), 401);
        let rest = Rest::new(&ts).await;
        let (_, list, _) = rest.get(CHANNELS).await;
        assert!(list.as_array().unwrap().is_empty());
        ts.stop().await;
    }
}

fn errors(body: &Value) -> Vec<(String, String)> {
    body["errors"]
        .as_array()
        .unwrap_or_else(|| panic!("no errors in {body}"))
        .iter()
        .map(|e| {
            (
                e["path"].as_str().unwrap().to_string(),
                e["message"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn has(errors: &[(String, String)], path: &str, message: &str) -> bool {
    errors.iter().any(|(p, m)| p == path && m == message)
}

async fn secret_count(ts: &TestServer) -> i64 {
    use sqlx::Row;
    sqlx::query("SELECT COUNT(*) AS n FROM secrets WHERE kind = 'notification-channel'")
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .try_get("n")
        .unwrap()
}

async fn put(rest: &Rest, id: &str, body: &Value) -> (u16, Value, String) {
    rest.send(Method::PUT, &format!("{CHANNELS}/{id}"), Some(body))
        .await
}

async fn delete(rest: &Rest, id: &str) -> (u16, Value, String) {
    rest.send(Method::DELETE, &format!("{CHANNELS}/{id}"), None)
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channels_api_field_errors() {
    const NO_URL: &str = "Enter a webhook URL starting with https://.";
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let rest = Rest::new(&ts).await;

        let telegram = json!({ "type": "telegram", "name": " ", "alert_types": [],
            "settings": { "chat_id": " " } });
        let (status, body, _) = rest.post(CHANNELS, &telegram).await;
        assert_eq!(status, 422, "[{}]", backend.name());
        let e = errors(&body);
        assert!(has(&e, "name", "Enter a name for the channel."), "{e:?}");
        assert!(has(&e, "alert_types", "Choose at least one alert type."));
        assert!(has(&e, "secret", "Enter a bot token."));
        assert!(has(&e, "settings.chat_id", "Enter a chat ID."));

        let slack =
            json!({ "type": "slack", "name": "sl", "secret": "http://hooks.example.test/x" });
        let (status, body, _) = rest.post(CHANNELS, &slack).await;
        assert_eq!(status, 422);
        assert!(has(&errors(&body), "settings.url", NO_URL));
        let (_, body, _) = rest
            .post(CHANNELS, &json!({ "type": "slack", "name": "sl" }))
            .await;
        assert!(has(&errors(&body), "settings.url", NO_URL));

        let (_, body, _) = rest
            .post(CHANNELS, &webhook("hook", "ftp://example.test", None))
            .await;
        assert!(has(
            &errors(&body),
            "settings.url",
            "Enter a URL starting with http:// or https://."
        ));

        let email = json!({ "type": "email", "name": "mail",
            "settings": { "port": 0, "from": "nope" } });
        let (_, body, _) = rest.post(CHANNELS, &email).await;
        let e = errors(&body);
        assert!(
            has(&e, "settings.smtp_host", "Enter the SMTP host."),
            "{e:?}"
        );
        assert!(has(&e, "settings.port", "Enter a port from 1 to 65535."));
        assert!(has(
            &e,
            "settings.recipients",
            "Enter a recipient or set an admin email in Settings, User."
        ));
        assert!(e.iter().any(|(p, _)| p == "settings.from"));
        let email = json!({ "type": "email", "name": "mail",
            "settings": { "smtp_host": "h", "port": 70000, "from": "a@example.test",
                "recipients": ["not an address"] } });
        let (_, body, _) = rest.post(CHANNELS, &email).await;
        let e = errors(&body);
        assert!(has(&e, "settings.port", "Enter a port from 1 to 65535."));
        assert!(e.iter().any(|(p, _)| p == "settings.recipients"));

        let ok = webhook("hook", "http://127.0.0.1:1/x", None);
        assert_eq!(rest.post(CHANNELS, &ok).await.0, 201);
        let (status, body, _) = rest.post(CHANNELS, &ok).await;
        assert_eq!(status, 422);
        assert!(has(
            &errors(&body),
            "name",
            "A channel with this name already exists."
        ));
        let (_, list, _) = rest.get(CHANNELS).await;
        assert_eq!(list.as_array().unwrap().len(), 1);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channels_api_update_keeps_secret() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let rest = Rest::new(&ts).await;
        let stub = Stub::start().await;
        let (_, created, _) = rest
            .post(CHANNELS, &webhook("hook", &stub.url("/a"), Some("keep-me")))
            .await;
        let id = created["id"].as_str().unwrap();

        let mut change = webhook("renamed", &stub.url("/b"), None);
        change["alert_types"] = json!(["backup_failed", "check_failed", "backup_failed"]);
        change["enabled"] = json!(false);
        let (status, saved, text) = put(&rest, id, &change).await;
        assert_eq!(status, 200, "[{}] {text}", backend.name());
        assert_eq!(saved["name"], "renamed");
        assert_eq!(saved["enabled"], false);
        assert_eq!(
            saved["alert_types"],
            json!(["backup_failed", "check_failed"])
        );
        assert_eq!(saved["secret_set"], true);
        assert_eq!(saved["settings"]["url"], stub.url("/b"));

        rest.post(&format!("{CHANNELS}/test"), &json!({ "channel_id": id }))
            .await;
        let hits = stub.hits();
        assert_eq!(hits[0].path, "/b");
        assert_eq!(hits[0].authorization.as_deref(), Some("Bearer keep-me"));
        assert_eq!(secret_count(&ts).await, 1);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channels_api_replace_secret() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let rest = Rest::new(&ts).await;
        let stub = Stub::start().await;
        let (_, created, _) = rest
            .post(CHANNELS, &webhook("hook", &stub.url("/a"), Some("old")))
            .await;
        let id = created["id"].as_str().unwrap();

        let (status, _, text) =
            put(&rest, id, &webhook("hook", &stub.url("/a"), Some("new"))).await;
        assert_eq!(status, 200, "[{}] {text}", backend.name());
        rest.post(&format!("{CHANNELS}/test"), &json!({ "channel_id": id }))
            .await;
        assert_eq!(stub.hits()[0].authorization.as_deref(), Some("Bearer new"));
        assert_eq!(secret_count(&ts).await, 1);

        let other = json!({ "type": "telegram", "name": "hook", "secret": "t",
            "settings": { "chat_id": "1" } });
        let (status, body, _) = put(&rest, id, &other).await;
        assert_eq!(status, 422);
        assert!(errors(&body).iter().any(|(p, _)| p == "type"));

        let (_, second, _) = rest
            .post(CHANNELS, &webhook("second", &stub.url("/c"), None))
            .await;
        let second_id = second["id"].as_str().unwrap();
        let (status, body, _) =
            put(&rest, second_id, &webhook("hook", &stub.url("/c"), None)).await;
        assert_eq!(status, 422);
        assert!(has(
            &errors(&body),
            "name",
            "A channel with this name already exists."
        ));
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channels_api_delete() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let rest = Rest::new(&ts).await;
        let (_, created, _) = rest
            .post(
                CHANNELS,
                &webhook("hook", "http://127.0.0.1:1/x", Some("s")),
            )
            .await;
        let id = created["id"].as_str().unwrap();
        assert_eq!(secret_count(&ts).await, 1);

        assert_eq!(delete(&rest, id).await.0, 204, "[{}]", backend.name());
        assert_eq!(rest.get(&format!("{CHANNELS}/{id}")).await.0, 404);
        assert_eq!(secret_count(&ts).await, 0);
        assert_eq!(delete(&rest, id).await.0, 404);
        ts.stop().await;
    }
}

fn config_webhook(url: &str) -> ConfigChannel {
    ConfigChannel {
        name: "from-config".to_string(),
        enabled: true,
        alerts: vec![AlertType::BackupFailed],
        kind: ChannelSpec::Webhook {
            url: url.to_string(),
            secret: Some(Secret::new("cfg-secret")),
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channels_api_config_channel_read_only() {
    for backend in backends() {
        let stub = Stub::start().await;
        let ts = TestServer::start_with(&backend, |c| {
            c.notifications.channels = vec![config_webhook(&stub.url("/cfg"))];
        })
        .await;
        let rest = Rest::new(&ts).await;

        let (_, list, text) = rest.get(CHANNELS).await;
        assert_eq!(list.as_array().unwrap().len(), 1, "[{}]", backend.name());
        assert_eq!(list[0]["source"], "config");
        assert_eq!(list[0]["secret_set"], true);
        assert_eq!(list[0]["alert_types"], json!(["backup_failed"]));
        assert!(!text.contains("cfg-secret"));
        let id = list[0]["id"].as_str().unwrap();
        let message = "This channel is defined in the server configuration. Change it there and restart the server.";

        let change = webhook("from-config", "http://127.0.0.1:1/x", None);
        let (status, body, _) = put(&rest, id, &change).await;
        assert_eq!(status, 409);
        assert_eq!(body["message"], message);
        let (status, body, _) = delete(&rest, id).await;
        assert_eq!(status, 409);
        assert_eq!(body["message"], message);

        let (status, body, _) = rest.post(CHANNELS, &change).await;
        assert_eq!(status, 422);
        assert!(has(
            &errors(&body),
            "name",
            "A channel with this name already exists."
        ));
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channels_api_config_channel_test() {
    for backend in backends() {
        let stub = Stub::start().await;
        let ts = TestServer::start_with(&backend, |c| {
            c.notifications.channels = vec![config_webhook(&stub.url("/cfg"))];
        })
        .await;
        let rest = Rest::new(&ts).await;
        let (_, list, _) = rest.get(CHANNELS).await;
        let id = list[0]["id"].as_str().unwrap();

        let (status, tested, _) = rest
            .post(&format!("{CHANNELS}/test"), &json!({ "channel_id": id }))
            .await;
        assert_eq!(status, 200, "[{}]", backend.name());
        assert_eq!(tested["ok"], true, "{tested}");
        let hits = stub.hits();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "/cfg");
        assert_eq!(hits[0].authorization.as_deref(), Some("Bearer cfg-secret"));
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channels_api_email_default_recipient() {
    use lodestore_server::auth::account::admin_email;
    use lodestore_server::notify::channels::load_channel;
    use lodestore_server::notify::email::build_for;
    use lodestore_server::notify::message::test_message;

    const NO_RECIPIENT: &str = "Enter a recipient or set an admin email in Settings, User.";
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let rest = Rest::new(&ts).await;
        let mail = |name: &str, recipients: Value| {
            json!({ "type": "email", "name": name, "enabled": true,
                "settings": { "smtp_host": "127.0.0.1", "port": 1,
                    "from": "Lodestore <lodestore@example.test>", "recipients": recipients } })
        };

        let (status, body, _) = rest.post(CHANNELS, &mail("mail", json!([]))).await;
        assert_eq!(status, 422, "[{}]", backend.name());
        assert!(has(&errors(&body), "settings.recipients", NO_RECIPIENT));
        let (status, tested, _) = rest
            .post(
                &format!("{CHANNELS}/test"),
                &json!({ "channel": mail("mail", json!([])) }),
            )
            .await;
        assert_eq!(status, 200);
        assert_eq!(tested["ok"], false);
        assert_eq!(tested["message"], NO_RECIPIENT);

        let (status, _, _) = rest
            .send(
                Method::PUT,
                "/api/v1/account",
                Some(&json!({ "email": "ops@example.com" })),
            )
            .await;
        assert_eq!(status, 200);
        let (status, own, text) = rest.post(CHANNELS, &mail("mail", json!([]))).await;
        assert_eq!(status, 201, "{text}");
        let (_, named, _) = rest
            .post(CHANNELS, &mail("named", json!(["oncall@example.com"])))
            .await;
        assert_eq!(own["settings"]["recipients"], json!([]));

        let state = ts.state();
        let admin = admin_email(state).await.unwrap();
        let message = test_message(&state.config.public_url);
        let render = |channel: &lodestore_server::notify::channels::Channel| {
            let built = build_for(channel, &message, admin.as_deref()).unwrap();
            String::from_utf8(built.formatted()).unwrap()
        };
        let default = load_channel(state, own["id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        let raw = render(&default);
        assert!(raw.contains("To: ops@example.com"), "{raw}");
        assert!(raw.contains("Subject: [Lodestore] Test message"), "{raw}");
        let explicit = load_channel(state, named["id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        let raw = render(&explicit);
        assert!(raw.contains("To: oncall@example.com"), "{raw}");
        assert!(!raw.contains("ops@example.com"), "{raw}");

        let (_, tested, _) = rest
            .post(
                &format!("{CHANNELS}/test"),
                &json!({ "channel_id": own["id"] }),
            )
            .await;
        assert_eq!(tested["ok"], false);
        assert!(
            tested["message"].as_str().unwrap().starts_with("SMTP:"),
            "{tested}"
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channels_api_thresholds() {
    const URL: &str = "/api/v1/notifications/thresholds";
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let rest = Rest::new(&ts).await;

        let (status, body, _) = rest.get(URL).await;
        assert_eq!(status, 200, "[{}]", backend.name());
        assert_eq!(
            body,
            json!({ "offline_after": "15m", "cert_days": 14,
                "offline_floor": "5m", "cert_days_max": 60 })
        );

        let (status, body, _) = rest
            .send(Method::PUT, URL, Some(&json!({ "offline_after": "2m" })))
            .await;
        assert_eq!(status, 422);
        assert!(has(
            &errors(&body),
            "offline_after",
            "Use a duration of 5m or more, such as 15m or 1h."
        ));
        let (status, body, _) = rest
            .send(
                Method::PUT,
                URL,
                Some(&json!({ "offline_after": "500000000000y" })),
            )
            .await;
        assert_eq!(status, 422);
        assert!(has(
            &errors(&body),
            "offline_after",
            "Use a duration of 30d or less."
        ));
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES ('alerts.offline_after_s', '9000000000000')",
        )
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
        let (_, body, _) = rest.get(URL).await;
        assert_eq!(body["offline_after"], "30d");
        sqlx::query("DELETE FROM settings WHERE key = 'alerts.offline_after_s'")
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        for bad in [61, 0] {
            let (status, body, _) = rest
                .send(Method::PUT, URL, Some(&json!({ "cert_days": bad })))
                .await;
            assert_eq!(status, 422);
            assert!(has(
                &errors(&body),
                "cert_days",
                "Enter a whole number of days from 1 to 60."
            ));
        }
        let (_, body, _) = rest.get(URL).await;
        assert_eq!(body["offline_after"], "15m");

        let (status, body, _) = rest
            .send(
                Method::PUT,
                URL,
                Some(&json!({ "offline_after": "30m", "cert_days": 7 })),
            )
            .await;
        assert_eq!(status, 200);
        assert_eq!(body["offline_after"], "30m");
        assert_eq!(body["cert_days"], 7);
        let loaded = lodestore_server::alerts::model::load_thresholds(ts.state())
            .await
            .unwrap();
        assert_eq!(
            loaded.offline_after,
            std::time::Duration::from_secs(30 * 60)
        );
        assert_eq!(loaded.cert_days, 7);

        let (_, body, _) = rest
            .send(Method::PUT, URL, Some(&json!({ "cert_days": 21 })))
            .await;
        assert_eq!(body["offline_after"], "30m");
        assert_eq!(body["cert_days"], 21);
        ts.stop().await;
    }
}
