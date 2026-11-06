mod common;

use common::{Backend, TestServer, backends, bearer_client};
use serde_json::{Value, json};

fn migration_sql(backend: &Backend) -> &'static str {
    match backend {
        Backend::Sqlite => include_str!("../migrations/sqlite/0025_restore_scopes.sql"),
        Backend::Postgres(_) => include_str!("../migrations/postgres/0025_restore_scopes.sql"),
    }
}

async fn set_scopes(ts: &TestServer, name: &str, scopes_json: &str) {
    sqlx::query("UPDATE api_tokens SET scopes_json = $1 WHERE name = $2")
        .bind(scopes_json)
        .bind(name)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
}

async fn token_named(ts: &TestServer, name: &str, scopes: &[&str]) -> String {
    let response = ts
        .api()
        .await
        .post(
            "/api/v1/tokens",
            &json!({ "name": name, "scopes": scopes, "expires_in_days": null }),
        )
        .await;
    assert_eq!(response.status(), 201);
    let body: Value = response.json().await.unwrap();
    body["token"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_scopes_upgraded_token_downloads_only() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let old = token_named(&ts, "old", &["hosts:read"]).await;
        token_named(&ts, "new", &["snapshots:read", "restore:host"]).await;
        set_scopes(&ts, "old", r#"["hosts:read","restore"]"#).await;

        let before = ts.api().await.get("/api/v1/tokens").await;
        assert_eq!(before.status(), 500, "[{}]", backend.name());

        for statement in migration_sql(&backend)
            .split(';')
            .filter(|s| !s.trim().is_empty())
        {
            sqlx::query(sqlx::AssertSqlSafe(statement.to_string()))
                .execute(&ts.state().db.pool)
                .await
                .unwrap();
        }

        let response = ts.api().await.get("/api/v1/tokens").await;
        assert_eq!(response.status(), 200, "[{}]", backend.name());
        let tokens: Vec<Value> = response.json().await.unwrap();
        let scopes =
            |name: &str| tokens.iter().find(|t| t["name"] == name).unwrap()["scopes"].clone();
        assert_eq!(scopes("old"), json!(["hosts:read", "restore:download"]));
        assert_eq!(scopes("new"), json!(["snapshots:read", "restore:host"]));

        let client = bearer_client(&old);
        let base = ts.rest_url();
        let download = client
            .post(format!("{base}/api/v1/downloads"))
            .json(&json!({ "snapshot_id": "0".repeat(64), "items": [], "format": "zip" }))
            .send()
            .await
            .unwrap();
        let status = download.status().as_u16();
        assert!(
            status != 401 && status != 403,
            "[{}] {status}",
            backend.name()
        );

        let restore = client
            .post(format!("{base}/api/v1/restores"))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(restore.status(), 403, "[{}]", backend.name());
        let body: Value = restore.json().await.unwrap();
        assert_eq!(
            body["message"],
            "The token needs the restore:host or restore:in-place scope."
        );
        ts.stop().await;
    }
}
