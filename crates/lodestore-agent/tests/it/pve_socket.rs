#![cfg(target_os = "linux")]

use crate::common::{local_agent, short_socket_dir};

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use lodestore_agent::jobs::JobHub;
use lodestore_agent::pve::socket::{BackupEvent, PveSessions, Token};
use lodestore_agent::pve::{PROVIDER_PROTOCOL, TOKEN_ENV};
use lodestore_agent::socket;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

const NO_TOKEN_TEXT: &str =
    "backups to this storage are started by Lodestore; start this backup from Lodestore";

fn own_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

fn proxmox_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/proxmox")
}

struct Running {
    pve: Arc<PveSessions>,
    socket: PathBuf,
    shutdown: CancellationToken,
    tmp: tempfile::TempDir,
    _socket_dir: tempfile::TempDir,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

async fn start_with(pve: PveSessions) -> Running {
    let tmp = tempfile::tempdir().unwrap();
    let agent = local_agent(tmp.path()).await;
    let hub = JobHub::open(&agent.paths, &agent.agent_id).unwrap();
    let socket_dir = short_socket_dir();
    let socket = socket_dir.path().join("a.sock");
    let shutdown = CancellationToken::new();
    let pve = Arc::new(pve);
    tokio::spawn(socket::serve_with(
        hub,
        Arc::clone(&pve),
        socket.clone(),
        shutdown.clone(),
    ));
    for _ in 0..200 {
        if UnixStream::connect(&socket).is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Running {
        pve,
        socket,
        shutdown,
        tmp,
        _socket_dir: socket_dir,
    }
}

async fn start() -> Running {
    start_with(PveSessions::new(own_uid())).await
}

struct Client {
    write: UnixStream,
    read: BufReader<UnixStream>,
}

impl Client {
    fn connect(path: &Path) -> Client {
        let stream = UnixStream::connect(path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        Client {
            read: BufReader::new(stream.try_clone().unwrap()),
            write: stream,
        }
    }

    fn send(&mut self, message: &Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.write.write_all(line.as_bytes()).unwrap();
    }

    fn recv(&mut self) -> Value {
        let mut line = String::new();
        self.read.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|_| panic!("not a reply: {line:?}"))
    }

    fn at_eof(&mut self) -> bool {
        let mut byte = [0u8; 1];
        matches!(self.read.read(&mut byte), Ok(0) | Err(_))
    }
}

fn request(op: &str, token: &Token, rest: Value) -> Value {
    let mut message = json!({"v": PROVIDER_PROTOCOL, "op": op, "token": token.expose()});
    message
        .as_object_mut()
        .unwrap()
        .extend(rest.as_object().unwrap().clone());
    message
}

fn info(dir: &str) -> Value {
    json!({"directory": dir, "sources": ["./"], "backup-user-id": 100000})
}

fn scenario(run: &Running, dir: &Path, extra: Value) -> PathBuf {
    let mut scenario = json!({
        "dir": dir,
        "config": "arch: amd64\nhostname: ct200\n",
        "excludes": ["/tmp/?*"],
        "mode": "snapshot",
        "sources": ["./"],
        "guest_uid": "abc",
    });
    scenario
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let path = run.tmp.path().join("scenario.json");
    std::fs::write(&path, scenario.to_string()).unwrap();
    path
}

fn fake_vzdump(
    run: &Running,
    scenario: &Path,
    vmid: u32,
    token: Option<&Token>,
) -> tokio::process::Child {
    let proxmox = proxmox_dir();
    let mut command = Command::new("perl");
    command
        .arg("-T")
        .arg("-I")
        .arg(proxmox.join("t/stubs"))
        .arg("-I")
        .arg(&proxmox)
        .arg(proxmox.join("t/fake-vzdump.pl"))
        .args([
            &vmid.to_string(),
            "--storage",
            "lodestore",
            "--mode",
            "snapshot",
        ])
        .args(["--remove", "0", "--protected", "0"])
        .env_remove(TOKEN_ENV)
        .env("LODESTORE_AGENT_SOCKET", &run.socket)
        .env("LODESTORE_FAKE_VZDUMP", scenario)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(token) = token {
        command.env(TOKEN_ENV, token.expose());
    }
    command.spawn().expect("perl is needed for these tests")
}

#[tokio::test(flavor = "multi_thread")]
async fn pve_socket_backup_roundtrip() {
    let run = start().await;
    let dir = run.tmp.path().join("snap");
    let scenario = scenario(&run, &dir, json!({}));
    let (token, mut session) = run.pve.register_backup(200);
    let child = fake_vzdump(&run, &scenario, 200, Some(&token));
    let pid = child.id().unwrap();
    run.pve.set_spawned_pid(&token, pid);

    let consumer = async {
        let Some(BackupEvent::Open {
            info,
            peer_pid,
            reply,
        }) = session.events.recv().await
        else {
            panic!("the first event is not Open");
        };
        assert_eq!(info.directory, dir.to_str().unwrap());
        assert_eq!(info.sources, ["./"]);
        assert_eq!(info.backup_user_id, Some(100_000));
        assert_eq!(info.guest_uid.as_deref(), Some("abc"));
        assert_eq!(peer_pid, pid);
        reply.send(Ok(())).unwrap();

        let Some(BackupEvent::Data {
            config,
            excludes,
            reply,
        }) = session.events.recv().await
        else {
            panic!("the second event is not Data");
        };
        assert_eq!(config, b"arch: amd64\nhostname: ct200\n");
        assert_eq!(excludes, ["/tmp/?*"]);
        reply.send(Ok(())).unwrap();

        let Some(BackupEvent::Finish {
            success,
            error,
            reply,
        }) = session.events.recv().await
        else {
            panic!("the third event is not Finish");
        };
        assert!(success, "{error:?}");
        reply.send(Ok(1234)).unwrap();
    };
    let (_, output) = tokio::join!(consumer, child.wait_with_output());
    let output = output.unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}{stderr}");
    assert!(stdout.contains("backup mode: snapshot"), "{stdout}");
    assert!(stdout.contains("archive-size: 1234"), "{stdout}");
}

#[tokio::test(flavor = "multi_thread")]
async fn pve_socket_no_token_refused() {
    let run = start().await;
    let scenario = scenario(&run, &run.tmp.path().join("snap"), json!({}));
    let (_token, mut session) = run.pve.register_backup(200);
    let child = fake_vzdump(&run, &scenario, 200, None);
    let output = child.wait_with_output().await.unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains(NO_TOKEN_TEXT), "{stderr}");
    assert!(session.events.try_recv().is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn pve_socket_wrong_vmid() {
    let run = start().await;
    let scenario = scenario(&run, &run.tmp.path().join("snap"), json!({}));
    let (token, mut session) = run.pve.register_backup(200);
    let child = fake_vzdump(&run, &scenario, 201, Some(&token));
    let output = child.wait_with_output().await.unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("wrong_guest"), "{stderr}");
    assert!(session.events.try_recv().is_err());
    assert!(!stderr.contains(token.expose()));
}
