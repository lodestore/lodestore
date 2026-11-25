use serde_json::Value;

use crate::common::{TestServer, backends};

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
