mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use assert_cmd::Command;
use lodestore_agent::Paths;
use lodestore_agent::identity::{write_json_file, write_private_file};
use lodestore_agent::jobs::JobHub;
use lodestore_agent::socket;
use lodestore_format::crypto::Entropy;
use lodestore_format::keys::KdfCost;
use lodestore_format::repo::{InitOptions, PendingInit};
use lodestore_format::store::LocalFsStore;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

const AGENT_PASSWORD: &str = "agent-test-password";

struct Agent {
    repo: PathBuf,
    socket: PathBuf,
    _dir: tempfile::TempDir,
    stop: CancellationToken,
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn prepare(dir: &Path) -> (Paths, PathBuf) {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let store = LocalFsStore::new(repo.clone());
    let mut entropy = Entropy::os();
    let pending = PendingInit::generate(
        &mut entropy,
        InitOptions {
            kdf: KdfCost {
                memory_kib: 64,
                iterations: 1,
                parallelism: 1,
            },
            ..InitOptions::default()
        },
    )
    .unwrap();
    let config_hash = lodestore_format::config::config_hash(pending.config_bytes());
    let writer_keys = pending.read_keys().write_keys(config_hash).to_file_text();
    pending
        .commit(&store, AGENT_PASSWORD.as_bytes(), &mut entropy)
        .await
        .unwrap();
    let paths = Paths {
        config_dir: dir.join("etc"),
        state_dir: dir.join("var"),
    };
    write_private_file(&paths.writer_keys(), writer_keys.as_bytes()).unwrap();
    write_json_file(
        &paths.repository_json(),
        &serde_json::json!({
            "repository_id": "test",
            "backend": "local",
            "options": BTreeMap::from([("path", repo.display().to_string())]),
        }),
    )
    .unwrap();
    (paths, repo)
}

fn start_agent() -> Agent {
    let dir = tempfile::Builder::new()
        .prefix("ls")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("a.sock");
    let stop = CancellationToken::new();
    let (ready, ready_rx) = std::sync::mpsc::channel();
    std::thread::spawn({
        let dir = dir.path().to_path_buf();
        let socket = socket.clone();
        let stop = stop.clone();
        move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let (paths, repo) = prepare(&dir).await;
                let hub = JobHub::open(&paths, "agent-interop").unwrap();
                tokio::spawn(socket::serve(Arc::clone(&hub), socket, stop.clone()));
                ready.send(repo).unwrap();
                stop.cancelled().await;
            });
        }
    });
    let repo = ready_rx.recv().unwrap();
    for _ in 0..400 {
        if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    Agent {
        repo,
        socket,
        _dir: dir,
        stop,
    }
}

fn lode() -> Command {
    let mut cmd = Command::cargo_bin("lode").unwrap();
    cmd.env_remove("LODE_REPOSITORY")
        .env_remove("LODE_PASSWORD")
        .env_remove("LODE_PASSWORD_FILE");
    cmd
}

#[test]
fn stdin_agent_interop_roundtrip() {
    let agent = start_agent();
    let data = common::pseudo_random(1 << 20);
    let out = lode()
        .args(["backup", "--stdin", "--name", "app.dump", "--json"])
        .arg("--agent-socket")
        .arg(&agent.socket)
        .write_stdin(data.clone())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let lines: Vec<Value> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let types: Vec<&str> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
    assert!(types.contains(&"backup_progress"), "{types:?}");
    let summary = lines.last().unwrap();
    assert_eq!(summary["type"], "backup_summary");
    assert_eq!(summary["stats"]["bytes_read"], 1 << 20);

    let dumped = lode()
        .env("LODE_PASSWORD", AGENT_PASSWORD)
        .arg("-r")
        .arg(&agent.repo)
        .args(["dump", "latest", "/app.dump"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(dumped, data);
}
