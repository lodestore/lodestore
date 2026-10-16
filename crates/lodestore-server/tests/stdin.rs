mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use common::{Api, TestServer, backends, wait_until};
use lodestore_agent::{EnrollArgs, NetOptions, RunArgs};
use lodestore_format::index::hex32;
use lodestore_format::repo::Repository;
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, Writer};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const PASSWORD: &[u8] = b"test-password";

async fn enroll(ts: &TestServer, api: &Api, repo: &str, hostname: &str) -> String {
    let token = api.join_token(repo, json!([])).await;
    let enrolled = lodestore_agent::enroll(EnrollArgs {
        token,
        paths: ts.agent_paths(hostname),
        hostname: Some(hostname.to_string()),
        net: NetOptions::default(),
    })
    .await
    .expect("enroll");
    enrolled.agent_id
}

async fn host_snapshots(api: &Api, host: &str) -> Vec<Value> {
    let response = api.get(&format!("/api/v1/hosts/{host}/snapshots")).await;
    assert_eq!(response.status(), 200);
    let page: Value = response.json().await.unwrap();
    page["items"].as_array().unwrap().clone()
}

fn frame(stream: &mut UnixStream, data: &[u8]) {
    stream
        .write_all(&u32::try_from(data.len()).unwrap().to_be_bytes())
        .unwrap();
    stream.write_all(data).unwrap();
}

fn replies(stream: UnixStream) -> Vec<Value> {
    BufReader::new(stream)
        .lines()
        .map_while(Result::ok)
        .map(|l| serde_json::from_str(&l).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stdin_via_agent_visible_on_server() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let paths = ts.agent_paths("host-a");
        let socket_dir = tempfile::Builder::new()
            .prefix("ls")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = socket_dir.path().join("agent.sock");
        let yaml = paths.agent_yaml();
        std::fs::write(&yaml, format!("socket: {}\n", socket.display())).unwrap();
        std::fs::set_permissions(&yaml, std::fs::Permissions::from_mode(0o600)).unwrap();

        let shutdown = CancellationToken::new();
        let agent = tokio::spawn(lodestore_agent::run(RunArgs {
            paths,
            net: NetOptions::default(),
            heartbeat: Duration::from_secs(1),
            shutdown: shutdown.clone(),
        }));
        let online = wait_until(Duration::from_secs(10), || async {
            api.hosts().await.iter().any(|h| h["status"] == "online")
        })
        .await;
        assert!(online, "[{}] the host never became online", backend.name());
        assert!(wait_until(Duration::from_secs(10), || async { socket.exists() }).await);

        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let line = json!({"v": 1, "op": "backup_stdin", "name": "app.dump", "policy": null});
        writeln!(stream, "{line}").unwrap();
        frame(&mut stream, b"first part of a dump, ");
        frame(&mut stream, b"second part");
        stream.write_all(&0u32.to_be_bytes()).unwrap();
        let answers = replies(stream);
        let summary = answers.last().unwrap();
        assert_eq!(summary["type"], "backup_summary", "{answers:?}");
        let snapshot_id = summary["snapshot_id"].as_str().unwrap().to_string();
        assert_eq!(summary["stats"]["bytes_read"], 33);

        let done = wait_until(Duration::from_secs(15), || async {
            let page: Value = api
                .get("/api/v1/jobs?source=cli")
                .await
                .json()
                .await
                .unwrap();
            page["items"]
                .as_array()
                .is_some_and(|items| items.iter().any(|j| j["state"] == "succeeded"))
        })
        .await;
        assert!(done, "[{}] the stream job never succeeded", backend.name());
        let page: Value = api
            .get("/api/v1/jobs?source=cli")
            .await
            .json()
            .await
            .unwrap();
        let job_id = page["items"][0]["id"].as_str().unwrap().to_string();
        let detail: Value = api
            .get(&format!("/api/v1/jobs/{job_id}"))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(detail["stdin_name"], "app.dump");
        assert_eq!(detail["source"], "cli");
        assert_eq!(detail["host_id"], host);
        assert_eq!(
            detail["caller_uid"],
            ts.dir.path().metadata().unwrap().uid()
        );
        assert_eq!(detail["snapshot_id"], snapshot_id);

        let listed = wait_until(Duration::from_secs(15), || async {
            !host_snapshots(&api, &host).await.is_empty()
        })
        .await;
        assert!(listed, "[{}] the snapshot never showed up", backend.name());
        let items = host_snapshots(&api, &host).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], snapshot_id);
        assert_eq!(items[0]["stdin_name"], "app.dump");
        assert_eq!(items[0]["job_id"], job_id);

        shutdown.cancel();
        agent.await.unwrap().expect("agent stops cleanly");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stdin_direct_visible_after_sync() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;

        let store = LocalFsStore::new(ts.local_repo_root("main"));
        let repository = Repository::open(&store, PASSWORD).await.unwrap();
        let keys = repository.read_keys().write_keys(repository.config_hash());
        let mut writer = Writer::new(&store, keys, repository.config())
            .await
            .unwrap();
        let outcome = writer
            .backup_stream(
                b"app.dump",
                &mut &b"a dump written without the agent"[..],
                BackupOptions {
                    hostname: Some(b"host-a".to_vec()),
                    ..BackupOptions::default()
                },
                &|_| {},
            )
            .await
            .expect("backup_stream");
        let id = hex32(&outcome.snapshot_id);

        assert!(host_snapshots(&api, &host).await.is_empty());
        let response = api
            .post(
                &format!("/api/v1/repositories/{repo}/catalog/sync"),
                &json!({}),
            )
            .await;
        assert_eq!(response.status(), 200);
        let items = host_snapshots(&api, &host).await;
        assert_eq!(items.len(), 1, "[{}]", backend.name());
        assert_eq!(items[0]["id"], id);
        assert_eq!(items[0]["stdin_name"], "app.dump");
        ts.stop().await;
    }
}
