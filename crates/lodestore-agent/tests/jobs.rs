mod common;

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::{TestAgent, local_agent, snapshots};
use lodestore_agent::facts::hook_facts;
use lodestore_agent::jobs::JobHub;
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{
    AgentMessage, JobAssign, JobCancel, JobEvent, JobEventLevel, JobFinished, JobFinishedAck,
    JobForget, JobOutcome, JobProgress, JobReportState, JobSpec, JobSync,
};

const WAIT: Duration = Duration::from_secs(120);

fn spec(paths: &[&Path]) -> JobSpec {
    JobSpec {
        paths: paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        ..Default::default()
    }
}

fn assign(id: &str, spec: JobSpec) -> ServerBody {
    ServerBody::JobAssign(JobAssign {
        job_id: id.to_string(),
        spec: Some(spec),
    })
}

fn finished(msgs: &[AgentMessage]) -> Option<&JobFinished> {
    msgs.iter().find_map(|m| match &m.body {
        Some(AgentBody::JobFinished(f)) => Some(f),
        _ => None,
    })
}

fn events(msgs: &[AgentMessage]) -> Vec<&JobEvent> {
    msgs.iter()
        .filter_map(|m| match &m.body {
            Some(AgentBody::JobEvents(e)) => Some(e.events.iter()),
            _ => None,
        })
        .flatten()
        .collect()
}

fn progress(msgs: &[AgentMessage]) -> Vec<&JobProgress> {
    msgs.iter()
        .filter_map(|m| match &m.body {
            Some(AgentBody::JobProgress(p)) => Some(p),
            _ => None,
        })
        .collect()
}

async fn collect_until_finished(hub: &JobHub) -> Vec<AgentMessage> {
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

fn data_dir(root: &Path, name: &str, files: usize, size: usize) -> PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..files {
        let mut state =
            0x9E37_79B9_7F4A_7C15_u64 ^ (i as u64 + 1).wrapping_mul(0xA24B_AED4_963E_E407);
        let mut bytes = Vec::with_capacity(size);
        while bytes.len() < size {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            bytes.extend_from_slice(&state.to_le_bytes());
        }
        bytes.truncate(size);
        std::fs::File::create(dir.join(format!("file-{i}")))
            .unwrap()
            .write_all(&bytes)
            .unwrap();
    }
    dir
}

fn make_fifos(dir: &Path, count: usize) {
    std::fs::create_dir_all(dir).unwrap();
    let names: Vec<PathBuf> = (0..count).map(|i| dir.join(format!("f{i}"))).collect();
    for chunk in names.chunks(1000) {
        let status = std::process::Command::new("mkfifo")
            .args(chunk)
            .status()
            .unwrap();
        assert!(status.success());
    }
}

fn open_hub(agent: &TestAgent) -> Arc<JobHub> {
    JobHub::open(&agent.paths, &agent.agent_id).unwrap()
}

fn repo_files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.insert(path.clone(), std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, &mut out);
    out
}

#[tokio::test]
async fn agent_job_backs_up_to_storage() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let src = data_dir(tmp.path(), "src", 10, 4096);
    let hub = open_hub(&agent);

    hub.handle(assign("job-1", spec(&[&src])));
    let msgs = collect_until_finished(&hub).await;

    let accepted = msgs
        .iter()
        .position(|m| matches!(m.body, Some(AgentBody::JobAccepted(_))))
        .expect("JobAccepted");
    let done = msgs
        .iter()
        .position(|m| matches!(m.body, Some(AgentBody::JobFinished(_))))
        .unwrap();
    assert!(accepted < done);

    let f = finished(&msgs).unwrap();
    assert_eq!(f.job_id, "job-1");
    assert_eq!(f.outcome, JobOutcome::Succeeded as i32, "{f:?}");
    assert_eq!(f.stats.as_ref().unwrap().files_new, 10);

    let list = snapshots(&agent).await;
    assert_eq!(list.len(), 1);
    assert_eq!(lodestore_format::index::hex32(&list[0].id), f.snapshot_id);
    assert_eq!(list[0].snapshot.agent_id, agent.agent_id.as_bytes());
    assert_eq!(list[0].snapshot.stats.unwrap().files_new, 10);
}

#[tokio::test]
async fn agent_job_only_adds_objects() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let src = data_dir(tmp.path(), "src", 4, 100_000);
    let hub = open_hub(&agent);

    hub.handle(assign("job-1", spec(&[&src])));
    collect_until_finished(&hub).await;
    let before = repo_files(&agent.repo_root);

    std::fs::write(src.join("file-extra"), b"more data").unwrap();
    hub.handle(assign("job-2", spec(&[&src])));
    let msgs = collect_until_finished(&hub).await;
    assert_eq!(
        finished(&msgs).unwrap().outcome,
        JobOutcome::Succeeded as i32
    );

    let after = repo_files(&agent.repo_root);
    for (path, bytes) in &before {
        assert_eq!(after.get(path), Some(bytes), "{} changed", path.display());
    }
    assert!(after.len() > before.len());
}

#[tokio::test]
async fn agent_job_reports_progress_and_eta() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let src = data_dir(tmp.path(), "src", 4, 2 << 20);
    let hub =
        JobHub::open_with_tick(&agent.paths, &agent.agent_id, Duration::from_millis(30)).unwrap();

    hub.handle(assign("job-1", spec(&[&src])));
    let msgs = collect_until_finished(&hub).await;
    assert_eq!(
        finished(&msgs).unwrap().outcome,
        JobOutcome::Succeeded as i32
    );

    let reports = progress(&msgs);
    assert!(reports.len() >= 2, "got {} progress reports", reports.len());
    assert!(
        reports
            .windows(2)
            .all(|w| w[0].bytes_done <= w[1].bytes_done)
    );
    assert!(reports.last().unwrap().bytes_done > reports[0].bytes_done);
    assert!(
        reports.iter().all(|p| p.eta_s.is_none() || p.scan_done),
        "an ETA before the scan finished"
    );
    assert!(reports.iter().any(|p| p.speed_bps > 0));
    assert!(
        reports.iter().any(|p| p.scan_done && p.eta_s.is_some()),
        "no ETA after the scan"
    );
}

#[tokio::test]
async fn agent_job_streams_warnings() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let src = data_dir(tmp.path(), "src", 2, 1000);
    make_fifos(&src.join("pipes"), 1);
    let locked = src.join("locked");
    std::fs::write(&locked, b"secret").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let as_root = rustix::process::getuid().is_root();
    let hub = open_hub(&agent);

    hub.handle(assign("job-1", spec(&[&src])));
    let msgs = collect_until_finished(&hub).await;

    let f = finished(&msgs).unwrap();
    assert_eq!(f.outcome, JobOutcome::Warnings as i32, "{f:?}");
    let all = events(&msgs);
    assert!(all.windows(2).all(|w| w[0].seq < w[1].seq));
    let pipe = all
        .iter()
        .find(|e| e.kind == "special_file")
        .expect("special_file event");
    assert!(pipe.path.ends_with("pipes/f0"));
    assert_eq!(pipe.level, JobEventLevel::Warning as i32);
    if !as_root {
        let unreadable = all
            .iter()
            .find(|e| e.kind == "unreadable")
            .expect("unreadable event");
        assert!(unreadable.path.ends_with("locked"));
        assert!(f.incomplete);
        assert!(f.errors_total > 0);
    }
    assert_eq!(f.last_event_seq, all.last().unwrap().seq);
}

#[tokio::test]
async fn agent_job_event_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let src = tmp.path().join("src");
    make_fifos(&src, 6000);
    let hub = open_hub(&agent);

    hub.handle(assign("job-1", spec(&[&src])));
    let msgs = collect_until_finished(&hub).await;

    let f = finished(&msgs).unwrap();
    assert_eq!(f.outcome, JobOutcome::Warnings as i32, "{f:?}");
    let all = events(&msgs);
    assert_eq!(all.len(), 5000);
    assert_eq!(all.last().unwrap().kind, "log_limit");
    assert_eq!(all.last().unwrap().seq, 5000);
    assert_eq!(f.last_event_seq, 5000);
}

#[tokio::test]
async fn agent_job_cancel_writes_no_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let src = data_dir(tmp.path(), "src", 4, 8 << 20);
    let hub = open_hub(&agent);

    hub.handle(assign("job-1", spec(&[&src])));
    hub.handle(ServerBody::JobCancel(JobCancel {
        job_id: "job-1".into(),
    }));
    let msgs = collect_until_finished(&hub).await;

    assert_eq!(
        finished(&msgs).unwrap().outcome,
        JobOutcome::Cancelled as i32
    );
    assert!(snapshots(&agent).await.is_empty());
}

#[tokio::test]
async fn agent_job_shutdown_is_interrupted() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let src = data_dir(tmp.path(), "src", 4, 8 << 20);
    let hub = open_hub(&agent);

    hub.handle(assign("job-1", spec(&[&src])));
    hub.shutdown();
    hub.wait_idle(WAIT).await;
    let msgs = collect_until_finished(&hub).await;

    assert_eq!(
        finished(&msgs).unwrap().outcome,
        JobOutcome::Interrupted as i32
    );
    assert!(snapshots(&agent).await.is_empty());
}

#[tokio::test]
async fn agent_job_busy_rejects_second() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let src = data_dir(tmp.path(), "src", 4, 8 << 20);
    let hub = open_hub(&agent);

    hub.handle(assign("job-1", spec(&[&src])));
    hub.handle(assign("job-2", spec(&[&src])));
    hub.handle(ServerBody::JobCancel(JobCancel {
        job_id: "job-1".into(),
    }));
    let msgs = collect_until_finished(&hub).await;

    let rejected: Vec<_> = msgs
        .iter()
        .filter_map(|m| match &m.body {
            Some(AgentBody::JobRejected(r)) => Some(r),
            _ => None,
        })
        .collect();
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0].job_id, "job-2");
    assert_eq!(rejected[0].code, "busy");
    assert_eq!(hub.reports().len(), 1);
}

#[tokio::test]
async fn agent_job_finished_resent_until_acked() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let src = data_dir(tmp.path(), "src", 2, 1000);
    let hub = open_hub(&agent);

    hub.handle(assign("job-1", spec(&[&src])));
    let msgs = collect_until_finished(&hub).await;
    let first = finished(&msgs).unwrap().clone();

    hub.session_reset();
    let again = hub.outbound();
    assert_eq!(finished(&again), Some(&first));
    assert_eq!(hub.reports()[0].state, JobReportState::Finished as i32);

    hub.handle(ServerBody::JobFinishedAck(JobFinishedAck {
        job_id: "job-1".into(),
    }));
    hub.session_reset();
    assert!(finished(&hub.outbound()).is_none());
    assert!(hub.reports().is_empty());
    assert_eq!(
        std::fs::read_dir(agent.paths.jobs_dir()).unwrap().count(),
        0
    );
}

#[tokio::test]
async fn agent_session_reset_returns_the_reports_it_resets_against() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let src = data_dir(tmp.path(), "src", 2, 1000);
    let hub = open_hub(&agent);

    hub.handle(assign("job-1", spec(&[&src])));
    let reports = hub.session_reset();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].job_id, "job-1");
    assert_eq!(reports, hub.reports());
    assert!(
        hub.outbound()
            .iter()
            .all(|m| !matches!(m.body, Some(AgentBody::JobAccepted(_))))
    );
}

#[tokio::test]
async fn agent_job_store_survives_restart_as_interrupted() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let jobs = agent.paths.jobs_dir();
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(
        jobs.join("job-9.json"),
        br#"{"job_id":"job-9","kind":"file","state":"running","last_event_seq":0}"#,
    )
    .unwrap();
    std::fs::write(
        jobs.join("job-9.events"),
        concat!(
            r#"{"seq":1,"at_ms":1,"level":1,"kind":"started","path":"","script":"","message":"m"}"#,
            "\n",
            r#"{"seq":2,"at_ms":2,"level":2,"kind":"unreadable","path":"/x","script":"","message":"m"}"#,
            "\n"
        ),
    )
    .unwrap();

    let hub = open_hub(&agent);
    let reports = hub.reports();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].state, JobReportState::Finished as i32);
    assert_eq!(reports[0].last_event_seq, 2);
    let f = reports[0].finished.as_ref().unwrap();
    assert_eq!(f.outcome, JobOutcome::Interrupted as i32);
    assert_eq!(f.last_event_seq, 2);

    let reopened = open_hub(&agent);
    assert_eq!(
        reopened.reports()[0].finished.as_ref().unwrap().outcome,
        JobOutcome::Interrupted as i32
    );
}

#[tokio::test]
async fn agent_job_sync_resends_events() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let src = data_dir(tmp.path(), "src", 1, 100);
    make_fifos(&src.join("pipes"), 5);
    let hub = open_hub(&agent);

    hub.handle(assign("job-1", spec(&[&src])));
    let msgs = collect_until_finished(&hub).await;
    assert!(events(&msgs).len() >= 6);

    hub.handle(ServerBody::JobSync(JobSync {
        job_id: "job-1".into(),
        resend_events_from: 3,
    }));
    let resent = hub.outbound();
    let seqs: Vec<u64> = events(&resent).iter().map(|e| e.seq).collect();
    assert_eq!(seqs.first(), Some(&3));
    assert_eq!(seqs.last(), events(&msgs).last().map(|e| &e.seq));
    assert!(seqs.windows(2).all(|w| w[0] + 1 == w[1]));
}

#[tokio::test]
async fn agent_job_forget_drops_files() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let src = data_dir(tmp.path(), "src", 1, 100);
    let hub = open_hub(&agent);

    hub.handle(assign("job-1", spec(&[&src])));
    collect_until_finished(&hub).await;
    assert!(agent.paths.jobs_dir().join("job-1.json").exists());

    hub.handle(ServerBody::JobForget(JobForget {
        job_id: "job-1".into(),
    }));
    assert!(hub.reports().is_empty());
    assert_eq!(
        std::fs::read_dir(agent.paths.jobs_dir()).unwrap().count(),
        0
    );
}

#[tokio::test]
async fn agent_hook_facts_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let hooks = agent.paths.hooks_dir();
    std::fs::create_dir_all(&hooks).unwrap();
    std::fs::set_permissions(&hooks, std::fs::Permissions::from_mode(0o700)).unwrap();
    let good = hooks.join("dump-db");
    std::fs::write(&good, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o700)).unwrap();
    let loose = hooks.join("loose");
    std::fs::write(&loose, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o777)).unwrap();
    std::fs::create_dir_all(&agent.paths.config_dir).unwrap();
    let yaml = agent.paths.agent_yaml();
    std::fs::write(&yaml, "hooks:\n  allow_inline: true\n").unwrap();
    std::fs::set_permissions(&yaml, std::fs::Permissions::from_mode(0o600)).unwrap();

    let facts = hook_facts(&agent.paths);
    assert!(facts.allow_inline);
    let scripts: Vec<(&str, bool)> = facts
        .scripts
        .iter()
        .map(|s| (s.name.as_str(), s.ok))
        .collect();
    assert_eq!(scripts, [("dump-db", true), ("loose", false)]);
}
