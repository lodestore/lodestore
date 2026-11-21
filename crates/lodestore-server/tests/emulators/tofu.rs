use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use crate::agent_material::{HOUR, alerts_of, body, create, sftp_draft, stored_location};
use crate::common::{Api, Backend, TestServer};
use crate::host_key_push::{
    capabilities_vec, enroll_agent, host_key_of, open_session, open_session_at,
};
use lodestore_format::store::SftpStore;
use lodestore_proto::agent::v1::ServerMessage;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_server::repos::db::{GuaranteeWrite, Stored, replace_host_key, writer_material};
use lodestore_testkit::sshd::SshdServer;
use serde_json::{Value, json};
use sqlx::Row;

const OTHER_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPXNmebOc3wEWwOMVZdkvtvsvLmchHYR1uQeOBlJtQWZ";

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

async fn row(ts: &TestServer, id: &str, column: &'static str) -> Option<String> {
    let sql = match column {
        "host_key_mismatch_json" => {
            "SELECT CAST(host_key_mismatch_json AS TEXT) FROM repositories WHERE id = $1"
        }
        "location_json" => "SELECT location_json FROM repositories WHERE id = $1",
        "guarantee_level" => "SELECT guarantee_level FROM repositories WHERE id = $1",
        "guarantee_accepted" => "SELECT guarantee_accepted FROM repositories WHERE id = $1",
        "access_generation" => {
            "SELECT CAST(access_generation AS TEXT) FROM repositories WHERE id = $1"
        }
        "guarantee_unchecked_runs" => {
            "SELECT CAST(guarantee_unchecked_runs AS TEXT) FROM repositories WHERE id = $1"
        }
        other => panic!("unknown column {other}"),
    };
    sqlx::query(sql)
        .bind(id)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .get(0)
}

async fn generation(ts: &TestServer, id: &str) -> i64 {
    row(ts, id, "access_generation")
        .await
        .unwrap()
        .parse()
        .unwrap()
}

async fn pinned_key(ts: &TestServer, id: &str) -> String {
    stored_location(ts, id).await["host_key"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn offered_key(api: &Api, id: &str) -> Value {
    let response = api
        .post(
            &format!("/api/v1/repositories/{id}/host-key/check"),
            &json!({}),
        )
        .await;
    assert_eq!(response.status(), 200);
    body(response).await
}

async fn rotate(sshd: &SshdServer) {
    sshd.rotate_host_key();
    tokio::time::sleep(Duration::from_millis(500)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn host_key_check_offers_the_new_key_after_a_rotation() {
    let sshd = SshdServer::start().await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;
    let id = create(&api, &sftp_draft(&sshd, "sftp-repo")).await;
    let pinned = pinned_key(&ts, &id).await;
    let pinned_fingerprint = SftpStore::host_key_fingerprint(&pinned).unwrap();
    let (mark, before) = (sshd.log().len(), files(&sshd));

    let same = offered_key(&api, &id).await;
    assert_eq!(same["matches"], true, "{same}");
    assert_eq!(same["pinned_fingerprint"], pinned_fingerprint);

    rotate(&sshd).await;
    let offer = offered_key(&api, &id).await;
    assert_eq!(offer["matches"], false, "{offer}");
    assert_eq!(offer["pinned_fingerprint"], pinned_fingerprint);
    assert_eq!(
        offer["fingerprint"],
        SftpStore::host_key_fingerprint(&sshd.host_key()).unwrap()
    );
    assert_ne!(offer["fingerprint"], pinned_fingerprint);
    assert_eq!(pinned_key(&ts, &id).await, pinned);

    tokio::time::sleep(Duration::from_millis(300)).await;
    let log = sshd.log();
    let tail = log.get(mark..).unwrap_or("");
    for forbidden in ["Accepted publickey", "Failed publickey", "subsystem"] {
        assert!(!tail.contains(forbidden), "{forbidden}: {tail}");
    }
    assert_eq!(files(&sshd), before);
    ts.stop().await;
}

async fn next_host_key(stream: &mut tonic::Streaming<ServerMessage>) -> String {
    let message = tokio::time::timeout(Duration::from_secs(10), stream.message())
        .await
        .expect("a host key message")
        .unwrap()
        .unwrap();
    host_key_of(message)
}

async fn silent_of_host_keys(stream: &mut tonic::Streaming<ServerMessage>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while let Ok(Ok(Some(message))) = tokio::time::timeout_at(deadline, stream.message()).await {
        assert!(
            !matches!(message.body, Some(ServerBody::UpdateHostKey(_))),
            "unexpected host key message"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn host_key_replace_pins_the_new_key_and_clears_the_alert() {
    let sshd = SshdServer::start().await;
    let ts = TestServer::start_with(&Backend::Sqlite, |c| c.schedule_tick_secs = 0).await;
    let api = ts.api().await;
    let id = create(&api, &sftp_draft(&sshd, "sftp-repo")).await;
    let current = enroll_agent(&ts, &api, &id, "current").await;
    let old = enroll_agent(&ts, &api, &id, "old").await;
    let mut with = open_session(&ts, &current, &["host_key_v1"]).await;
    assert_eq!(next_host_key(&mut with).await, pinned_key(&ts, &id).await);
    let mut without = open_session(&ts, &old, &[]).await;
    let _offline = enroll_agent(&ts, &api, &id, "offline").await;

    let now = lodestore_server::db::now_ms();
    let stop = tokio_util::sync::CancellationToken::new();
    rotate(&sshd).await;
    for hours in [25, 50, 75] {
        lodestore_server::maintenance::guarantee::tick(ts.state(), now + hours * HOUR, &stop)
            .await
            .unwrap();
        lodestore_server::alerts::tick(ts.state(), now + hours * HOUR)
            .await
            .unwrap();
    }
    assert_eq!(alerts_of(&ts, "host_key_changed", "firing").await.len(), 1);

    let offer = offered_key(&api, &id).await;
    let new_key = offer["key"].as_str().unwrap().to_string();
    let before = generation(&ts, &id).await;
    let response = api
        .put(
            &format!("/api/v1/repositories/{id}/host-key"),
            &json!({ "host_key": new_key }),
        )
        .await;
    assert_eq!(response.status(), 200);
    let replaced = body(response).await;
    assert_eq!(replaced["fingerprint"], offer["fingerprint"]);
    assert_eq!(replaced["online_notified"], 1, "{replaced}");
    assert_eq!(replaced["needs_update"], 1, "{replaced}");
    assert_eq!(replaced["offline"], 1, "{replaced}");

    assert_eq!(next_host_key(&mut with).await, new_key);
    silent_of_host_keys(&mut without).await;
    assert_eq!(generation(&ts, &id).await, before + 1);
    assert_eq!(pinned_key(&ts, &id).await, new_key);
    assert_eq!(row(&ts, &id, "host_key_mismatch_json").await, None);
    assert_eq!(
        row(&ts, &id, "guarantee_unchecked_runs").await.unwrap(),
        "0"
    );
    lodestore_server::alerts::tick(ts.state(), now + 80 * HOUR)
        .await
        .unwrap();
    assert!(
        alerts_of(&ts, "host_key_changed", "firing")
            .await
            .is_empty()
    );
    assert_eq!(
        alerts_of(&ts, "host_key_changed", "resolved").await.len(),
        1
    );

    let response = api
        .post(&format!("/api/v1/repositories/{id}/check"), &json!({}))
        .await;
    assert_eq!(response.status(), 200);
    let rechecked = body(response).await;
    let write = rechecked["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "write")
        .unwrap();
    assert_eq!(write["status"], "pass", "{rechecked}");
    ts.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn host_key_replace_discards_a_result_from_an_older_access_generation() {
    let sshd = SshdServer::start().await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = ts.api().await;
    let id = create(&api, &sftp_draft(&sshd, "sftp-repo")).await;
    let pinned = pinned_key(&ts, &id).await;
    let mismatch = r#"{"offered":"SHA256:b","pinned":"SHA256:a"}"#;
    sqlx::query("UPDATE repositories SET host_key_mismatch_json = $1 WHERE id = $2")
        .bind(mismatch)
        .bind(&id)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
    let g = generation(&ts, &id).await;
    sqlx::query("UPDATE repositories SET access_generation = access_generation + 1 WHERE id = $1")
        .bind(&id)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
    let level = row(&ts, &id, "guarantee_level").await.unwrap();
    let accepted = row(&ts, &id, "guarantee_accepted").await.unwrap();
    let write = GuaranteeWrite {
        level: "unchecked".into(),
        notes_json: "[]".into(),
        accepted: accepted.clone(),
        read_accepted: accepted,
    };

    let stale = replace_host_key(ts.state(), &id, g, OTHER_KEY, "unchecked", "[]", &write)
        .await
        .unwrap();
    assert_eq!(stale, Stored::Stale);
    assert_eq!(pinned_key(&ts, &id).await, pinned);
    assert_eq!(row(&ts, &id, "guarantee_level").await.unwrap(), level);
    assert_eq!(
        row(&ts, &id, "host_key_mismatch_json").await.as_deref(),
        Some(mismatch)
    );

    let written = replace_host_key(ts.state(), &id, g + 1, OTHER_KEY, "unchecked", "[]", &write)
        .await
        .unwrap();
    assert_eq!(written, Stored::Written);
    assert_eq!(pinned_key(&ts, &id).await, OTHER_KEY);
    assert_eq!(row(&ts, &id, "host_key_mismatch_json").await, None);
    assert_eq!(generation(&ts, &id).await, g + 2);
    ts.stop().await;
}

async fn wait_for_waiters(lock: &Arc<tokio::sync::Mutex<()>>, count: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while Arc::strong_count(lock) != count {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the lock has {} holders, expected {count}",
            Arc::strong_count(lock)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

type Put = tokio::task::JoinHandle<(u16, Value)>;

fn put_host_key(api: &Arc<Api>, id: &str, key: &str) -> Put {
    let (api, path, key) = (
        Arc::clone(api),
        format!("/api/v1/repositories/{id}/host-key"),
        key.to_string(),
    );
    tokio::spawn(async move {
        let response = api.put(&path, &json!({ "host_key": key })).await;
        (response.status().as_u16(), body(response).await)
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the storage emulators: scripts/test-emulators.sh"]
async fn host_key_replace_and_session_start_do_not_interleave() {
    let sshd = SshdServer::start().await;
    let ts = TestServer::start(&Backend::Sqlite).await;
    let api = Arc::new(ts.api().await);
    let id = create(&api, &sftp_draft(&sshd, "sftp-repo")).await;
    let paths = enroll_agent(&ts, &api, &id, "sftp-agent").await;
    let original = pinned_key(&ts, &id).await;
    let lock = ts.state().repos.access_lock(&id);

    rotate(&sshd).await;
    let second = offered_key(&api, &id).await["key"]
        .as_str()
        .unwrap()
        .to_string();
    let held = lock.lock().await;
    wait_for_waiters(&lock, 2).await;
    let put = put_host_key(&api, &id, &second);
    wait_for_waiters(&lock, 3).await;
    assert_eq!(pinned_key(&ts, &id).await, original);
    let opening = tokio::spawn(open_session_at(
        ts.grpc_address(),
        paths.clone(),
        capabilities_vec(&["host_key_v1"]),
    ));
    wait_for_waiters(&lock, 4).await;
    assert!(!put.is_finished());
    drop(held);
    let (status, replaced) = put.await.unwrap();
    assert_eq!(status, 200, "{replaced}");
    assert_eq!(replaced["online_notified"], 1, "{replaced}");
    let mut stream = opening.await.unwrap();
    assert_eq!(next_host_key(&mut stream).await, second);
    assert_eq!(next_host_key(&mut stream).await, second);
    silent_of_host_keys(&mut stream).await;
    drop(stream);

    rotate(&sshd).await;
    let third = offered_key(&api, &id).await["key"]
        .as_str()
        .unwrap()
        .to_string();
    let held = lock.lock().await;
    let opening = tokio::spawn(open_session_at(
        ts.grpc_address(),
        paths.clone(),
        capabilities_vec(&["host_key_v1"]),
    ));
    wait_for_waiters(&lock, 3).await;
    let put = put_host_key(&api, &id, &third);
    wait_for_waiters(&lock, 4).await;
    drop(held);
    let mut stream = opening.await.unwrap();
    let (status, replaced) = put.await.unwrap();
    assert_eq!(status, 200, "{replaced}");
    assert_eq!(replaced["online_notified"], 1, "{replaced}");
    assert_eq!(next_host_key(&mut stream).await, second);
    assert_eq!(next_host_key(&mut stream).await, third);
    silent_of_host_keys(&mut stream).await;
    assert_eq!(pinned_key(&ts, &id).await, third);
    ts.stop().await;
}
