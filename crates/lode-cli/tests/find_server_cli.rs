use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};

use assert_cmd::Command;
use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

const TOKEN: &str = "lsk_test_token";
const AT: i64 = 1_792_843_200_000;
const SNAP_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SNAP_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    path: String,
    query: String,
    body: Value,
}

type Responder = Box<dyn Fn(usize, &Recorded) -> (u16, String) + Send + Sync>;

struct Shared {
    routes: HashMap<String, Responder>,
    log: Mutex<Vec<Recorded>>,
}

struct Fake {
    url: String,
    shared: Arc<Shared>,
}

impl Fake {
    fn requests(&self, method: &str, path: &str) -> Vec<Recorded> {
        self.shared
            .log
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == method && r.path == path)
            .cloned()
            .collect()
    }
}

fn status(code: u16, value: Value) -> Responder {
    let text = value.to_string();
    Box::new(move |_, _| (code, text.clone()))
}

fn ok(value: Value) -> Responder {
    status(200, value)
}

async fn handle(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, 1 << 20).await.unwrap();
    let recorded = Recorded {
        method: parts.method.to_string(),
        path: parts.uri.path().to_owned(),
        query: parts.uri.query().unwrap_or_default().to_owned(),
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    };
    let key = format!("{} {}", recorded.method, recorded.path);
    let calls = {
        let mut log = shared.log.lock().unwrap();
        log.push(recorded.clone());
        log.iter()
            .filter(|r| r.method == recorded.method && r.path == recorded.path)
            .count()
            - 1
    };
    let (code, text) = match shared.routes.get(&key) {
        Some(responder) => responder(calls, &recorded),
        None => (
            404,
            json!({"code": "not_found", "message": format!("no route {key}")}).to_string(),
        ),
    };
    (
        StatusCode::from_u16(code).unwrap(),
        [("content-type", "application/json")],
        text,
    )
        .into_response()
}

fn fake(routes: Vec<(&str, Responder)>) -> Fake {
    let shared = Arc::new(Shared {
        routes: routes
            .into_iter()
            .map(|(key, responder)| (key.to_owned(), responder))
            .collect(),
        log: Mutex::new(Vec::new()),
    });
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let app = Router::new()
        .fallback(handle)
        .with_state(Arc::clone(&shared));
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                axum::serve(listener, app).await.unwrap();
            });
    });
    Fake {
        url: format!("http://{addr}"),
        shared,
    }
}

fn lode(server: &Fake, home: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::cargo_bin("lode").unwrap();
    cmd.env_remove("LODE_REPOSITORY")
        .env("XDG_CONFIG_HOME", home)
        .env("HOME", home)
        .env("LODESTORE_SERVER", &server.url)
        .env("LODESTORE_TOKEN", TOKEN)
        .args(["server", "find"])
        .args(args);
    cmd
}

fn output(cmd: &mut Command, code: i32) -> (String, String) {
    let out = cmd.assert().code(code).get_output().clone();
    (
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

fn host(key: &str, hostname: &str, agent: bool, state: Option<&str>) -> Value {
    json!({
        "key": key, "host_id": agent.then_some(key), "hostname": hostname, "agent": agent,
        "status": state, "labels": [], "repository_id": "r1", "repository_name": "main",
        "snapshot_count": 3, "latest_snapshot_ms": AT
    })
}

fn hosts() -> (&'static str, Responder) {
    (
        "GET /api/v1/restore/hosts",
        ok(json!([
            host("a1", "web-1", true, Some("online")),
            host("a2", "db-1", true, Some("offline")),
            host("agentless:old-box", "old-box", false, None),
        ])),
    )
}

fn group(index: u64, host_key: &str, hostname: &str, path: &str, snapshot: &str) -> Value {
    json!({
        "index": index, "host_key": host_key, "host_id": null, "hostname": hostname,
        "agent": host_key.len() == 2, "labels": [], "path_b64": "", "path": path,
        "last_seen_ms": AT, "gone": null, "revision": index + 1,
        "versions": [
            {"content_id": "01", "kind": "file", "size": 5, "mtime_ms": AT - 1000,
             "first_ms": AT - 2000, "last_ms": AT - 1000, "snapshot_count": 1,
             "snapshots": [{"id": "0".repeat(64), "time_ms": AT - 1000}]},
            {"content_id": "02", "kind": "file", "size": 6, "mtime_ms": AT,
             "first_ms": AT, "last_ms": AT, "snapshot_count": 2,
             "snapshots": [{"id": snapshot, "time_ms": AT}, {"id": "1".repeat(64), "time_ms": AT - 10}]}
        ]
    })
}

fn session(state: &str, stop: Value, groups: Vec<Value>) -> Value {
    let revision = groups.len() as u64;
    json!({
        "id": "f1", "pattern": "nginx.conf", "state": state, "stop_reason": stop,
        "limit": 1000, "scanned": 4, "total": 4, "skipped": 0, "hosts": 2,
        "files": groups.len(), "versions": 2, "started_at": AT, "finished_at": AT + 5000,
        "error": null, "revision": revision, "groups": groups
    })
}

fn start() -> (&'static str, Responder) {
    ("POST /api/v1/find", status(202, json!({"id": "f1"})))
}

fn finished(state: &str, stop: Value, groups: Vec<Value>) -> Vec<(&'static str, Responder)> {
    vec![
        hosts(),
        start(),
        ("GET /api/v1/find/f1", ok(session(state, stop, groups))),
    ]
}

#[test]
fn find_prints_table_and_restore_command() {
    let g1 = group(0, "a1", "web-1", "/etc/nginx/nginx.conf", SNAP_A);
    let g2 = group(1, "agentless:old-box", "old-box", "/srv/nginx.conf", SNAP_B);
    let running = session("running", Value::Null, vec![g1.clone()]);
    let done = session("done", Value::Null, vec![g2]);
    let (running_text, done_text) = (running.to_string(), done.to_string());
    let poll: Responder = Box::new(move |call, _| {
        if call == 0 {
            (200, running_text.clone())
        } else {
            (200, done_text.clone())
        }
    });
    let mut routes = vec![hosts(), start()];
    routes.push(("GET /api/v1/find/f1", poll));
    let server = fake(routes);
    let home = tempfile::tempdir().unwrap();
    let (stdout, stderr) = output(&mut lode(&server, home.path(), &["nginx.conf"]), 0);
    assert_eq!(stderr, "");
    let header = stdout.lines().next().unwrap();
    for column in ["HOST", "PATH", "VERSIONS", "LAST SEEN", "SNAPSHOT"] {
        assert!(header.contains(column), "{stdout}");
    }
    assert!(stdout.contains("/etc/nginx/nginx.conf"), "{stdout}");
    assert!(stdout.contains("/srv/nginx.conf"), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "lode server restore {SNAP_A} --host web-1 --path /etc/nginx/nginx.conf"
        )),
        "{stdout}"
    );
    let polls = server.requests("GET", "/api/v1/find/f1");
    assert_eq!(polls.len(), 2);
    assert_eq!(polls[0].query, "after=0");
    assert_eq!(polls[1].query, "after=1");
}

#[test]
fn find_hints_another_host_when_the_agent_is_not_online() {
    for (key, hostname) in [("agentless:old-box", "old-box"), ("a2", "db-1")] {
        let g = group(0, key, hostname, "/etc/app.conf", SNAP_A);
        let server = fake(finished("done", Value::Null, vec![g]));
        let home = tempfile::tempdir().unwrap();
        let (stdout, _) = output(&mut lode(&server, home.path(), &["app.conf"]), 0);
        assert!(!stdout.contains(&format!("--host {hostname}")), "{stdout}");
        assert!(
            stdout.contains(&format!(
                "{hostname} has no online agent. Restore to another host with lode server restore {SNAP_A} --host <HOST> --path /etc/app.conf, or download it from the web UI."
            )),
            "{stdout}"
        );
    }
}

#[test]
fn find_filters_hosts_and_labels() {
    let server = fake(finished("done", Value::Null, vec![]));
    let home = tempfile::tempdir().unwrap();
    let args = [
        "x",
        "--host",
        "web-1",
        "--host",
        "agentless:old-box",
        "--label",
        "role=web",
    ];
    let (stdout, _) = output(&mut lode(&server, home.path(), &args), 0);
    assert_eq!(stdout.trim(), "No files match \"x\".");
    let post = &server.requests("POST", "/api/v1/find")[0];
    assert_eq!(post.body["pattern"], "x");
    assert_eq!(post.body["hosts"], json!(["a1", "agentless:old-box"]));
    assert_eq!(post.body["labels"], json!(["role=web"]));
}

#[test]
fn find_rejects_unknown_hosts_and_bad_labels_before_searching() {
    let server = fake(finished("done", Value::Null, vec![]));
    let home = tempfile::tempdir().unwrap();
    let (_, stderr) = output(&mut lode(&server, home.path(), &["x", "--host", "nope"]), 2);
    assert!(stderr.contains("No host matches \"nope\""), "{stderr}");
    let (_, stderr) = output(
        &mut lode(&server, home.path(), &["x", "--label", "role"]),
        2,
    );
    assert!(stderr.contains("KEY=VALUE"), "{stderr}");
    assert!(server.requests("POST", "/api/v1/find").is_empty());
}

#[test]
fn find_incomplete_warns_and_exits_zero() {
    let g = group(0, "a1", "web-1", "/etc/nginx/nginx.conf", SNAP_A);
    let server = fake(finished("incomplete", json!("limit"), vec![g]));
    let home = tempfile::tempdir().unwrap();
    let (stdout, stderr) = output(&mut lode(&server, home.path(), &["nginx"]), 0);
    assert!(stderr.contains("Stopped at 1000 matches."), "{stderr}");
    assert!(stdout.contains("/etc/nginx/nginx.conf"), "{stdout}");

    let server = fake(finished("incomplete", json!("timeout"), vec![]));
    let (_, stderr) = output(&mut lode(&server, home.path(), &["nginx"]), 0);
    assert!(stderr.contains("Stopped after 5 seconds."), "{stderr}");
}

#[test]
fn find_failed_reports_the_reason() {
    let mut failed = session("failed", Value::Null, vec![]);
    failed["error"] = json!("The catalog is unavailable.");
    let server = fake(vec![hosts(), start(), ("GET /api/v1/find/f1", ok(failed))]);
    let home = tempfile::tempdir().unwrap();
    let (_, stderr) = output(&mut lode(&server, home.path(), &["nginx"]), 1);
    assert!(stderr.contains("The catalog is unavailable."), "{stderr}");
}

#[test]
fn find_json_is_one_session_with_every_group() {
    let g1 = group(0, "a1", "web-1", "/a/nginx.conf", SNAP_A);
    let g2 = group(1, "a1", "web-1", "/b/nginx.conf", SNAP_B);
    let server = fake(finished("done", Value::Null, vec![g1, g2]));
    let home = tempfile::tempdir().unwrap();
    let (stdout, _) = output(&mut lode(&server, home.path(), &["nginx", "--json"]), 0);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "{stdout}");
    let doc: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(doc["type"], "find_session");
    assert_eq!(doc["state"], "done");
    assert_eq!(doc["groups"].as_array().unwrap().len(), 2);
    assert!(!stdout.contains("HOST"));
}

#[test]
fn find_forbidden_token_exits_with_the_token_code() {
    let server = fake(vec![
        hosts(),
        (
            "POST /api/v1/find",
            status(
                403,
                json!({"code": "forbidden", "message": "The token needs the snapshots:read scope."}),
            ),
        ),
    ]);
    let home = tempfile::tempdir().unwrap();
    let (_, stderr) = output(&mut lode(&server, home.path(), &["nginx"]), 13);
    assert!(stderr.contains("snapshots:read"), "{stderr}");
    assert!(!stderr.contains(TOKEN), "{stderr}");
}

#[test]
fn find_invalid_pattern_is_a_usage_error() {
    let server = fake(vec![
        hosts(),
        (
            "POST /api/v1/find",
            status(
                422,
                json!({"errors": [{"path": "pattern", "message": "The pattern is empty."}]}),
            ),
        ),
    ]);
    let home = tempfile::tempdir().unwrap();
    let (_, stderr) = output(&mut lode(&server, home.path(), &[""]), 2);
    assert!(stderr.contains("The pattern is empty."), "{stderr}");
}

#[test]
fn find_names_with_control_characters_are_neutralised() {
    let g = group(0, "a1", "web-1", "/tmp/evil\u{1b}[2J.conf", SNAP_A);
    let server = fake(finished("done", Value::Null, vec![g]));
    let home = tempfile::tempdir().unwrap();
    let (stdout, _) = output(&mut lode(&server, home.path(), &["evil"]), 0);
    assert!(!stdout.contains('\u{1b}'), "{stdout:?}");
    assert!(stdout.contains("/tmp/evil?[2J.conf"), "{stdout}");
    assert!(!stdout.contains("lode server restore"), "{stdout}");
}
