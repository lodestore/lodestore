use lodestore_format::store::policy::{
    STORAGE_PRESETS, agent_iam_policy_json, agent_minio_commands,
};
use serde_json::Value;

use crate::common::{TestServer, backends};

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_policy_preset_gcs_returns_gcloud_commands() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let response = api
            .get("/api/v1/agent-policy?preset=google-cloud-storage&bucket=repo-bucket&prefix=lode")
            .await;
        assert_eq!(response.status(), 200, "[{}]", backend.name());
        let body: Value = response.json().await.unwrap();
        let rights = &body["rights"];
        assert_eq!(rights["mechanism"], "commands");
        let text = rights["text"].as_str().unwrap();
        assert!(text.contains("storage.objects.create,storage.objects.get,storage.objects.list"));
        assert!(text.contains("gs://repo-bucket"));
        assert!(
            rights["server_text"]
                .as_str()
                .unwrap()
                .contains("legacyBucketReader")
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_policy_without_preset_is_unchanged() {
    let ts = TestServer::start(&backends()[0]).await;
    let api = ts.api().await;
    let body: Value = api
        .get("/api/v1/agent-policy?bucket=repo-bucket&prefix=lode")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        body["iam_json"],
        agent_iam_policy_json("repo-bucket", "lode")
    );
    assert_eq!(
        body["minio_commands"],
        agent_minio_commands("repo-bucket", "lode")
    );
    assert!(body.get("rights").is_none());
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_policy_every_preset_answers() {
    let ts = TestServer::start(&backends()[0]).await;
    let api = ts.api().await;
    for p in STORAGE_PRESETS {
        let prefix = if p.id == "sftp" {
            "/srv/lodestore"
        } else {
            "lode"
        };
        let response = api
            .get(&format!(
                "/api/v1/agent-policy?preset={}&bucket=repo-bucket&prefix={}",
                p.id,
                encode(prefix)
            ))
            .await;
        assert_eq!(response.status(), 200, "{}", p.id);
        let body: Value = response.json().await.unwrap();
        assert_eq!(
            body["rights"]["mechanism"],
            p.mechanism.as_str(),
            "{}",
            p.id
        );
        assert!(!body["rights"]["text"].as_str().unwrap().is_empty());
    }
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_policy_refuses_unsafe_input() {
    let ts = TestServer::start(&backends()[0]).await;
    let api = ts.api().await;
    for bad in ["a;rm -rf", "$(id)", "it's", "a b", "a\nb", "a`id`"] {
        let bad = encode(bad);
        for query in [
            format!("preset=minio&bucket={bad}"),
            format!("preset=minio&bucket=repo-bucket&prefix={bad}"),
            format!("preset=sftp&prefix=/srv/x&user={bad}"),
        ] {
            let response = api.get(&format!("/api/v1/agent-policy?{query}")).await;
            assert_eq!(response.status(), 422, "{query}");
        }
    }
    let dots = api
        .get("/api/v1/agent-policy?preset=minio&bucket=repo-bucket&prefix=a/../b")
        .await;
    assert_eq!(dots.status(), 422);
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_policy_unknown_preset_is_refused() {
    let ts = TestServer::start(&backends()[0]).await;
    let api = ts.api().await;
    let response = api
        .get("/api/v1/agent-policy?preset=nope&bucket=repo-bucket")
        .await;
    assert_eq!(response.status(), 422);
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_policy_needs_a_session() {
    let ts = TestServer::start(&backends()[0]).await;
    let response = reqwest::get(format!(
        "{}/api/v1/agent-policy?preset=minio&bucket=repo-bucket",
        ts.rest_url()
    ))
    .await
    .unwrap();
    assert_eq!(response.status(), 401);
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn storage_presets_need_a_session() {
    let ts = TestServer::start(&backends()[0]).await;
    let response = reqwest::get(format!("{}/api/v1/storage-presets", ts.rest_url()))
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn storage_presets_list_every_preset() {
    let ts = TestServer::start(&backends()[0]).await;
    let api = ts.api().await;
    let body: Value = api
        .get("/api/v1/storage-presets")
        .await
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    let want: Vec<&str> = STORAGE_PRESETS.iter().map(|p| p.id).collect();
    assert_eq!(ids, want);
    ts.stop().await;
}
