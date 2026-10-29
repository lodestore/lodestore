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

#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    path: String,
    auth: Option<String>,
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
    fn requests(&self) -> Vec<Recorded> {
        self.shared.log.lock().unwrap().clone()
    }

    fn only(&self, method: &str, path: &str) -> Recorded {
        let found: Vec<_> = self
            .requests()
            .into_iter()
            .filter(|r| r.method == method && r.path == path)
            .collect();
        assert_eq!(found.len(), 1, "{method} {path}: {found:?}");
        found[0].clone()
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
    to_bytes(body, 1 << 20).await.unwrap();
    let recorded = Recorded {
        method: parts.method.to_string(),
        path: parts.uri.path().to_owned(),
        auth: parts
            .headers
            .get("authorization")
            .map(|v| v.to_str().unwrap().to_owned()),
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
        .args(["server", "check"])
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

const AT: i64 = 1_792_843_200_000;

fn gc_probe() -> (&'static str, Responder) {
    (
        "GET /api/v1/repositories/r1/gc",
        ok(json!({"enabled": true})),
    )
}

fn run_dto(level: &str, state: &str, outcome: Value, summary: Value, findings: Value) -> Value {
    let total = findings.as_array().map_or(0, Vec::len);
    json!({
        "id": "run1", "level": level, "origin": "manual", "state": state,
        "outcome": outcome, "progress": null, "created_at": AT, "started_at": AT,
        "finished_at": AT, "duration_ms": 1000, "started_by": "token",
        "findings_total": total, "error": null, "summary": summary, "findings": findings
    })
}

fn structure_summary() -> Value {
    json!({"structure": {
        "snapshots_checked": 12, "trees_checked": 3400, "packs_listed": 118, "missing_packs": 0
    }})
}

fn started(level: &str) -> (&'static str, Responder) {
    let route = match level {
        "structure" => "POST /api/v1/repositories/r1/checks/structure/run",
        "packs" => "POST /api/v1/repositories/r1/checks/packs/run",
        _ => "POST /api/v1/repositories/r1/checks/restore/run",
    };
    (route, status(202, json!({"run_id": "run1"})))
}

fn run_and_report(level: &str, run: Value, findings: Value) -> Vec<(&'static str, Responder)> {
    vec![
        gc_probe(),
        started(level),
        (
            "GET /api/v1/repositories/r1/checks/runs/run1",
            ok(run.clone()),
        ),
        (
            "GET /api/v1/repositories/r1/checks/runs/run1/report",
            ok(json!({"run": run, "findings": findings, "notes": []})),
        ),
    ]
}

#[test]
fn check_server_cli_passed() {
    let run = run_dto(
        "structure",
        "done",
        json!("passed"),
        structure_summary(),
        json!([]),
    );
    let server = fake(run_and_report("structure", run, json!([])));
    let home = tempfile::tempdir().unwrap();
    let (stdout, _) = output(&mut lode(&server, home.path(), &["r1"]), 0);
    assert_eq!(
        stdout.lines().next().unwrap(),
        "Structure check for repository r1 (server)"
    );
    assert!(stdout.contains("Passed"), "{stdout}");
    assert!(stdout.contains("Trees checked"), "{stdout}");
    assert!(stdout.contains("3,400"), "{stdout}");
    let post = server.only("POST", "/api/v1/repositories/r1/checks/structure/run");
    assert!(post.path.contains("/checks/structure/run"));
    assert_eq!(post.auth.as_deref(), Some("Bearer lsk_test_token"));
}

#[test]
fn check_server_cli_problems() {
    let findings = json!([
        {"kind": "missing_pack", "key": "data/ab/abcdef", "detail": "The pack is gone."}
    ]);
    let run = run_dto(
        "structure",
        "failed",
        json!("problems"),
        structure_summary(),
        findings.clone(),
    );
    let server = fake(run_and_report("structure", run, findings));
    let home = tempfile::tempdir().unwrap();
    let (stdout, _) = output(&mut lode(&server, home.path(), &["r1"]), 3);
    assert!(stdout.contains("Failed"), "{stdout}");
    let problems = stdout.split("Problems").nth(1).expect("a Problems block");
    assert!(
        problems.contains("data/ab/abcdef   The pack is gone."),
        "{stdout}"
    );
}

#[test]
fn check_server_cli_blocked() {
    let server = fake(vec![
        gc_probe(),
        (
            "POST /api/v1/repositories/r1/checks/structure/run",
            status(
                409,
                json!({"code": "conflict", "message": "A cleanup cycle is running. Checks wait for it."}),
            ),
        ),
    ]);
    let home = tempfile::tempdir().unwrap();
    let (_, stderr) = output(&mut lode(&server, home.path(), &["r1"]), 1);
    assert!(
        stderr.contains("A cleanup cycle is running. Checks wait for it."),
        "{stderr}"
    );
}

#[test]
fn check_server_cli_packs_coverage() {
    let summary = json!({"packs": {
        "packs_read": 40, "bytes_read": 5_368_709_120u64, "blobs_verified": 9000, "damaged_blobs": 0
    }});
    let run = run_dto("packs", "done", json!("passed"), summary, json!([]));
    let mut routes = run_and_report("packs", run, json!([]));
    routes.push((
        "GET /api/v1/repositories/r1/checks",
        ok(json!({
            "repository_id": "r1", "levels": [],
            "coverage": {"packs": 300, "checked_30d_pct": 62, "never_checked": 118}
        })),
    ));
    let server = fake(routes);
    let home = tempfile::tempdir().unwrap();
    let args = ["r1", "--level", "packs"];
    let (stdout, _) = output(&mut lode(&server, home.path(), &args), 0);
    assert_eq!(
        stdout.lines().next().unwrap(),
        "Pack reading check for repository r1 (server)"
    );
    for fact in ["Packs read", "Data read", "Blobs verified", "Damaged blobs"] {
        assert!(stdout.contains(fact), "{stdout}");
    }
    let row = |label: &str| {
        stdout
            .lines()
            .find(|l| l.trim_start().starts_with(label))
            .unwrap_or_else(|| panic!("no {label} row in {stdout}"))
            .split_whitespace()
            .skip(label.split_whitespace().count())
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert_eq!(row("Coverage"), "62% of packs checked in the last 30 days");
    assert_eq!(row("Never checked"), "118 packs");
}

#[test]
fn check_server_cli_restore_stopped() {
    let summary = json!({"restore": {
        "snapshots_sampled": 1, "files_restored": 0, "bytes_restored": 0,
        "hash_mismatches": 0, "skipped": 20, "stopped_at_limit": true
    }});
    let mut run = run_dto("restore", "failed", json!("stopped"), summary, json!([]));
    run["error"] = json!("Stopped at the size limit.");
    let server = fake(run_and_report("restore", run, json!([])));
    let home = tempfile::tempdir().unwrap();
    let args = ["r1", "--level", "restore"];
    let (stdout, _) = output(&mut lode(&server, home.path(), &args), 1);
    assert_eq!(
        stdout.lines().next().unwrap(),
        "Test restore check for repository r1 (server)"
    );
    assert!(stdout.contains("Stopped at the size limit."), "{stdout}");
    assert!(stdout.contains("Files restored"), "{stdout}");
    assert!(stdout.contains("Hash mismatches"), "{stdout}");
}

#[test]
fn check_server_cli_json_full_report() {
    let findings: Vec<Value> = (0..25)
        .map(|i| json!({"kind": "missing_pack", "key": format!("data/{i}"), "detail": "gone"}))
        .collect();
    let mut run = run_dto(
        "structure",
        "failed",
        json!("problems"),
        structure_summary(),
        json!(findings[..20]),
    );
    run["findings_total"] = json!(25);
    let server = fake(run_and_report("structure", run, json!(findings)));
    let home = tempfile::tempdir().unwrap();
    let (stdout, _) = output(&mut lode(&server, home.path(), &["r1", "--json"]), 3);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "{stdout}");
    let doc: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(doc["type"], "check_report");
    assert_eq!(doc["run"]["outcome"], "problems");
    assert_eq!(doc["findings"].as_array().unwrap().len(), 25);
    assert!(doc["notes"].is_array());
}

#[test]
fn check_server_cli_cancelled() {
    let run = run_dto("structure", "aborted", Value::Null, json!({}), json!([]));
    let server = fake(vec![
        gc_probe(),
        started("structure"),
        ("GET /api/v1/repositories/r1/checks/runs/run1", ok(run)),
    ]);
    let home = tempfile::tempdir().unwrap();
    let (_, stderr) = output(&mut lode(&server, home.path(), &["r1"]), 1);
    assert!(stderr.contains("The check was cancelled."), "{stderr}");
}

#[test]
fn check_server_cli_scattered_poll_failures_do_not_add_up() {
    let done = run_dto(
        "structure",
        "done",
        json!("passed"),
        structure_summary(),
        json!([]),
    );
    let mut running = done.clone();
    running["state"] = json!("running");
    let (done_text, running_text) = (done.to_string(), running.to_string());
    let poll: Responder = Box::new(move |call, _| match call {
        7 => (200, done_text.clone()),
        n if n % 2 == 0 => (
            503,
            json!({"code": "unavailable", "message": "busy"}).to_string(),
        ),
        _ => (200, running_text.clone()),
    });
    let mut routes = run_and_report("structure", done, json!([]));
    routes.retain(|(key, _)| *key != "GET /api/v1/repositories/r1/checks/runs/run1");
    routes.push(("GET /api/v1/repositories/r1/checks/runs/run1", poll));
    let server = fake(routes);
    let home = tempfile::tempdir().unwrap();
    let (stdout, _) = output(&mut lode(&server, home.path(), &["r1"]), 0);
    assert!(stdout.contains("Passed"), "{stdout}");
}

#[test]
fn check_server_cli_packs_read_nothing() {
    let summary = json!({"packs": {
        "packs_read": 0, "bytes_read": 0, "blobs_verified": 0, "damaged_blobs": 0
    }});
    let run = run_dto("packs", "done", json!("passed"), summary, json!([]));
    let server = fake(run_and_report("packs", run, json!([])));
    let home = tempfile::tempdir().unwrap();
    let args = ["r1", "--level", "packs"];
    let (stdout, _) = output(&mut lode(&server, home.path(), &args), 0);
    assert!(stdout.contains("Nothing to check"), "{stdout}");
    assert!(!stdout.contains("Passed"), "{stdout}");
}
