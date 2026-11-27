#![cfg(target_os = "linux")]

use crate::common::{local_agent, short_socket_dir};

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use lodestore_agent::jobs::JobHub;
use lodestore_agent::pve::socket::{
    BackupEvent, MAX_PVE_LINE, MAX_PVE_SESSIONS, PveSessions, Token,
};
use lodestore_agent::pve::{PROVIDER_PROTOCOL, TOKEN_ENV};
use lodestore_agent::socket::{self, MAX_HANDLERS};
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
        let _ = self.read.read_line(&mut line);
        if line.is_empty() {
            return Value::Null;
        }
        serde_json::from_str(&line).unwrap_or_else(|_| panic!("not a reply: {line:?}"))
    }

    fn at_eof(&mut self) -> bool {
        let mut byte = [0u8; 1];
        matches!(self.read.read(&mut byte), Ok(0) | Err(_))
    }

    fn ask(&mut self, message: &Value) -> Value {
        self.send(message);
        self.recv()
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

fn job_check(token: &Token) -> Value {
    request("pve_job_check", token, json!({}))
}

fn code(reply: &Value) -> &str {
    reply["code"].as_str().unwrap_or_default()
}

fn open_message(token: &str, vmid: u32) -> Value {
    json!({"v": PROVIDER_PROTOCOL, "op": "pve_ct_open", "token": token, "vmid": vmid,
        "info": info("/mnt/vzsnap0")})
}

#[tokio::test(flavor = "multi_thread")]
async fn pve_socket_non_root_forbidden() {
    let run = start_with(PveSessions::new(own_uid().wrapping_add(1))).await;
    let (token, mut session) = run.pve.register_backup(200);
    for message in [job_check(&token), open_message(token.expose(), 200)] {
        let mut client = Client::connect(&run.socket);
        let reply = client.ask(&message);
        assert_eq!(code(&reply), "forbidden", "{reply}");
        assert!(client.at_eof());
    }
    assert!(session.events.try_recv().is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn pve_socket_bad_token() {
    let run = start().await;
    let (mine, mut session) = run.pve.register_backup(200);
    let (gone, dropped) = run.pve.register_backup(200);
    drop(dropped);
    let lookalike = format!("{}0", &mine.expose()[..63]);
    for token in [
        gone.expose().to_string(),
        mine.expose()[..63].to_string(),
        "z".repeat(64),
        lookalike,
    ] {
        let mut client = Client::connect(&run.socket);
        let reply = client.ask(&open_message(&token, 200));
        assert_eq!(code(&reply), "bad_token", "{reply}");
        assert!(client.at_eof());
    }
    assert!(session.events.try_recv().is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn pve_socket_wrong_process() {
    let run = start().await;
    let (token, _session) = run.pve.register_backup(200);
    let mut stranger = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    run.pve.set_spawned_pid(&token, stranger.id());
    let mut client = Client::connect(&run.socket);
    let reply = client.ask(&job_check(&token));
    assert_eq!(code(&reply), "wrong_process", "{reply}");
    assert!(client.at_eof());

    run.pve.set_spawned_pid(&token, std::process::id());
    let mut client = Client::connect(&run.socket);
    assert_eq!(client.ask(&job_check(&token))["ok"], true);
    stranger.kill().unwrap();
    stranger.wait().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn pve_socket_oversized_line() {
    let run = start().await;
    let (token, mut session) = run.pve.register_backup(200);
    let blob = vec![b'x'; MAX_PVE_LINE + 4096];

    let mut client = Client::connect(&run.socket);
    let _ = client.write.write_all(&blob);
    assert_eq!(code(&client.recv()), "bad_request");
    assert!(client.at_eof());

    let consumer = tokio::spawn(async move {
        let Some(BackupEvent::Open { reply, .. }) = session.events.recv().await else {
            panic!("no Open event");
        };
        reply.send(Ok(())).unwrap();
        let last = session.events.recv().await;
        assert!(matches!(last, Some(BackupEvent::Aborted)), "{last:?}");
    });
    let mut client = Client::connect(&run.socket);
    assert_eq!(client.ask(&open_message(token.expose(), 200))["ok"], true);
    let _ = client.write.write_all(&blob);
    assert_eq!(code(&client.recv()), "bad_request");
    assert!(client.at_eof());
    consumer.await.unwrap();

    let (other, _other_session) = run.pve.register_backup(201);
    let mut client = Client::connect(&run.socket);
    assert_eq!(client.ask(&job_check(&other))["ok"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn pve_socket_idle_first_line() {
    let wait = Duration::from_millis(300);
    let run = start_with(PveSessions::new(own_uid()).with_first_line_timeout(wait)).await;
    let (token, _session) = run.pve.register_backup(200);
    let mut idle: Vec<Client> = (0..MAX_HANDLERS)
        .map(|_| Client::connect(&run.socket))
        .collect();
    let mut over = Client::connect(&run.socket);
    assert_eq!(code(&over.recv()), "busy");

    for client in &mut idle {
        assert_eq!(code(&client.recv()), "bad_request");
        assert!(client.at_eof());
    }

    let mut client = Client::connect(&run.socket);
    assert_eq!(client.ask(&job_check(&token))["ok"], true);
    let mut client = Client::connect(&run.socket);
    let reply = client.ask(&json!({"v": 1, "op": "backup_stdin", "name": "a/b"}));
    assert_eq!(reply["code"], "bad_request", "{reply}");
}

#[tokio::test(flavor = "multi_thread")]
async fn pve_socket_session_limit() {
    let run = start().await;
    let mut held = Vec::new();
    for vmid in 0..MAX_PVE_SESSIONS {
        let vmid = 100 + u32::try_from(vmid).unwrap();
        let (token, mut session) = run.pve.register_backup(vmid);
        let mut client = Client::connect(&run.socket);
        client.send(&open_message(token.expose(), vmid));
        let Some(BackupEvent::Open { reply, .. }) = session.events.recv().await else {
            panic!("no Open event");
        };
        reply.send(Ok(())).unwrap();
        assert_eq!(client.recv()["ok"], true);
        held.push((client, token, session));
    }

    let (token, _session) = run.pve.register_backup(300);
    let mut client = Client::connect(&run.socket);
    let reply = client.ask(&job_check(&token));
    assert_eq!(code(&reply), "busy", "{reply}");

    let mut stdin = Client::connect(&run.socket);
    let reply = stdin.ask(&json!({"v": 1, "op": "backup_stdin", "name": "a/b"}));
    assert_eq!(reply["code"], "bad_request", "{reply}");

    held.pop();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut client = Client::connect(&run.socket);
    assert_eq!(client.ask(&job_check(&token))["ok"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn pve_socket_protocol_mismatch() {
    let run = start().await;
    let (token, _session) = run.pve.register_backup(200);
    for version in [0, PROVIDER_PROTOCOL + 1] {
        let mut client = Client::connect(&run.socket);
        let reply =
            client.ask(&json!({"v": version, "op": "pve_job_check", "token": token.expose()}));
        assert_eq!(code(&reply), "protocol_mismatch", "{reply}");
        let message = reply["message"].as_str().unwrap();
        assert!(message.contains("lodestore-agent pve install"), "{reply}");
        assert!(client.at_eof());
    }
    let mut client = Client::connect(&run.socket);
    let reply = client.ask(&json!({"op": "pve_job_check", "token": token.expose()}));
    assert_eq!(code(&reply), "bad_request", "{reply}");

    let provider =
        std::fs::read_to_string(proxmox_dir().join("PVE/BackupProvider/Plugin/Lodestore.pm"))
            .unwrap();
    let declared = provider
        .lines()
        .find_map(|l| l.strip_prefix("use constant PROTOCOL => "))
        .and_then(|rest| rest.trim_end_matches(';').parse::<u32>().ok());
    assert_eq!(declared, Some(PROVIDER_PROTOCOL));
}

#[test]
fn pve_plugin_refuses_deletion() {
    let proxmox = proxmox_dir();
    let script = r#"
        use PVE::Storage::Custom::LodestorePlugin;
        my $p = 'PVE::Storage::Custom::LodestorePlugin';
        for my $m (qw(free_image prune_backups alloc_image volume_snapshot volume_resize)) {
            eval { $p->$m() };
            print "$m: $@";
        }
        print join(' ', map { $_ // 'undef' } $p->parse_volname('backup/ct/200/1732600000')), "\n";
        print join(' ', map { $_ // 'undef' } $p->parse_volname('backup/restore/' . ('ab' x 16))), "\n";
        for my $bad ('backup/../x', 'backup/ct/200/../1', 'backup/restore/xyz') {
            eval { $p->parse_volname($bad) };
            print "parse: $@";
        }
        print 'api: ', $p->api(), "\n";
    "#;
    let run = |apiver: &str| {
        let output = std::process::Command::new("perl")
            .arg("-I")
            .arg(proxmox.join("t/stubs"))
            .arg("-I")
            .arg(&proxmox)
            .arg("-e")
            .arg(script)
            .env("LODESTORE_FAKE_APIVER", apiver)
            .output()
            .expect("perl is needed for these tests");
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    let out = run("17");
    for name in ["free_image", "prune_backups"] {
        let line = out.lines().find(|l| l.starts_with(name)).unwrap();
        assert!(
            line.contains("deletion is managed by the Lodestore server"),
            "{line}"
        );
    }
    for name in ["alloc_image", "volume_snapshot", "volume_resize"] {
        let line = out.lines().find(|l| l.starts_with(name)).unwrap();
        assert!(
            line.contains("not supported by Lodestore storage"),
            "{line}"
        );
    }
    assert!(out.contains("\nbackup ct/200/1732600000 200\n"), "{out}");
    let restore = format!("\nbackup restore/{} undef\n", "ab".repeat(16));
    assert!(out.contains(&restore), "{out}");
    assert_eq!(out.matches("parse: unable to parse").count(), 3, "{out}");
    assert!(out.contains("api: 17"));
    assert!(run("12").contains("api: 12"));
    assert!(run("18").contains("api: 17"));
}
