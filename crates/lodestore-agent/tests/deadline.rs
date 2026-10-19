mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use common::{TestAgent, local_agent, short_socket_dir, snapshots};
use lodestore_agent::jobs::JobHub;
use lodestore_agent::session::CAPABILITIES;
use lodestore_agent::socket;
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{AgentMessage, Hook, JobAssign, JobFinished, JobOutcome, JobSpec};
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(120);

fn finished(msgs: &[AgentMessage]) -> Option<&JobFinished> {
    msgs.iter().find_map(|m| match &m.body {
        Some(AgentBody::JobFinished(f)) => Some(f),
        _ => None,
    })
}

async fn run_job(hub: &Arc<JobHub>, spec: JobSpec) -> JobFinished {
    hub.handle(ServerBody::JobAssign(JobAssign {
        job_id: "job-1".into(),
        spec: Some(spec),
        restore: None,
    }));
    let mut all = Vec::new();
    let limit = tokio::time::Instant::now() + WAIT;
    loop {
        all.extend(hub.outbound());
        if let Some(f) = finished(&all) {
            return f.clone();
        }
        assert!(
            tokio::time::Instant::now() < limit,
            "the job did not finish"
        );
        tokio::select! {
            () = hub.changed() => {}
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
}

fn snapshot_objects(agent: &TestAgent) -> usize {
    std::fs::read_dir(agent.repo_root.join("snapshots"))
        .map(Iterator::count)
        .unwrap_or(0)
}

fn sleep_hook(agent: &TestAgent, seconds: u32) -> Hook {
    let dir = agent.paths.hooks_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.join("slow");
    std::fs::write(&path, format!("#!/bin/sh\nsleep {seconds}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    Hook {
        script: "slow".into(),
        timeout_s: 30,
        ..Default::default()
    }
}

async fn job_with(max_runtime_s: u64, sleep: u32) -> (TestAgent, JobFinished, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let hub = JobHub::open(&agent.paths, &agent.agent_id).unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("file"), b"hello").unwrap();
    let spec = JobSpec {
        paths: vec![dir.to_string_lossy().into_owned()],
        before: if sleep > 0 {
            vec![sleep_hook(&agent, sleep)]
        } else {
            Vec::new()
        },
        max_runtime_s,
        ..Default::default()
    };
    let done = run_job(&hub, spec).await;
    (agent, done, tmp)
}

#[tokio::test]
async fn deadline_job_without_snapshot() {
    let (agent, done, _tmp) = job_with(1, 2).await;
    assert_eq!(done.outcome, JobOutcome::Failed as i32, "{done:?}");
    assert_eq!(done.error_code, "time_limit");
    assert!(done.snapshot_id.is_empty());
    assert_eq!(snapshot_objects(&agent), 0);
    assert!(snapshots(&agent).await.is_empty());
}

#[tokio::test]
async fn deadline_job_within_limit() {
    let (agent, done, _tmp) = job_with(600, 0).await;
    assert_eq!(done.outcome, JobOutcome::Succeeded as i32, "{done:?}");
    assert_eq!(snapshots(&agent).await.len(), 1);
}

#[test]
fn deadline_hello_announces_gc() {
    assert!(CAPABILITIES.contains(&"gc_v1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn deadline_stdin_job_without_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let hub = JobHub::open(&agent.paths, &agent.agent_id).unwrap();
    hub.set_stdin_max_runtime(Duration::from_secs(1));
    let socket_dir = short_socket_dir();
    let path = socket_dir.path().join("a.sock");
    let shutdown = CancellationToken::new();
    tokio::spawn(socket::serve(
        Arc::clone(&hub),
        path.clone(),
        shutdown.clone(),
    ));
    let mut stream = loop {
        match UnixStream::connect(&path) {
            Ok(stream) => break stream,
            Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(120)))
        .unwrap();
    let reader = BufReader::new(stream.try_clone().unwrap());
    stream
        .write_all(b"{\"v\":1,\"op\":\"backup_stdin\",\"name\":\"app.dump\",\"policy\":null}\n")
        .unwrap();
    stream.write_all(&4u32.to_be_bytes()).unwrap();
    stream.write_all(b"data").unwrap();
    tokio::time::sleep(Duration::from_millis(2200)).await;
    stream.write_all(&0u32.to_be_bytes()).unwrap();

    let mut last = String::new();
    for line in reader.lines().map_while(Result::ok) {
        last = line;
    }
    assert!(last.contains("\"error\""), "{last}");
    let done = loop {
        if let Some(f) = hub.outbound().iter().find_map(|m| match &m.body {
            Some(AgentBody::JobFinished(f)) => Some(f.clone()),
            _ => None,
        }) {
            break f;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    shutdown.cancel();
    assert_eq!(done.outcome, JobOutcome::Failed as i32, "{done:?}");
    assert_eq!(done.error_code, "time_limit");
    assert_eq!(snapshot_objects(&agent), 0);
}
