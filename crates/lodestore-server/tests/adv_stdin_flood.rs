//! A local user who may talk to the agent's socket floods the host with
//! `backup_stdin` requests that name a policy. The scheduled history of that
//! policy must survive. Only public paths of the first release are used
//! (REST, the agent's socket protocol v1, SQL reads, `schedule::tick`), so
//! the scenario runs unchanged against that release.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use common::{AgentEnv, TestServer, backends, wait_until};
use lodestore_policy::jiff::Timestamp;
use lodestore_server::schedule::tick;
use serde_json::{Value, json};
use sqlx::Row;

const POLICY: &str = "keep-three";
const FLOOD: usize = 30;

fn frame(stream: &mut UnixStream, data: &[u8]) {
    stream
        .write_all(&u32::try_from(data.len()).unwrap().to_be_bytes())
        .unwrap();
    stream.write_all(data).unwrap();
}

fn stdin_backup(socket: &std::path::Path, n: usize) {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let line = json!({"v": 1, "op": "backup_stdin", "name": "flood.dump", "policy": POLICY});
    writeln!(stream, "{line}").unwrap();
    frame(&mut stream, format!("flood number {n}").as_bytes());
    stream.write_all(&0u32.to_be_bytes()).unwrap();
    let answers: Vec<Value> = BufReader::new(stream)
        .lines()
        .map_while(Result::ok)
        .map(|l| serde_json::from_str(&l).unwrap())
        .collect();
    assert_eq!(
        answers.last().unwrap()["type"],
        "backup_summary",
        "{answers:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_stdin_flood_keeps_scheduled_history() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        let mut env = AgentEnv::start_on(&ts, &repo, "main", "host-a").await;
        let data = env.work.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("file"), "content").unwrap();

        let socket_dir = tempfile::Builder::new()
            .prefix("ls")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = socket_dir.path().join("agent.sock");
        let yaml = env.paths.agent_yaml();
        std::fs::write(&yaml, format!("socket: {}\n", socket.display())).unwrap();
        std::fs::set_permissions(&yaml, std::fs::Permissions::from_mode(0o600)).unwrap();
        env.stop_agent().await;
        env.start_agent().await;
        assert!(wait_until(Duration::from_secs(10), || async { socket.exists() }).await);

        let policy = format!(
            "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: {POLICY}\n\
             spec:\n  repository: main\n  selector:\n    matchLabels:\n      agent: host-a\n  \
             paths:\n    - {}\n  schedule:\n    cron: \"*/5 * * * *\"\n    jitter: 0m\n  \
             retention:\n    keepLast: 3\n",
            data.display()
        );
        let response = env
            .api
            .post("/api/v1/policies", &json!({ "yaml": policy }))
            .await;
        assert_eq!(response.status(), 201);

        let state = ts.state();
        let at = |text: &str| -> Timestamp { text.parse().unwrap() };
        tick(state, at("2026-06-01T10:02:00Z")).await.unwrap();
        for n in 1..=3 {
            let now = format!("2026-06-01T10:{:02}:10Z", 5 * n);
            let queued = tick(state, at(&now)).await.unwrap().queued;
            assert_eq!(queued.len(), 1, "slot {n}");
            let job = env.wait_job_settled(&queued[0].job_id).await;
            assert!(
                job["state"] == "succeeded" || job["state"] == "warnings",
                "{job}"
            );
        }

        let socket_path = socket.clone();
        tokio::task::spawn_blocking(move || {
            for n in 0..FLOOD {
                stdin_backup(&socket_path, n);
            }
        })
        .await
        .unwrap();
        let pool = &state.db.pool;
        let settled = wait_until(Duration::from_secs(120), || async {
            let pending: i64 = sqlx::query(
                "SELECT COUNT(*) AS n FROM jobs WHERE stdin_name IS NOT NULL \
                 AND (retention_json IS NULL OR retention_json LIKE '%running%')",
            )
            .fetch_one(pool)
            .await
            .unwrap()
            .try_get("n")
            .unwrap();
            pending == 0
        })
        .await;
        assert!(
            settled,
            "[{}] retention of the flood never settled",
            backend.name()
        );

        let kept: i64 = sqlx::query(
            "SELECT COUNT(*) AS n FROM snapshots WHERE policy_name = $1 AND stdin_name IS NULL",
        )
        .bind(POLICY)
        .fetch_one(pool)
        .await
        .unwrap()
        .try_get("n")
        .unwrap();
        assert_eq!(
            kept,
            3,
            "[{}] scheduled history was evicted",
            backend.name()
        );

        env.stop().await;
        ts.stop().await;
    }
}
