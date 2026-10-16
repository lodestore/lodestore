mod common;

use std::fs::File;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use common::{Api, Backend, TestServer, backends, wait_until};
use lodestore_agent::{AgentError, EnrollArgs, NetOptions, Paths, RunArgs};
use lodestore_format::reader::Reader;
use lodestore_format::repo::Repository;
use lodestore_format::snapshot::SnapshotEntry;
use lodestore_format::store::LocalFsStore;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(90);
const SMALL: u64 = 64 * 1024;

struct Env {
    ts: TestServer,
    api: Api,
    host: String,
    paths: Paths,
    work: TempDir,
    agent: Option<(CancellationToken, JoinHandle<Result<(), AgentError>>)>,
}

impl Env {
    async fn start(backend: &Backend) -> Env {
        let ts = TestServer::start(backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let token = api
            .join_token(&repo, json!([{ "key": "env", "value": "e2e" }]))
            .await;
        let paths = ts.agent_paths("e2e");
        lodestore_agent::enroll(EnrollArgs {
            token,
            paths: paths.clone(),
            hostname: Some("host-e2e".to_string()),
            net: NetOptions::default(),
        })
        .await
        .expect("enroll");
        let host = api.hosts().await[0]["id"].as_str().unwrap().to_string();
        let mut env = Env {
            ts,
            api,
            host,
            paths,
            work: tempfile::tempdir().unwrap(),
            agent: None,
        };
        env.start_agent().await;
        env
    }

    async fn start_agent(&mut self) {
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(lodestore_agent::run(RunArgs {
            paths: self.paths.clone(),
            net: NetOptions::default(),
            heartbeat: Duration::from_secs(1),
            shutdown: shutdown.clone(),
        }));
        self.agent = Some((shutdown, handle));
        let api = &self.api;
        assert!(
            wait_until(WAIT, || async {
                api.hosts().await.iter().any(|h| h["status"] == "online")
            })
            .await,
            "the agent never became online"
        );
    }

    async fn stop_agent(&mut self) {
        let (shutdown, handle) = self.agent.take().expect("the agent is running");
        shutdown.cancel();
        tokio::time::timeout(WAIT, handle)
            .await
            .expect("the agent stops in time")
            .unwrap()
            .expect("the agent stops cleanly");
    }

    async fn stop(mut self) {
        if self.agent.is_some() {
            self.stop_agent().await;
        }
        self.ts.stop().await;
    }

    async fn json(&self, path: &str) -> Value {
        let response = self.api.get(path).await;
        assert_eq!(response.status(), 200, "GET {path}");
        response.json().await.unwrap()
    }

    fn data(&self, name: &str) -> (PathBuf, u64) {
        let dir = self.work.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let old = SystemTime::now() - Duration::from_secs(3600);
        let mut total = 0;
        for i in 0..20 {
            let body = format!("file {i} {}", "x".repeat(i * 500));
            total += body.len() as u64;
            let path = dir.join(format!("file-{i}"));
            std::fs::write(&path, body).unwrap();
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        (dir, total)
    }

    fn install_hook(&self, name: &str, body: &str) {
        let dir = self.paths.hooks_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    async fn policy(&self, name: &str, path: &Path, hooks: &str) {
        let yaml = format!(
            "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: {name}\n\
             spec:\n  repository: main\n  selector:\n    matchLabels:\n      env: e2e\n  \
             paths:\n    - {}\n{hooks}",
            path.display()
        );
        let response = self
            .api
            .post("/api/v1/policies", &json!({ "yaml": yaml }))
            .await;
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        assert_eq!(status, 201, "{body}");
    }

    async fn run(&self, policy: &str) -> String {
        let response = self
            .api
            .post(&format!("/api/v1/policies/{policy}/run"), &json!({}))
            .await;
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["jobs"][0]["created"], true, "{body}");
        body["jobs"][0]["id"].as_str().unwrap().to_string()
    }

    async fn wait_for(&self, id: &str, states: &[&str]) -> Value {
        let deadline = Instant::now() + WAIT;
        loop {
            let job = self.json(&format!("/api/v1/jobs/{id}")).await;
            if states.contains(&job["state"].as_str().unwrap()) {
                return job;
            }
            assert!(Instant::now() < deadline, "job {id} stuck: {job}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn finish(&self, id: &str) -> Value {
        self.wait_for(
            id,
            &[
                "succeeded",
                "warnings",
                "failed",
                "cancelled",
                "interrupted",
                "not_started",
            ],
        )
        .await
    }

    async fn run_to_end(&self, policy: &str) -> Value {
        let id = self.run(policy).await;
        self.finish(&id).await
    }

    async fn events(&self, id: &str, query: &str) -> Vec<Value> {
        self.json(&format!("/api/v1/jobs/{id}/events{query}")).await["events"]
            .as_array()
            .unwrap()
            .clone()
    }

    async fn snapshots(&self) -> Vec<Value> {
        self.json(&format!("/api/v1/hosts/{}/snapshots", self.host))
            .await["items"]
            .as_array()
            .unwrap()
            .clone()
    }

    async fn repo_snapshots(&self) -> Vec<SnapshotEntry> {
        let store = LocalFsStore::new(self.ts.local_repo_root("main"));
        let repo = Repository::open(&store, b"test-password").await.unwrap();
        let reader = Reader::open(&store, repo.read_keys().duplicate(), repo.config()).unwrap();
        reader.list_snapshots().await.unwrap().entries
    }

    async fn wait_file(&self, path: &Path) {
        assert!(
            wait_until(WAIT, || async { path.exists() }).await,
            "{} never appeared",
            path.display()
        );
    }
}

fn pid_alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn sleeper(env: &Env, pid_file: &Path) {
    env.install_hook(
        "slow",
        &format!("echo $$ > {}\nexec sleep 30", pid_file.display()),
    );
}

const BEFORE_SLOW: &str = "  hooks:\n    before:\n      - script: slow\n";

fn stat(job: &Value, key: &str) -> u64 {
    job["stats"][key]
        .as_u64()
        .unwrap_or_else(|| panic!("{job}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_run_now_creates_snapshot() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        let (src, _) = env.data("src");
        env.policy("nightly", &src, "").await;

        let job = env.run_to_end("nightly").await;
        assert_eq!(job["state"], "succeeded", "[{}] {job}", backend.name());
        let snapshot_id = job["snapshot_id"].as_str().expect("a snapshot id");

        let snapshots = env.snapshots().await;
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0]["id"], snapshot_id);
        assert_eq!(snapshots[0]["policy_name"], "nightly");
        assert_eq!(snapshots[0]["job_id"], job["id"]);

        let entries = env.repo_snapshots().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].snapshot.agent_id, env.host.as_bytes());
        assert_eq!(entries[0].snapshot.stats.unwrap().files_new, 20);
        assert_eq!(entries[0].snapshot.tags, vec!["policy=nightly".to_string()]);

        let kinds: Vec<String> = env
            .events(job["id"].as_str().unwrap(), "")
            .await
            .iter()
            .map(|e| e["kind"].as_str().unwrap().to_string())
            .collect();
        assert!(kinds.contains(&"started".to_string()), "{kinds:?}");
        assert!(kinds.contains(&"snapshot_written".to_string()), "{kinds:?}");
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_second_run_uploads_almost_nothing() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        let (src, _) = env.data("src");
        env.policy("nightly", &src, "").await;

        let first = env.run_to_end("nightly").await;
        assert_eq!(first["state"], "succeeded", "{first}");
        assert_eq!(stat(&first, "files_new"), 20);

        let second = env.run_to_end("nightly").await;
        assert_eq!(second["state"], "succeeded", "{second}");
        assert!(stat(&second, "bytes_added") < SMALL, "{second}");
        assert_eq!(stat(&second, "files_unchanged"), 20);
        assert_eq!(stat(&second, "bytes_read"), 0);
        assert_eq!(env.snapshots().await.len(), 2);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_cache_loss_full_rehash() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        let (src, total) = env.data("src");
        env.policy("nightly", &src, "").await;

        let first = env.run_to_end("nightly").await;
        assert_eq!(first["state"], "succeeded", "{first}");
        std::fs::remove_dir_all(env.paths.cache_dir()).unwrap();

        let second = env.run_to_end("nightly").await;
        assert_eq!(second["state"], "succeeded", "{second}");
        assert_eq!(stat(&second, "bytes_read"), total);
        assert!(stat(&second, "bytes_added") < SMALL, "{second}");
        let entries = env.repo_snapshots().await;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].snapshot.root_tree, entries[1].snapshot.root_tree);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_special_files_warn() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        let (src, _) = env.data("src");
        let status = std::process::Command::new("mkfifo")
            .arg(src.join("pipe"))
            .status()
            .unwrap();
        assert!(status.success());
        env.policy("nightly", &src, "").await;

        let job = env.run_to_end("nightly").await;
        assert_eq!(job["state"], "warnings", "{job}");
        assert!(job["snapshot_id"].is_string());
        let id = job["id"].as_str().unwrap();
        let events = env.events(id, "?level=warning").await;
        let pipe = events
            .iter()
            .find(|e| e["kind"] == "special_file")
            .unwrap_or_else(|| panic!("{events:?}"));
        assert!(pipe["path"].as_str().unwrap().ends_with("/pipe"));
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_missing_path_fails() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        let missing = env.work.path().join("nope");
        env.policy("nightly", &missing, "").await;

        let job = env.run_to_end("nightly").await;
        assert_eq!(job["state"], "failed", "{job}");
        assert_eq!(job["error_code"], "path_missing");
        assert_eq!(
            job["error_detail"]["path"].as_str(),
            missing.to_str(),
            "{job}"
        );
        assert!(job["snapshot_id"].is_null());
        assert!(env.snapshots().await.is_empty());
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_hooks_named_only() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        let (src, _) = env.data("src");
        env.policy(
            "inline",
            &src,
            "  hooks:\n    before:\n      - command: \"true\"\n",
        )
        .await;
        env.policy(
            "ghost",
            &src,
            "  hooks:\n    before:\n      - script: not-installed\n",
        )
        .await;

        let inline = env.run_to_end("inline").await;
        assert_eq!(inline["state"], "failed", "{inline}");
        assert_eq!(inline["error_code"], "inline_not_allowed");

        let ghost = env.run_to_end("ghost").await;
        assert_eq!(ghost["state"], "failed", "{ghost}");
        assert_eq!(ghost["error_code"], "hook_missing");
        assert_eq!(ghost["error_detail"]["script"], "not-installed");
        assert!(env.snapshots().await.is_empty());
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_server_restart_mid_job_no_loss_no_dupes() {
    for backend in backends() {
        let mut env = Env::start(&backend).await;
        let (src, _) = env.data("src");
        let marker = env.work.path().join("hook-started");
        env.install_hook("slow", &format!("touch {}\nsleep 4", marker.display()));
        env.policy("nightly", &src, BEFORE_SLOW).await;

        let id = env.run("nightly").await;
        env.wait_file(&marker).await;
        env.ts.restart().await;
        env.api = env.ts.api().await;

        let job = env.finish(&id).await;
        assert_eq!(job["state"], "succeeded", "[{}] {job}", backend.name());
        let page = env.json("/api/v1/jobs?policy=nightly").await;
        assert_eq!(page["total"], 1, "{page}");
        assert_eq!(env.snapshots().await.len(), 1);

        let seqs: Vec<i64> = env
            .events(&id, "")
            .await
            .iter()
            .map(|e| e["seq"].as_i64().unwrap())
            .collect();
        let expected: Vec<i64> = (1..=seqs.len() as i64).collect();
        assert_eq!(seqs, expected);
        assert_eq!(job["log_lines"], seqs.len());
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_cancel_running_job() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        let (src, _) = env.data("src");
        let pid_file = env.work.path().join("slow.pid");
        sleeper(&env, &pid_file);
        env.policy("nightly", &src, BEFORE_SLOW).await;

        let id = env.run("nightly").await;
        env.wait_file(&pid_file).await;
        let pid = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .to_string();
        assert!(pid_alive(&pid));

        let response = env
            .api
            .post(&format!("/api/v1/jobs/{id}/cancel"), &json!({}))
            .await;
        assert_eq!(response.status(), 200);
        let job = env.finish(&id).await;
        assert_eq!(job["state"], "cancelled", "{job}");
        assert!(job["snapshot_id"].is_null());
        assert!(env.snapshots().await.is_empty());
        assert!(
            wait_until(WAIT, || async { !pid_alive(&pid) }).await,
            "the hook process is still running"
        );
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_agent_restart_marks_interrupted() {
    for backend in backends() {
        let mut env = Env::start(&backend).await;
        let (src, _) = env.data("src");
        let pid_file = env.work.path().join("slow.pid");
        sleeper(&env, &pid_file);
        env.policy("nightly", &src, BEFORE_SLOW).await;

        let id = env.run("nightly").await;
        env.wait_file(&pid_file).await;
        env.stop_agent().await;
        env.start_agent().await;

        let job = env.finish(&id).await;
        assert_eq!(job["state"], "interrupted", "[{}] {job}", backend.name());
        assert!(job["snapshot_id"].is_null());
        let queued = env.json("/api/v1/jobs?status=queued").await;
        assert_eq!(queued["total"], 0, "{queued}");
        assert_eq!(env.json("/api/v1/jobs").await["total"], 1);
        env.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_progress_recorded() {
    for backend in backends() {
        let env = Env::start(&backend).await;
        let (src, _) = env.data("src");
        env.install_hook("linger", "sleep 3");
        env.policy(
            "nightly",
            &src,
            "  hooks:\n    after:\n      - script: linger\n",
        )
        .await;

        let job = env.run_to_end("nightly").await;
        assert_eq!(job["state"], "succeeded", "{job}");
        assert_eq!(job["progress"]["scan_done"], true, "{job}");
        assert_eq!(job["progress"]["files_total"], 20);

        let page = env.json("/api/v1/jobs").await;
        assert_eq!(page["items"][0]["progress"]["files_total"], 20);
        env.stop().await;
    }
}
