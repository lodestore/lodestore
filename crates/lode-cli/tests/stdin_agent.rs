mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::thread::JoinHandle;

use assert_cmd::Command;
use assert_cmd::assert::Assert;
use serde_json::Value;

struct Received {
    header: String,
    frames: Vec<usize>,
    data: Vec<u8>,
    closed_with_empty_frame: bool,
}

struct Fake {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    handle: JoinHandle<Received>,
}

fn serve(replies: Vec<String>) -> Fake {
    let dir = tempfile::Builder::new()
        .prefix("ls")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("s");
    let listener = UnixListener::bind(&socket).unwrap();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        let mut got = Received {
            header,
            frames: Vec::new(),
            data: Vec::new(),
            closed_with_empty_frame: false,
        };
        loop {
            let mut len = [0u8; 4];
            if reader.read_exact(&mut len).is_err() {
                break;
            }
            let len = u32::from_be_bytes(len) as usize;
            if len == 0 {
                got.closed_with_empty_frame = true;
                break;
            }
            assert!(len <= 1_048_576);
            let mut frame = vec![0u8; len];
            reader.read_exact(&mut frame).unwrap();
            got.frames.push(len);
            got.data.extend_from_slice(&frame);
        }
        let mut stream = stream;
        for line in replies {
            writeln!(stream, "{line}").unwrap();
        }
        got
    });
    Fake {
        _dir: dir,
        socket,
        handle,
    }
}

fn lode() -> Command {
    let mut cmd = Command::cargo_bin("lode").unwrap();
    cmd.env_remove("LODE_REPOSITORY")
        .env_remove("LODE_PASSWORD")
        .env_remove("LODE_PASSWORD_FILE");
    cmd
}

fn stdout_of(assert: &Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stdout).into_owned()
}

fn stderr_of(assert: &Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stderr).into_owned()
}

const ACCEPTED: &str = r#"{"v":1,"type":"accepted","job_id":"j1"}"#;
const PROGRESS: &str = r#"{"v":1,"type":"backup_progress","files_done":0,"bytes_done":10,"bytes_added":5,"current_path":"/app.dump","files_total":0,"bytes_total":0,"scan_done":false,"bytes_processed":10}"#;
const SUMMARY: &str = r#"{"v":1,"type":"backup_summary","snapshot_id":"abc123","incomplete":false,"parent":null,"stats":{"files_new":1,"files_changed":0,"files_unchanged":0,"dirs":0,"bytes_total":10,"bytes_read":10,"bytes_added":5},"errors_total":0,"warnings_total":0}"#;

fn ok_replies() -> Vec<String> {
    vec![ACCEPTED.into(), PROGRESS.into(), SUMMARY.into()]
}

#[test]
fn stdin_agent_roundtrip() {
    let fake = serve(ok_replies());
    let data = common::pseudo_random(1 << 20);
    let assert = lode()
        .args(["backup", "--stdin", "--name", "app.dump", "--json"])
        .arg("--agent-socket")
        .arg(&fake.socket)
        .write_stdin(data.clone())
        .assert()
        .success();
    let got = fake.handle.join().unwrap();
    assert_eq!(
        got.header,
        "{\"v\":1,\"op\":\"backup_stdin\",\"name\":\"app.dump\",\"policy\":null}\n"
    );
    assert_eq!(got.data, data);
    assert_eq!(got.frames, vec![65536; 16]);
    assert!(got.closed_with_empty_frame);
    let out = stdout_of(&assert);
    assert_eq!(out.lines().collect::<Vec<_>>(), [PROGRESS, SUMMARY]);
}

#[test]
fn stdin_agent_policy_in_header() {
    let fake = serve(ok_replies());
    lode()
        .args(["backup", "--stdin", "--name", "a", "--policy", "nightly"])
        .arg("--agent-socket")
        .arg(&fake.socket)
        .write_stdin("x")
        .assert()
        .success();
    let got = fake.handle.join().unwrap();
    let header: Value = serde_json::from_str(&got.header).unwrap();
    assert_eq!(header["policy"], "nightly");
}

#[test]
fn stdin_agent_conflicts_with_repository() {
    let repo = tempfile::tempdir().unwrap();
    for flag in ["--agent", "--agent-socket=/tmp/none"] {
        lode()
            .arg("-r")
            .arg(repo.path())
            .args(["backup", "--stdin", "--name", "x", flag])
            .write_stdin("x")
            .assert()
            .code(2);
    }
    lode()
        .env("LODE_REPOSITORY", repo.path())
        .args(["backup", "--stdin", "--name", "x", "--agent"])
        .write_stdin("x")
        .assert()
        .code(2);
    lode().args(["backup", "--agent", "/tmp"]).assert().code(2);
}

#[test]
fn stdin_agent_missing_socket_fails() {
    let assert = lode()
        .args(["backup", "--stdin", "--name", "x"])
        .args(["--agent-socket", "/nonexistent/sock"])
        .write_stdin("x")
        .assert()
        .code(1);
    assert!(stderr_of(&assert).contains("cannot reach the agent at /nonexistent/sock"));
}

#[test]
fn stdin_agent_error_reply_exits_1() {
    let fake = serve(vec![
        r#"{"v":1,"type":"error","code":"busy","message":"another backup is running"}"#.into(),
    ]);
    let assert = lode()
        .args(["backup", "--stdin", "--name", "x"])
        .arg("--agent-socket")
        .arg(&fake.socket)
        .write_stdin("x")
        .assert()
        .code(1);
    assert!(stderr_of(&assert).contains("another backup is running"));
    fake.handle.join().unwrap();
}

#[test]
fn stdin_agent_human_output() {
    let fake = serve(ok_replies());
    let assert = lode()
        .args(["backup", "--stdin", "--name", "x"])
        .arg("--agent-socket")
        .arg(&fake.socket)
        .write_stdin("x")
        .assert()
        .success();
    assert!(stdout_of(&assert).contains("snapshot abc123 saved"));
    assert!(stderr_of(&assert).contains("queued as job j1"));
    fake.handle.join().unwrap();
}
