mod common;

use common::{ADMIN_PASSWORD, Api, TestServer, backends, bearer_client};
use serde_json::{Value, json};
use sqlx::Row;

const NEW_PASSWORD: &str = "a brand new passphrase";

fn account_url(ts: &TestServer, tail: &str) -> String {
    format!("{}/api/v1/account{tail}", ts.rest_url())
}

async fn put_account(ts: &TestServer, api: &Api, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .put(account_url(ts, ""))
        .header("cookie", api.cookie())
        .header("x-csrf-token", &api.csrf)
        .json(body)
        .send()
        .await
        .unwrap()
}

async fn change_password(
    ts: &TestServer,
    api: &Api,
    current: &str,
    new: &str,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(account_url(ts, "/password"))
        .header("cookie", api.cookie())
        .header("x-csrf-token", &api.csrf)
        .json(&json!({ "current_password": current, "new_password": new }))
        .send()
        .await
        .unwrap()
}

async fn session_status(api: &Api) -> u16 {
    api.get("/api/v1/auth/session").await.status().as_u16()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn account_email_roundtrip() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;

        let shown: Value = api.get("/api/v1/account").await.json().await.unwrap();
        assert_eq!(shown, json!({ "username": "admin", "email": null }));

        let saved = put_account(&ts, &api, &json!({ "email": " ops@example.com " })).await;
        assert_eq!(saved.status(), 200, "[{}]", backend.name());
        let saved: Value = saved.json().await.unwrap();
        assert_eq!(saved["email"], "ops@example.com");
        let shown: Value = api.get("/api/v1/account").await.json().await.unwrap();
        assert_eq!(shown["email"], "ops@example.com");
        assert_eq!(
            lodestore_server::auth::account::admin_email(ts.state())
                .await
                .unwrap()
                .as_deref(),
            Some("ops@example.com")
        );

        for cleared in [json!({ "email": null }), json!({ "email": "" })] {
            put_account(&ts, &api, &json!({ "email": "ops@example.com" })).await;
            let response = put_account(&ts, &api, &cleared).await;
            assert_eq!(response.status(), 200);
            let body: Value = response.json().await.unwrap();
            assert!(body["email"].is_null(), "{body}");
        }
        assert_eq!(
            lodestore_server::auth::account::admin_email(ts.state())
                .await
                .unwrap(),
            None
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn account_email_invalid() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        put_account(&ts, &api, &json!({ "email": "ops@example.com" })).await;

        let response = put_account(&ts, &api, &json!({ "email": "nope" })).await;
        assert_eq!(response.status(), 422, "[{}]", backend.name());
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["errors"][0]["path"], "email");
        assert_eq!(
            body["errors"][0]["message"],
            "Enter an address such as admin@example.com."
        );
        let shown: Value = api.get("/api/v1/account").await.json().await.unwrap();
        assert_eq!(shown["email"], "ops@example.com");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn account_wrong_password_keeps_session() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;

        let response = change_password(&ts, &api, "not the password", NEW_PASSWORD).await;
        assert_eq!(response.status(), 422, "[{}]", backend.name());
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["code"], "wrong_password");
        assert_eq!(body["message"], "Current password is wrong.");
        assert_eq!(session_status(&api).await, 200);
        Api::login(&ts.rest_url(), "admin", ADMIN_PASSWORD).await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn account_weak_password() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;

        let response = change_password(&ts, &api, ADMIN_PASSWORD, "short").await;
        assert_eq!(response.status(), 422, "[{}]", backend.name());
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["code"], "weak_password");
        assert_eq!(session_status(&api).await, 200);
        Api::login(&ts.rest_url(), "admin", ADMIN_PASSWORD).await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn account_password_rate_limited() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;

        let mut last = 0;
        for _ in 0..8 {
            last = change_password(&ts, &api, "guess", NEW_PASSWORD)
                .await
                .status()
                .as_u16();
            if last == 429 {
                break;
            }
            assert_eq!(last, 422, "[{}]", backend.name());
        }
        assert_eq!(last, 429, "[{}]", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn account_password_signs_out_other_sessions() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let first = ts.api().await;
        let second = Api::login(&ts.rest_url(), "admin", ADMIN_PASSWORD).await;
        assert_eq!(session_status(&second).await, 200);

        let response = change_password(&ts, &first, ADMIN_PASSWORD, NEW_PASSWORD).await;
        assert_eq!(response.status(), 204, "[{}]", backend.name());
        assert_eq!(session_status(&first).await, 200);
        assert_eq!(session_status(&second).await, 401);

        let client = reqwest::Client::new();
        let login = |password: &'static str| {
            let url = format!("{}/api/v1/auth/login", ts.rest_url());
            let client = client.clone();
            async move {
                client
                    .post(url)
                    .json(&json!({ "username": "admin", "password": password }))
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            }
        };
        assert_eq!(login(NEW_PASSWORD).await, 204);
        assert_eq!(login(ADMIN_PASSWORD).await, 401);
        let left: i64 = sqlx::query("SELECT COUNT(*) AS n FROM sessions")
            .fetch_one(&ts.state().db.pool)
            .await
            .unwrap()
            .get("n");
        assert_eq!(left, 2);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn account_session_only() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let token = ts
            .issue_token(&["policies:write", "jobs:run", "jobs:read"])
            .await;
        let client = bearer_client(&token);

        let response = client.get(account_url(&ts, "")).send().await.unwrap();
        assert_eq!(response.status(), 401, "[{}]", backend.name());
        let response = client
            .put(account_url(&ts, ""))
            .json(&json!({ "email": "ops@example.com" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
        let response = client
            .post(account_url(&ts, "/password"))
            .json(&json!({ "current_password": ADMIN_PASSWORD, "new_password": NEW_PASSWORD }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
        assert_eq!(
            lodestore_server::auth::account::admin_email(ts.state())
                .await
                .unwrap(),
            None
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn account_password_change_beats_pending_sign_in() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let pool = &ts.state().db.pool;
        let hash_of = || async {
            let row = sqlx::query("SELECT id, password_hash FROM admin_users")
                .fetch_one(pool)
                .await
                .unwrap();
            (
                row.get::<String, _>("id"),
                row.get::<String, _>("password_hash"),
            )
        };
        let sessions = || async {
            sqlx::query("SELECT COUNT(*) AS n FROM sessions")
                .fetch_one(pool)
                .await
                .unwrap()
                .get::<i64, _>("n")
        };
        let (id, old_hash) = hash_of().await;

        let response = change_password(&ts, &api, ADMIN_PASSWORD, NEW_PASSWORD).await;
        assert_eq!(response.status(), 204, "[{}]", backend.name());
        let before = sessions().await;

        let stale = lodestore_server::auth::insert_session(ts.state(), &id, &old_hash)
            .await
            .unwrap();
        assert!(stale.is_none(), "[{}]", backend.name());
        assert_eq!(sessions().await, before, "[{}]", backend.name());

        let (_, new_hash) = hash_of().await;
        let fresh = lodestore_server::auth::insert_session(ts.state(), &id, &new_hash)
            .await
            .unwrap();
        assert!(fresh.is_some(), "[{}]", backend.name());
        assert_eq!(sessions().await, before + 1);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn change_password_rejects_new_password_over_1024_bytes() {
    for backend in backends() {
        let name = backend.name();
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;

        for new in ["a".repeat(1025), "ж".repeat(513)] {
            let response = change_password(&ts, &api, ADMIN_PASSWORD, &new).await;
            assert_eq!(response.status(), 422, "[{name}]");
            let body: Value = response.json().await.unwrap();
            assert_eq!(body["code"], "weak_password");
            assert_eq!(body["message"], "At most 1024 bytes.");
        }

        let response = change_password(&ts, &api, &"a".repeat(4097), NEW_PASSWORD).await;
        assert_eq!(response.status(), 422, "[{name}]");
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["code"], "wrong_password");

        let ok = change_password(&ts, &api, ADMIN_PASSWORD, &"a".repeat(1024)).await;
        assert_eq!(ok.status(), 204, "[{name}]");
        ts.stop().await;
    }
}
