use crate::common::{Api, TestServer, backends, seed_agent};

use std::collections::BTreeMap;
use std::time::Duration;

use lodestore_format::crypto::Entropy;
use lodestore_format::repo::{InitOptions, PendingInit};
use lodestore_server::repos::db::{
    AccessKey, GuaranteeWrite, NewRepository, RepoExtras, RepoLocation, create_repository_ext,
    replace_agent_creds, replace_agent_options,
};
use serde_json::Value;

fn key(id: &str) -> AccessKey {
    AccessKey {
        access_key_id: id.into(),
        secret_access_key: "secret".into(),
    }
}

async fn repo(
    ts: &TestServer,
    name: &str,
    backend: &str,
    options: &[(&str, &str)],
    server_creds: Option<AccessKey>,
    agent_creds: Option<AccessKey>,
    extras: RepoExtras,
) -> String {
    let pending = PendingInit::generate(&mut Entropy::os(), InitOptions::default())
        .expect("generating a repository");
    let keys = pending.read_keys();
    create_repository_ext(
        ts.state(),
        NewRepository {
            name: name.into(),
            location: RepoLocation {
                backend: backend.into(),
                options: options
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect::<BTreeMap<_, _>>(),
            },
            format_repo_id: *keys.repo_id(),
            config_hash: lodestore_format::config::config_hash(pending.config_bytes()),
            master: keys.master().clone(),
            server_slot_secret: None,
            server_creds,
            agent_creds,
        },
        extras,
    )
    .await
    .expect("registering the repository")
}

async fn s3_repo(ts: &TestServer, server_key: &str, agent_key: &str) -> String {
    repo(
        ts,
        "s3",
        "s3",
        &[("bucket", "b"), ("endpoint", "http://127.0.0.1:9")],
        Some(key(server_key)),
        Some(key(agent_key)),
        RepoExtras::default(),
    )
    .await
}

async fn revoke(api: &Api, host: &str) {
    let done = api
        .post(&format!("/api/v1/hosts/{host}/revoke"), &Value::Null)
        .await;
    assert_eq!(done.status(), 204);
    tokio::time::sleep(Duration::from_millis(5)).await;
}

async fn access(api: &Api, repo: &str) -> Value {
    let response = api
        .get(&format!("/api/v1/repositories/{repo}/storage-access"))
        .await;
    assert_eq!(response.status(), 200);
    response.json().await.expect("a body")
}

fn unchecked() -> GuaranteeWrite {
    GuaranteeWrite {
        level: "unchecked".into(),
        notes_json: "[]".into(),
        accepted: "unchecked".into(),
        read_accepted: "unchecked".into(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn storage_access_s3_revoke_then_rotation_asks_to_disable_the_old_key() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let id = s3_repo(&ts, "server-key", "agent-key").await;
        seed_agent(&ts, &id, "h1", "[]").await;

        let before = access(&api, &id).await;
        assert_eq!(before["status"], "no_revoked_hosts");

        revoke(&api, "h1").await;
        let held = access(&api, &id).await;
        assert_eq!(held["after_revoke"], "until_rotation");
        assert_eq!(held["rotate_via"], "agent-key");
        assert_eq!(held["status"], "revoked_hosts_hold_access");
        assert_eq!(held["revoked_hosts_since_rotation"], 1);

        replace_agent_creds(
            ts.state(),
            &id,
            &key("agent-key-2"),
            "ok",
            "[]",
            &unchecked(),
        )
        .await
        .unwrap();
        let rotated = access(&api, &id).await;
        assert_eq!(rotated["status"], "rotated_disable_old_key_at_provider");
        assert_eq!(rotated["revoked_hosts_since_rotation"], 0);
        assert_eq!(rotated["revoked_hosts_before_rotation"], 1);
        assert!(!rotated.to_string().contains("secret"), "{rotated}");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn storage_access_counts_hosts_that_need_an_update() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let azure = repo(
            &ts,
            "azure",
            "azblob",
            &[
                ("account_name", "lodeacct"),
                ("endpoint", "http://127.0.0.1:9/lodeacct"),
                ("container", "vended"),
            ],
            None,
            None,
            RepoExtras {
                secret_options: BTreeMap::from([("account_key".into(), "a2V5".into())]),
                agent_sas_vending: true,
                ..RepoExtras::default()
            },
        )
        .await;
        seed_agent(&ts, &azure, "old", "[]").await;
        seed_agent(&ts, &azure, "unknown", "[]").await;
        sqlx::query("UPDATE agents SET capabilities_json = NULL WHERE id = 'unknown'")
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        seed_agent(&ts, &azure, "new", r#"["sas_refresh_v1"]"#).await;
        assert_eq!(access(&api, &azure).await["hosts_need_agent_update"], 2);

        let s3 = s3_repo(&ts, "server-key", "agent-key").await;
        seed_agent(&ts, &s3, "s3-old", "[]").await;
        assert_eq!(access(&api, &s3).await["hosts_need_agent_update"], 0);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn storage_access_agent_options_rotation_asks_to_disable_the_old_key() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let id = repo(
            &ts,
            "gcs",
            "gcs",
            &[("bucket", "b")],
            None,
            None,
            RepoExtras {
                secret_options: BTreeMap::from([("credential".into(), "server".into())]),
                agent_options: BTreeMap::from([("credential".into(), "agent".to_string().into())]),
                ..RepoExtras::default()
            },
        )
        .await;
        seed_agent(&ts, &id, "h1", "[]").await;
        revoke(&api, "h1").await;
        let held = access(&api, &id).await;
        assert_eq!(held["status"], "revoked_hosts_hold_access");
        assert_eq!(held["rotate_via"], "agent-options");

        let generation = lodestore_server::repos::db::server_access(ts.state(), &id)
            .await
            .unwrap()
            .generation;
        let options = BTreeMap::from([("credential".to_string(), "agent-2".to_string().into())]);
        replace_agent_options(
            ts.state(),
            &id,
            generation,
            &options,
            "ok",
            "[]",
            &unchecked(),
        )
        .await
        .unwrap();
        let rotated = access(&api, &id).await;
        assert_eq!(rotated["status"], "rotated_disable_old_key_at_provider");
        assert_eq!(rotated["rotate_via"], "agent-options");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn storage_access_s3_agent_key_equal_to_server_key_is_reported() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let id = s3_repo(&ts, "same-key", "same-key").await;
        seed_agent(&ts, &id, "h1", "[]").await;
        revoke(&api, "h1").await;
        let shared = access(&api, &id).await;
        assert_eq!(shared["after_revoke"], "server_key_shared");
        assert!(shared["rotate_via"].is_null(), "{shared}");
        assert_eq!(shared["status"], "revoked_hosts_hold_access");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn storage_access_needs_a_session() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let id = s3_repo(&ts, "server-key", "agent-key").await;
        let anonymous = reqwest::get(format!(
            "{}/api/v1/repositories/{id}/storage-access",
            ts.rest_url()
        ))
        .await
        .unwrap();
        assert_eq!(anonymous.status(), 401);
        let unknown = api.get("/api/v1/repositories/nope/storage-access").await;
        assert_eq!(unknown.status(), 404);
        ts.stop().await;
    }
}
