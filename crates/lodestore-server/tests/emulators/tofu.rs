use std::process::Command;
use std::time::Duration;

use crate::agent_material::{body, create, sftp_draft};
use crate::common::{Backend, TestServer};
use lodestore_format::store::SftpStore;
use lodestore_server::repos::db::writer_material;
use lodestore_testkit::sshd::SshdServer;
use serde_json::{Value, json};

fn unpinned_draft(sshd: &SshdServer, name: &str) -> Value {
    let mut draft = sftp_draft(sshd, name);
    draft["location"]["options"]
        .as_object_mut()
        .unwrap()
        .remove("host_key");
    draft
}

fn exec(sshd: &SshdServer, args: &[&str]) -> String {
    let out = Command::new("docker")
        .arg("exec")
        .arg(sshd.container())
        .args(args)
        .output()
        .expect("docker exec");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn files(sshd: &SshdServer) -> String {
    exec(sshd, &["find", sshd.root(), "-type", "f"])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sftp_tofu_check_offers_the_host_key_and_writes_nothing() {
    let sshd = SshdServer::start().await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;
    let (mark, before) = (sshd.log().len(), files(&sshd));

    for empty in [None, Some("  ")] {
        let mut draft = unpinned_draft(&sshd, "sftp-repo");
        if let Some(value) = empty {
            draft["location"]["options"]["host_key"] = json!(value);
        }
        let response = api.post("/api/v1/repositories/check", &draft).await;
        assert_eq!(response.status(), 200);
        let result = body(response).await;
        assert_eq!(result["blocking"], true, "{result}");
        assert_eq!(result["guarantee_level"], "unchecked");
        assert_eq!(result["guarantee_notes"], json!([]));
        let offered = &result["host_key"];
        assert_eq!(
            offered["fingerprint"],
            SftpStore::host_key_fingerprint(&sshd.host_key()).unwrap()
        );
        let host_key = sshd.host_key();
        let bare = host_key.split_whitespace().take(2).collect::<Vec<_>>();
        assert_eq!(offered["key"], bare.join(" "));
        let write = result["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == "write")
            .unwrap();
        assert_eq!(write["status"], "fail");
        assert!(
            write["error"].as_str().unwrap().contains("host key"),
            "{write}"
        );
    }

    tokio::time::sleep(Duration::from_millis(300)).await;
    let log = sshd.log();
    let tail = log.get(mark..).unwrap_or("");
    for forbidden in ["Accepted publickey", "Failed publickey", "subsystem"] {
        assert!(!tail.contains(forbidden), "{forbidden}: {tail}");
    }
    assert_eq!(files(&sshd), before);

    for path in [
        "/api/v1/repositories/wizard",
        "/api/v1/repositories/connect",
    ] {
        let draft = unpinned_draft(&sshd, "sftp-repo");
        let request = if path.ends_with("connect") {
            json!({ "draft": draft, "password": "correct horse battery" })
        } else {
            draft
        };
        let response = api.post(path, &request).await;
        assert_eq!(response.status(), 422, "{path}");
        assert!(
            body(response).await["message"]
                .as_str()
                .unwrap()
                .contains("host key"),
            "{path}"
        );
    }

    for (what, mutate) in [("endpoint", "endpoint"), ("user", "user"), ("key", "key")] {
        let mut draft = unpinned_draft(&sshd, "sftp-repo");
        draft["location"]["options"]
            .as_object_mut()
            .unwrap()
            .remove(mutate);
        let response = api.post("/api/v1/repositories/check", &draft).await;
        assert_eq!(response.status(), 422, "{what}");
    }
    let mut draft = unpinned_draft(&sshd, "sftp-repo");
    draft["location"]["options"]["bogus"] = json!("x");
    let response = api.post("/api/v1/repositories/check", &draft).await;
    assert_eq!(response.status(), 422);

    let mut draft = unpinned_draft(&sshd, "sftp-repo");
    draft["location"]["options"]["endpoint"] = json!("127.0.0.1:1");
    let response = api.post("/api/v1/repositories/check", &draft).await;
    assert_eq!(response.status(), 200);
    let result = body(response).await;
    assert_eq!(result["blocking"], true);
    assert!(result.get("host_key").is_none(), "{result}");
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn sftp_tofu_confirmed_key_is_pinned_and_reaches_the_agent() {
    let sshd = SshdServer::start().await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;

    let response = api
        .post(
            "/api/v1/repositories/check",
            &unpinned_draft(&sshd, "sftp-repo"),
        )
        .await;
    let offered = body(response).await["host_key"]["key"]
        .as_str()
        .unwrap()
        .to_string();

    let mut draft = sftp_draft(&sshd, "sftp-repo");
    draft["location"]["options"]["host_key"] = json!(offered);
    let response = api.post("/api/v1/repositories/check", &draft).await;
    assert_eq!(response.status(), 200);
    let result = body(response).await;
    assert_eq!(result["blocking"], false, "{result}");
    assert!(result.get("host_key").is_none(), "{result}");

    let id = create(&api, &draft).await;
    let stored = crate::agent_material::stored_location(&ts, &id).await;
    assert_eq!(stored["host_key"], offered);
    let material = writer_material(ts.state(), &id).await.unwrap();
    assert_eq!(material.location.options["host_key"], offered);
    ts.stop().await;
}
