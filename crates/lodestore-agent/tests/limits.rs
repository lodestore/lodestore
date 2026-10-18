mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{TestAgent, local_agent, snapshots};
use lodestore_agent::jobs::JobHub;
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{
    AgentMessage, Hook, IoniceClass, JobAssign, JobFinished, JobOutcome, JobSpec, Limits,
};

const WAIT: Duration = Duration::from_secs(120);
const RATE: u64 = 512 << 10;

fn random_file(path: &Path, len: usize) {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let bytes: Vec<u8> = (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect();
    std::fs::write(path, bytes).unwrap();
}

fn slow_after_hook(agent: &TestAgent) -> Hook {
    let dir = agent.paths.hooks_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.join("linger");
    std::fs::write(&path, "#!/bin/sh\nsleep 2\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    Hook {
        script: "linger".into(),
        ..Default::default()
    }
}

fn finished(msgs: &[AgentMessage]) -> Option<&JobFinished> {
    msgs.iter().find_map(|m| match &m.body {
        Some(AgentBody::JobFinished(f)) => Some(f),
        _ => None,
    })
}

fn upload_speeds(msgs: &[AgentMessage]) -> Vec<u64> {
    msgs.iter()
        .filter_map(|m| match &m.body {
            Some(AgentBody::JobProgress(p)) => Some(p.upload_speed_bps),
            _ => None,
        })
        .collect()
}

async fn run_job(hub: &Arc<JobHub>, spec: JobSpec) -> Vec<AgentMessage> {
    hub.handle(ServerBody::JobAssign(JobAssign {
        job_id: "job-1".into(),
        spec: Some(spec),
        restore: None,
    }));
    let mut all = Vec::new();
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        all.extend(hub.outbound());
        if finished(&all).is_some() {
            return all;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the job did not finish"
        );
        tokio::select! {
            () = hub.changed() => {}
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
}

async fn run_with(limits: Option<Limits>) -> (Duration, Vec<AgentMessage>, usize) {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let hub = JobHub::open(&agent.paths, &agent.agent_id).unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();
    random_file(&dir.join("blob"), 2 << 20);
    let spec = JobSpec {
        policy_name: "capped".into(),
        paths: vec![dir.to_string_lossy().into_owned()],
        after: vec![slow_after_hook(&agent)],
        limits,
        ..Default::default()
    };
    let started = Instant::now();
    let msgs = run_job(&hub, spec).await;
    let took = started.elapsed();
    let snaps = snapshots(&agent).await.len();
    (took, msgs, snaps)
}

#[tokio::test]
async fn limits_upload_rate_caps_speed() {
    let (took, msgs, snaps) = run_with(Some(Limits {
        upload_rate_bps: RATE,
        ..Default::default()
    }))
    .await;
    let done = finished(&msgs).unwrap();
    assert_eq!(done.outcome, JobOutcome::Succeeded as i32, "{done:?}");
    assert_eq!(snaps, 1);
    assert!(took >= Duration::from_secs(3), "{took:?}");
    let speeds = upload_speeds(&msgs);
    let top = speeds.iter().copied().max().unwrap_or(0);
    assert!(top > 0, "{speeds:?}");
    assert!(top <= RATE + RATE / 10, "{speeds:?}");
}

#[tokio::test]
async fn limits_absent_unchanged() {
    let (took, msgs, snaps) = run_with(None).await;
    let done = finished(&msgs).unwrap();
    assert_eq!(done.outcome, JobOutcome::Succeeded as i32, "{done:?}");
    assert_eq!(snaps, 1);
    assert!(took < Duration::from_secs(20), "{took:?}");
    let top = upload_speeds(&msgs).into_iter().max().unwrap_or(0);
    assert!(top > 0, "{top}");
}

fn kinds(msgs: &[AgentMessage]) -> Vec<String> {
    msgs.iter()
        .filter_map(|m| match &m.body {
            Some(AgentBody::JobEvents(e)) => Some(e.events.iter().map(|e| e.kind.clone())),
            _ => None,
        })
        .flatten()
        .collect()
}

fn probe_hook(agent: &TestAgent, out: &Path) -> Hook {
    let dir = agent.paths.hooks_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.join("probe");
    let out = out.display();
    let body = format!(
        "#!/bin/sh\nset -- $(cat /proc/self/stat)\necho \"${{19}}\" > {out}/nice\n\
         if command -v ionice >/dev/null 2>&1; then ionice -p $$ > {out}/ionice; fi\n"
    );
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    Hook {
        script: "probe".into(),
        ..Default::default()
    }
}

async fn run_small(limits: Option<Limits>, probe: Option<&Path>) -> Vec<AgentMessage> {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let hub = JobHub::open(&agent.paths, &agent.agent_id).unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("file"), b"hello").unwrap();
    let spec = JobSpec {
        policy_name: "niced".into(),
        paths: vec![dir.to_string_lossy().into_owned()],
        before: probe.map(|p| probe_hook(&agent, p)).into_iter().collect(),
        limits,
        ..Default::default()
    };
    run_job(&hub, spec).await
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn limits_priority_applied() {
    let out = tempfile::tempdir().unwrap();
    let msgs = run_small(
        Some(Limits {
            nice: 10,
            ionice_class: IoniceClass::Idle as i32,
            ..Default::default()
        }),
        Some(out.path()),
    )
    .await;
    let done = finished(&msgs).unwrap();
    assert_eq!(done.outcome, JobOutcome::Succeeded as i32, "{done:?}");
    assert!(
        !kinds(&msgs).iter().any(|k| k == "priority_failed"),
        "{msgs:?}"
    );
    let nice = std::fs::read_to_string(out.path().join("nice")).unwrap();
    assert_eq!(nice.trim(), "10");
    if let Ok(ionice) = std::fs::read_to_string(out.path().join("ionice")) {
        assert!(ionice.contains("idle"), "{ionice}");
    }
}

#[tokio::test]
async fn limits_priority_default_untouched() {
    let msgs = run_small(
        Some(Limits {
            upload_rate_bps: 0,
            nice: 0,
            ionice_class: IoniceClass::Unspecified as i32,
            ionice_level: 4,
        }),
        None,
    )
    .await;
    let done = finished(&msgs).unwrap();
    assert_eq!(done.outcome, JobOutcome::Succeeded as i32, "{done:?}");
    assert!(
        !kinds(&msgs).iter().any(|k| k == "priority_failed"),
        "{msgs:?}"
    );
}
