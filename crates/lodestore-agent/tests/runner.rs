mod common;

use std::fs::File;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use common::{TestAgent, local_agent, snapshots};
use lodestore_agent::jobs::JobHub;
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{
    AgentMessage, Hook, JobAssign, JobEvent, JobEventLevel, JobFinished, JobOutcome, JobSpec,
};

const WAIT: Duration = Duration::from_secs(120);

fn script(name: &str) -> Hook {
    Hook {
        script: name.into(),
        ..Default::default()
    }
}

fn inline(command: &str) -> Hook {
    Hook {
        command: command.into(),
        ..Default::default()
    }
}

fn spec(policy: &str, path: &Path) -> JobSpec {
    JobSpec {
        policy_name: policy.into(),
        paths: vec![path.to_string_lossy().into_owned()],
        ..Default::default()
    }
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

async fn run_job(hub: &std::sync::Arc<JobHub>, id: &str, spec: JobSpec) -> Vec<AgentMessage> {
    hub.handle(ServerBody::JobAssign(JobAssign {
        job_id: id.into(),
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

fn open_hub(agent: &TestAgent) -> std::sync::Arc<JobHub> {
    JobHub::open(&agent.paths, &agent.agent_id).unwrap()
}

fn data_dir(root: &Path, name: &str) -> PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let old = SystemTime::now() - Duration::from_secs(3600);
    for i in 0..10 {
        let path = dir.join(format!("file-{i}"));
        std::fs::write(&path, format!("file {i} {}", "x".repeat(i * 1000))).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }
    dir
}

fn install_hook(agent: &TestAgent, name: &str, body: &str) {
    let dir = agent.paths.hooks_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn allow_inline(agent: &TestAgent) {
    std::fs::create_dir_all(&agent.paths.config_dir).unwrap();
    let yaml = agent.paths.agent_yaml();
    std::fs::write(&yaml, "hooks:\n  allow_inline: true\n").unwrap();
    std::fs::set_permissions(&yaml, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn outcome(msgs: &[AgentMessage]) -> JobOutcome {
    JobOutcome::try_from(finished(msgs).unwrap().outcome).unwrap()
}

#[tokio::test]
async fn runner_pre_hook_runs_before_path_check() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let hub = open_hub(&agent);
    let target = tmp.path().join("made-by-hook");
    install_hook(
        &agent,
        "make-dir",
        &format!("mkdir -p {0} && echo data > {0}/file", target.display()),
    );

    let mut job = spec("db", &target);
    job.before = vec![script("make-dir")];
    let msgs = run_job(&hub, "job-1", job).await;
    assert_eq!(outcome(&msgs), JobOutcome::Succeeded, "{msgs:?}");
    assert_eq!(snapshots(&agent).await.len(), 1);
}

#[tokio::test]
async fn runner_missing_path_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let hub = open_hub(&agent);
    let status = tmp.path().join("status");
    install_hook(
        &agent,
        "record",
        &format!("echo \"$LODESTORE_BACKUP_STATUS\" > {}", status.display()),
    );

    let missing = tmp.path().join("nope");
    let mut job = spec("db", &missing);
    job.after = vec![script("record")];
    let msgs = run_job(&hub, "job-1", job).await;

    let f = finished(&msgs).unwrap();
    assert_eq!(f.outcome, JobOutcome::Failed as i32);
    assert_eq!(f.error_code, "path_missing");
    assert_eq!(
        f.error_detail.get("path").map(String::as_str),
        missing.to_str()
    );
    assert!(f.snapshot_id.is_empty());
    assert!(snapshots(&agent).await.is_empty());
    assert_eq!(std::fs::read_to_string(&status).unwrap().trim(), "failed");
}

#[tokio::test]
async fn runner_failed_pre_hook_skips_backup_runs_post() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let hub = open_hub(&agent);
    let src = data_dir(tmp.path(), "src");
    let second = tmp.path().join("second-ran");
    let status = tmp.path().join("status");
    install_hook(&agent, "boom", "exit 7");
    install_hook(&agent, "second", &format!("touch {}", second.display()));
    install_hook(
        &agent,
        "record",
        &format!("echo \"$LODESTORE_BACKUP_STATUS\" > {}", status.display()),
    );

    let mut job = spec("db", &src);
    job.before = vec![script("boom"), script("second")];
    job.after = vec![script("record")];
    let msgs = run_job(&hub, "job-1", job).await;

    let f = finished(&msgs).unwrap();
    assert_eq!(f.outcome, JobOutcome::Failed as i32);
    assert_eq!(f.error_code, "pre_hook_failed");
    assert_eq!(f.error_detail.get("code").map(String::as_str), Some("7"));
    assert_eq!(
        f.error_detail.get("script").map(String::as_str),
        Some("boom")
    );
    assert!(!second.exists());
    assert!(snapshots(&agent).await.is_empty());
    assert_eq!(std::fs::read_to_string(&status).unwrap().trim(), "failed");
}

#[tokio::test]
async fn runner_post_hook_failure_is_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let hub = open_hub(&agent);
    let src = data_dir(tmp.path(), "src");
    let status = tmp.path().join("status");
    install_hook(&agent, "bad-post", "exit 1");
    install_hook(
        &agent,
        "record",
        &format!("echo \"$LODESTORE_BACKUP_STATUS\" > {}", status.display()),
    );

    let mut job = spec("db", &src);
    job.after = vec![script("bad-post"), script("record")];
    let msgs = run_job(&hub, "job-1", job).await;

    assert_eq!(outcome(&msgs), JobOutcome::Warnings);
    let warning = events(&msgs)
        .into_iter()
        .find(|e| e.kind == "post_hook_failed")
        .expect("post_hook_failed event");
    assert_eq!(warning.level, JobEventLevel::Warning as i32);
    assert_eq!(std::fs::read_to_string(&status).unwrap().trim(), "success");
    assert_eq!(snapshots(&agent).await.len(), 1);
}

#[tokio::test]
async fn runner_inline_rejected_without_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let hub = open_hub(&agent);
    let src = data_dir(tmp.path(), "src");

    let mut job = spec("db", &src);
    job.before = vec![inline("true")];
    let msgs = run_job(&hub, "job-1", job.clone()).await;
    let f = finished(&msgs).unwrap();
    assert_eq!(f.outcome, JobOutcome::Failed as i32);
    assert_eq!(f.error_code, "inline_not_allowed");
    assert!(snapshots(&agent).await.is_empty());

    allow_inline(&agent);
    let msgs = run_job(&hub, "job-2", job).await;
    assert_eq!(outcome(&msgs), JobOutcome::Succeeded, "{msgs:?}");

    let mut job = spec("db", &src);
    job.before = vec![script("not-installed")];
    let msgs = run_job(&hub, "job-3", job).await;
    assert_eq!(finished(&msgs).unwrap().error_code, "hook_missing");
}

#[tokio::test]
async fn runner_hook_output_logged() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let hub = open_hub(&agent);
    let src = data_dir(tmp.path(), "src");
    install_hook(&agent, "chatty", "echo from-stdout; echo from-stderr >&2");

    let mut job = spec("db", &src);
    job.before = vec![script("chatty")];
    let msgs = run_job(&hub, "job-1", job).await;
    assert_eq!(outcome(&msgs), JobOutcome::Succeeded);

    let all = events(&msgs);
    let text: String = all
        .iter()
        .filter(|e| e.kind == "hook_output" && e.script == "chatty")
        .map(|e| e.message.as_str())
        .collect();
    assert!(
        text.contains("from-stdout") && text.contains("from-stderr"),
        "{text}"
    );
    let kinds: Vec<&str> = all.iter().map(|e| e.kind.as_str()).collect();
    let started = kinds.iter().position(|k| *k == "hook_started").unwrap();
    let done = kinds.iter().position(|k| *k == "hook_finished").unwrap();
    let written = kinds.iter().position(|k| *k == "snapshot_written").unwrap();
    assert!(started < done && done < written);
}

#[tokio::test]
async fn runner_cache_second_run_reads_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let hub = open_hub(&agent);
    let src = data_dir(tmp.path(), "src");

    let first = run_job(&hub, "job-1", spec("db", &src)).await;
    let stats = finished(&first).unwrap().stats.as_ref().unwrap();
    assert_eq!(stats.files_new, 10);

    let second = run_job(&hub, "job-2", spec("db", &src)).await;
    let stats = finished(&second).unwrap().stats.as_ref().unwrap();
    assert_eq!(stats.bytes_read, 0);
    assert_eq!(stats.files_unchanged, 10);
    let list = snapshots(&agent).await;
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].snapshot.root_tree, list[1].snapshot.root_tree);
}

#[tokio::test]
async fn runner_cache_loss_full_rehash_same_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let hub = open_hub(&agent);
    let src = data_dir(tmp.path(), "src");

    run_job(&hub, "job-1", spec("db", &src)).await;
    std::fs::remove_dir_all(agent.paths.cache_dir()).unwrap();

    let again = run_job(&hub, "job-2", spec("db", &src)).await;
    let stats = finished(&again).unwrap().stats.as_ref().unwrap();
    assert!(stats.bytes_read > 0);
    assert_eq!(stats.files_unchanged, 0);
    assert!(stats.bytes_added < 64 * 1024, "added {}", stats.bytes_added);
    let list = snapshots(&agent).await;
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].snapshot.root_tree, list[1].snapshot.root_tree);
}

#[tokio::test]
async fn runner_cache_per_policy() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let hub = open_hub(&agent);
    let src = data_dir(tmp.path(), "src");

    run_job(&hub, "job-1", spec("alpha", &src)).await;
    run_job(&hub, "job-2", spec("beta", &src)).await;

    let repo_dirs: Vec<_> = std::fs::read_dir(agent.paths.cache_dir())
        .unwrap()
        .collect();
    assert_eq!(repo_dirs.len(), 1);
    let mut names: Vec<String> = std::fs::read_dir(repo_dirs[0].as_ref().unwrap().path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names[0].starts_with("alpha-") && names[1].starts_with("beta-"));

    let third = run_job(&hub, "job-3", spec("alpha", &src)).await;
    assert_eq!(
        finished(&third).unwrap().stats.as_ref().unwrap().bytes_read,
        0
    );
}
