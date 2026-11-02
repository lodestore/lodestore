mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::{TestAgent, local_agent, short_socket_dir, snapshots};
use lodestore_agent::identity::{AgentConfig, Identity, Trust, TrustMode};
use lodestore_agent::jobs::JobHub;
use lodestore_agent::socket;
use lodestore_agent::{NetOptions, RunArgs};
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{
    AgentMessage, JobAssign, JobCancel, JobCreated, JobFinished, JobOutcome, JobSpec,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

struct Client {
    write: UnixStream,
    read: BufReader<UnixStream>,
}

impl Client {
    fn connect(path: &Path) -> Client {
        let stream = UnixStream::connect(path).unwrap();
        // macOS refuses this once the agent has already replied and closed.
        let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
        Client {
            read: BufReader::new(stream.try_clone().unwrap()),
            write: stream,
        }
    }

    fn line(&mut self, line: &str) {
        self.write.write_all(line.as_bytes()).unwrap();
        self.write.write_all(b"\n").unwrap();
    }

    fn header(&mut self, header: &Value) {
        self.line(&header.to_string());
    }

    fn frame(&mut self, data: &[u8]) {
        self.write
            .write_all(&u32::try_from(data.len()).unwrap().to_be_bytes())
            .unwrap();
        self.write.write_all(data).unwrap();
    }

    fn commit(&mut self) {
        self.write.write_all(&0u32.to_be_bytes()).unwrap();
    }

    fn next(&mut self) -> Value {
        let mut line = String::new();
        self.read.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn rest(&mut self) -> Vec<Value> {
        (&mut self.read)
            .lines()
            .map_while(Result::ok)
            .map(|l| serde_json::from_str(&l).unwrap())
            .collect()
    }
}

fn header(name: &str, policy: Option<&str>) -> Value {
    json!({"v": 1, "op": "backup_stdin", "name": name, "policy": policy})
}

fn kinds(replies: &[Value]) -> Vec<&str> {
    replies
        .iter()
        .map(|r| r["type"].as_str().unwrap())
        .collect()
}

struct Running {
    agent: TestAgent,
    hub: Arc<JobHub>,
    socket: PathBuf,
    shutdown: CancellationToken,
    _tmp: tempfile::TempDir,
    _socket_dir: tempfile::TempDir,
}

async fn wait_listening(socket: &Path) {
    for _ in 0..200 {
        if let Ok(stream) = UnixStream::connect(socket) {
            let mut client = Client {
                read: BufReader::new(stream.try_clone().unwrap()),
                write: stream,
            };
            client.header(&json!({"v": 1, "op": "ping", "name": "x"}));
            if client.rest().iter().any(|r| r["code"] == "unsupported") {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the socket never started listening");
}

async fn start() -> Running {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let hub = JobHub::open(&agent.paths, &agent.agent_id).unwrap();
    let socket_dir = short_socket_dir();
    let socket = socket_dir.path().join("a.sock");
    let shutdown = CancellationToken::new();
    tokio::spawn(socket::serve(
        Arc::clone(&hub),
        socket.clone(),
        shutdown.clone(),
    ));
    wait_listening(&socket).await;
    Running {
        agent,
        hub,
        socket,
        shutdown,
        _tmp: tmp,
        _socket_dir: socket_dir,
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn created(msgs: &[AgentMessage]) -> Vec<&JobCreated> {
    msgs.iter()
        .filter_map(|m| match &m.body {
            Some(AgentBody::JobCreated(c)) => Some(c),
            _ => None,
        })
        .collect()
}

fn finished(msgs: &[AgentMessage]) -> Vec<&JobFinished> {
    msgs.iter()
        .filter_map(|m| match &m.body {
            Some(AgentBody::JobFinished(f)) => Some(f),
            _ => None,
        })
        .collect()
}

fn snapshot_objects(agent: &TestAgent) -> usize {
    std::fs::read_dir(agent.repo_root.join("snapshots"))
        .map(Iterator::count)
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_without_commit_writes_no_snapshot() {
    let run = start().await;
    let mut client = Client::connect(&run.socket);
    client.header(&header("app.dump", None));
    client.frame(b"half of a dump");
    client.write.shutdown(Shutdown::Write).unwrap();

    let replies = client.rest();
    let last = replies.last().unwrap();
    assert_eq!(last["type"], "error");
    assert_eq!(last["code"], "stream_incomplete");
    assert!(!kinds(&replies).contains(&"backup_summary"));
    let msgs = run.hub.outbound();
    let done = finished(&msgs);
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].outcome, JobOutcome::Failed as i32);
    assert_eq!(done[0].error_code, "stream_incomplete");
    assert_eq!(snapshot_objects(&run.agent), 0);
    assert!(snapshots(&run.agent).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_rejects_unknown_op_and_bad_header() {
    let run = start().await;
    let long = "x".repeat(5000);
    let cases = [
        (
            json!({"v": 1, "op": "restore", "name": "a"}).to_string(),
            "unsupported",
        ),
        (
            json!({"v": 2, "op": "backup_stdin", "name": "a"}).to_string(),
            "unsupported",
        ),
        (header("a/b", None).to_string(), "bad_request"),
        (header("..", None).to_string(), "bad_request"),
        (
            json!({"v": 1, "op": "backup_stdin", "name": "a", "extra": 1}).to_string(),
            "bad_request",
        ),
        (header("a", Some("")).to_string(), "bad_request"),
        (header(&long, None).to_string(), "bad_request"),
        ("not json".to_string(), "bad_request"),
    ];
    for (line, code) in cases {
        let mut client = Client::connect(&run.socket);
        client.line(&line);
        let replies = client.rest();
        assert_eq!(replies.len(), 1, "{line}");
        assert_eq!(replies[0]["type"], "error", "{line}");
        assert_eq!(replies[0]["code"], code, "{line}");
    }
    assert!(run.hub.reports().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_empty_stream_is_empty_file() {
    let run = start().await;
    let mut client = Client::connect(&run.socket);
    client.header(&header("app.dump", None));
    client.commit();

    let replies = client.rest();
    assert_eq!(kinds(&replies)[0], "accepted");
    let summary = replies.last().unwrap();
    assert_eq!(summary["type"], "backup_summary");
    let stats = &summary["stats"];
    assert_eq!(stats["files_new"], 1);
    assert_eq!(stats["bytes_read"], 0);
    let entries = snapshots(&run.agent).await;
    assert_eq!(entries.len(), 1);
    assert!(
        entries[0]
            .snapshot
            .tags
            .contains(&"stdin=app.dump".to_string())
    );
    let msgs = run.hub.outbound();
    assert_eq!(finished(&msgs)[0].outcome, JobOutcome::Succeeded as i32);
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_file_mode_0660() {
    let run = start().await;
    let mode = std::fs::metadata(&run.socket).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o660);
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_leaves_no_staging_directory_behind() {
    let run = start().await;
    let names: Vec<_> = std::fs::read_dir(run.socket.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("a.sock")]);
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_caps_connections_waiting_for_a_header() {
    let run = start().await;
    let idle: Vec<_> = (0..16)
        .map(|_| UnixStream::connect(&run.socket).unwrap())
        .collect();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut late = Client::connect(&run.socket);
    let reply = late.next();
    assert_eq!(reply["type"], "error");
    assert_eq!(reply["code"], "busy");
    drop(idle);
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_records_caller_and_policy_tag() {
    let run = start().await;
    let mut client = Client::connect(&run.socket);
    client.header(&header("app.dump", Some("db")));
    client.frame(&[7u8; 100_000]);
    client.commit();

    let replies = client.rest();
    assert_eq!(kinds(&replies).last(), Some(&"backup_summary"));
    assert_eq!(replies.last().unwrap()["stats"]["bytes_read"], 100_000);
    let msgs = run.hub.outbound();
    let jobs = created(&msgs);
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].caller_uid, rustix::process::getuid().as_raw());
    assert_eq!(jobs[0].caller_gid, rustix::process::getgid().as_raw());
    let spec = jobs[0].spec.as_ref().unwrap();
    assert_eq!(spec.stdin_name, "app.dump");
    assert_eq!(spec.policy_name, "db");
    assert_eq!(spec.tags, ["policy=db"]);
    assert!(spec.gc_locks_required);
    let reports = run.hub.reports();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].created.as_ref(), Some(jobs[0]));
    let entries = snapshots(&run.agent).await;
    assert!(entries[0].snapshot.tags.contains(&"policy=db".to_string()));
    assert!(
        entries[0]
            .snapshot
            .tags
            .contains(&"stdin=app.dump".to_string())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_limits_parallel_streams() {
    let run = start().await;
    let mut open: Vec<Client> = (0..2)
        .map(|_| {
            let mut client = Client::connect(&run.socket);
            client.header(&header("app.dump", None));
            client.frame(b"data");
            assert_eq!(client.next()["type"], "accepted");
            client
        })
        .collect();

    let mut third = Client::connect(&run.socket);
    third.header(&header("app.dump", None));
    let replies = third.rest();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["code"], "busy");

    for client in &mut open {
        client.commit();
        assert_eq!(kinds(&client.rest()).last(), Some(&"backup_summary"));
    }
    let mut again = Client::connect(&run.socket);
    again.header(&header("app.dump", None));
    again.commit();
    assert_eq!(kinds(&again.rest()).last(), Some(&"backup_summary"));
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_runs_alongside_file_job() {
    let run = start().await;
    let src = run._tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    for i in 0..2u64 {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64 ^ (i + 1).wrapping_mul(0xA24B_AED4_963E_E407);
        let mut bytes = Vec::with_capacity(8 << 20);
        while bytes.len() < 8 << 20 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            bytes.extend_from_slice(&state.to_le_bytes());
        }
        std::fs::write(src.join(format!("file-{i}")), bytes).unwrap();
    }
    run.hub.handle(ServerBody::JobAssign(JobAssign {
        job_id: "job-1".into(),
        spec: Some(JobSpec {
            paths: vec![src.to_string_lossy().into_owned()],
            ..Default::default()
        }),
        restore: None,
    }));
    assert!(
        run.hub
            .reports()
            .iter()
            .any(|r| r.job_id == "job-1" && r.finished.is_none())
    );

    let mut client = Client::connect(&run.socket);
    client.header(&header("app.dump", None));
    client.frame(b"dump");
    client.commit();
    assert_eq!(kinds(&client.rest()).last(), Some(&"backup_summary"));

    run.hub.handle(ServerBody::JobCancel(JobCancel {
        job_id: "job-1".into(),
    }));
    run.hub.wait_idle(Duration::from_secs(120)).await;
    let entries = snapshots(&run.agent).await;
    assert_eq!(entries.len(), 1);
    assert!(
        entries[0]
            .snapshot
            .tags
            .contains(&"stdin=app.dump".to_string())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_path_from_agent_yaml() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = lodestore_agent::Paths {
        config_dir: tmp.path().join("etc"),
        state_dir: tmp.path().join("var"),
    };
    AgentConfig {
        grpc_address: "127.0.0.1:1".into(),
        server_name: "localhost".into(),
        trust: Trust {
            mode: TrustMode::System,
            ca_sha256: None,
            ca_bundle: None,
        },
    }
    .save(&paths)
    .unwrap();
    Identity {
        agent_id: "agent-test".into(),
        repository_id: "test".into(),
        issued_at: 0,
        expires_at: i64::MAX / 2,
    }
    .save(&paths)
    .unwrap();
    let socket_dir = short_socket_dir();
    let socket = socket_dir.path().join("custom.sock");
    let yaml = paths.agent_yaml();
    std::fs::write(&yaml, format!("socket: {}\n", socket.display())).unwrap();
    std::fs::set_permissions(&yaml, std::fs::Permissions::from_mode(0o600)).unwrap();

    let shutdown = CancellationToken::new();
    let agent = tokio::spawn(lodestore_agent::run(RunArgs {
        paths: paths.clone(),
        net: NetOptions::default(),
        heartbeat: Duration::from_secs(15),
        shutdown: shutdown.clone(),
    }));
    wait_listening(&socket).await;
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(30), agent)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    std::fs::write(&yaml, "socket: /nonexistent-dir/agent.sock\n").unwrap();
    let shutdown = CancellationToken::new();
    let agent = tokio::spawn(lodestore_agent::run(RunArgs {
        paths,
        net: NetOptions::default(),
        heartbeat: Duration::from_secs(15),
        shutdown: shutdown.clone(),
    }));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!agent.is_finished());
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(30), agent)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
