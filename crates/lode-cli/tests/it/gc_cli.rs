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

fn ok(value: Value) -> Responder {
    status(200, value)
}

fn dynamic(f: impl Fn(usize, &Recorded) -> (u16, String) + Send + Sync + 'static) -> Responder {
    Box::new(f)
}

fn status(code: u16, value: Value) -> Responder {
    let text = value.to_string();
    Box::new(move |_, _| (code, text.clone()))
}

fn error(code: u16, error_code: &str, message: &str) -> Responder {
    status(code, json!({"code": error_code, "message": message}))
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

fn lode(server: &Fake, home: &Path) -> Command {
    let mut cmd = bare(home);
    cmd.env("LODESTORE_SERVER", &server.url)
        .env("LODESTORE_TOKEN", TOKEN)
        .arg("gc");
    cmd
}

fn lode_server(server: &Fake, home: &Path) -> Command {
    let mut cmd = bare(home);
    cmd.env("LODESTORE_SERVER", &server.url)
        .env("LODESTORE_TOKEN", TOKEN)
        .args(["server", "gc"]);
    cmd
}

fn bare(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("lode").unwrap();
    cmd.env_remove("LODESTORE_SERVER")
        .env_remove("LODESTORE_TOKEN")
        .env_remove("LODE_REPOSITORY")
        .env("XDG_CONFIG_HOME", home)
        .env("HOME", home);
    cmd
}

fn stdout(cmd: &mut Command) -> String {
    String::from_utf8(cmd.assert().success().get_output().stdout.clone()).unwrap()
}

fn failure(cmd: &mut Command, code: i32) -> String {
    String::from_utf8(cmd.assert().code(code).get_output().stderr.clone()).unwrap()
}

const AT: i64 = 1_792_843_200_000;

fn policies() -> (&'static str, Responder) {
    (
        "GET /api/v1/policies",
        ok(json!([
            {"name": "nightly", "repository_id": "r1", "repository_name": "main", "hosts": 2},
            {"name": "weekly", "repository_id": "r1", "repository_name": "main", "hosts": 1},
            {"name": "other", "repository_id": "r2", "repository_name": "second", "hosts": 1}
        ])),
    )
}

fn cycle(id: &str, state: &str, dry_run: bool) -> Value {
    json!({
        "id": id, "state": state, "dry_run": dry_run, "origin": "manual",
        "started_at": AT, "delete_after": null, "progress": null,
        "marked_packs": 0, "marked_bytes": 0, "blocked_by_writers": [],
        "waiting_for_restores": false, "error": null, "error_after_delete": false
    })
}

fn summary(id: &str, dry_run: bool, locked: u64, until: Option<i64>) -> Value {
    json!({
        "cycle_id": id, "dry_run": dry_run, "finished_at": AT, "duration_ms": 1000,
        "bytes_freed": 13_314_398_618u64, "packs_deleted": 318, "packs_repacked": 41,
        "repack_transfer_bytes": 9_019_431_322u64, "locked_bytes": locked,
        "locked_until": until, "locked_until_unknown": locked > 0 && until.is_none()
    })
}

fn gc_status(cycle: Value, last_dry_run: Value, last_cleanup: Value) -> Value {
    json!({
        "repository_id": "r1", "enabled": true,
        "settings": {}, "pause_floor": "24h", "schedule": null,
        "preflight": {"problems": [], "dry_run_allowed": true, "checked_at": AT},
        "has_object_lock": false, "cycle": cycle,
        "last_cleanup": last_cleanup, "last_dry_run": last_dry_run, "last_aborted_at": null,
        "events": [], "events_total": 0
    })
}

fn dry_run_routes(locked: u64, until: Option<i64>, waits: bool) -> Vec<(&'static str, Responder)> {
    let report = {
        let mut full = summary("c1", true, locked, until);
        full["delete"] = json!([{"id": "aa", "size": 10, "live_bytes": 0}]);
        full["repack"] = json!([{"id": "bb", "size": 10, "live_bytes": 3}]);
        full["ignored"] = json!([]);
        full["unindexed"] = json!([]);
        full["locked"] = json!([]);
        full
    };
    let done = gc_status(Value::Null, summary("c1", true, locked, until), Value::Null);
    let busy = gc_status(cycle("c1", "marking", true), Value::Null, Value::Null);
    vec![
        policies(),
        (
            "POST /api/v1/repositories/r1/gc/dry-run",
            status(202, json!({"cycle_id": "c1"})),
        ),
        (
            "GET /api/v1/repositories/r1/gc",
            dynamic(move |n, _| {
                let body = if waits && n == 0 { &busy } else { &done };
                (200, body.to_string())
            }),
        ),
        (
            "GET /api/v1/repositories/r1/gc/cycles/c1/report",
            ok(report),
        ),
    ]
}

#[test]
fn gc_cli_dry_run_text() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(dry_run_routes(0, None, true));
    let text = stdout(lode(&server, home.path()).args(["--dry-run", "main"]));
    assert_eq!(
        text,
        "Dry run for repository main\n\
         \x20 Would free           12.4 GiB\n\
         \x20 To delete            318 packs\n\
         \x20 To repack            41 packs\n\
         \x20 Repack transfer      8.4 GiB\n"
    );
    let post = server.only("POST", "/api/v1/repositories/r1/gc/dry-run");
    assert_eq!(post.auth.as_deref(), Some("Bearer lsk_test_token"));
    server.only("GET", "/api/v1/policies");
    server.only("GET", "/api/v1/repositories/r1/gc/cycles/c1/report");
}

#[test]
fn gc_cli_dry_run_survives_a_failed_poll() {
    let home = tempfile::tempdir().unwrap();
    let mut routes = dry_run_routes(0, None, false);
    let done = gc_status(Value::Null, summary("c1", true, 0, None), Value::Null);
    routes.retain(|(key, _)| *key != "GET /api/v1/repositories/r1/gc");
    routes.push((
        "GET /api/v1/repositories/r1/gc",
        dynamic(move |n, _| {
            if n == 0 {
                (
                    503,
                    json!({"code": "unavailable", "message": "later"}).to_string(),
                )
            } else {
                (200, done.to_string())
            }
        }),
    ));
    let server = fake(routes);
    let text = stdout(lode(&server, home.path()).args(["--dry-run", "main"]));
    assert!(text.starts_with("Dry run for repository main\n"), "{text}");
}

#[test]
fn gc_cli_dry_run_object_lock_line() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(dry_run_routes(3_435_973_837, Some(AT), false));
    let text = stdout(lode(&server, home.path()).args(["--dry-run", "main"]));
    assert!(
        text.ends_with("  Held by Object Lock  3.2 GiB, frees after 24 Oct 2026\n"),
        "{text}"
    );

    let server = fake(dry_run_routes(3_435_973_837, None, false));
    let text = stdout(lode(&server, home.path()).args(["--dry-run", "main"]));
    assert!(
        text.ends_with("  Held by Object Lock  3.2 GiB, release date unknown\n"),
        "{text}"
    );
}

#[test]
fn gc_cli_dry_run_json() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(dry_run_routes(0, None, false));
    let text = stdout(lode(&server, home.path()).args(["--json", "--dry-run", "main"]));
    assert_eq!(text.lines().count(), 1, "{text}");
    let report: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(report["v"], 1);
    assert_eq!(report["type"], "gc_report");
    assert_eq!(report["delete"][0]["id"], "aa");
    assert_eq!(report["repack"][0]["id"], "bb");
    assert_eq!(report["bytes_freed"], 13_314_398_618u64);
}

#[test]
fn gc_cli_dry_run_failed() {
    let home = tempfile::tempdir().unwrap();
    let mut failed = gc_status(Value::Null, Value::Null, Value::Null);
    failed["events"] = json!([
        {"seq": 1, "at": AT, "level": "info", "kind": "started", "message": "Dry run started."},
        {"seq": 2, "at": AT, "level": "error", "kind": "failed", "message": "opening the repository: access denied"}
    ]);
    let server = fake(vec![
        policies(),
        (
            "POST /api/v1/repositories/r1/gc/dry-run",
            status(202, json!({"cycle_id": "c1"})),
        ),
        ("GET /api/v1/repositories/r1/gc", ok(failed)),
    ]);
    let err = failure(lode(&server, home.path()).args(["--dry-run", "main"]), 1);
    assert_eq!(
        err.trim(),
        "error: Dry run failed: opening the repository: access denied"
    );
}

#[test]
fn gc_cli_server_alias() {
    let home = tempfile::tempdir().unwrap();
    let via_gc = fake(dry_run_routes(0, None, false));
    let via_server = fake(dry_run_routes(0, None, false));
    let a = stdout(lode(&via_gc, home.path()).args(["--dry-run", "main"]));
    let b = stdout(lode_server(&via_server, home.path()).args(["--dry-run", "main"]));
    assert_eq!(a, b);
    let paths = |f: &Fake| -> Vec<String> {
        f.requests()
            .into_iter()
            .map(|r| format!("{} {}", r.method, r.path))
            .collect()
    };
    assert_eq!(paths(&via_gc), paths(&via_server));
}

#[test]
fn gc_cli_name_or_id() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(dry_run_routes(0, None, false));
    stdout(lode(&server, home.path()).args(["--dry-run", "r1"]));
    assert!(
        server
            .requests()
            .iter()
            .all(|r| r.path != "/api/v1/policies")
    );

    let err = failure(lode(&server, home.path()).args(["--dry-run", "nobody"]), 2);
    assert!(err.contains("Unknown repository \"nobody\""), "{err}");
}

#[test]
fn gc_cli_run_until_waiting() {
    let home = tempfile::tempdir().unwrap();
    let mut waiting = cycle("c1", "waiting", false);
    waiting["marked_packs"] = json!(12);
    waiting["marked_bytes"] = json!(1_288_490_189u64);
    waiting["delete_after"] = json!(AT);
    let server = fake(vec![
        policies(),
        (
            "POST /api/v1/repositories/r1/gc/run",
            status(202, json!({"cycle_id": "c1"})),
        ),
        (
            "GET /api/v1/repositories/r1/gc",
            ok(gc_status(waiting, Value::Null, Value::Null)),
        ),
    ]);
    let text = stdout(lode(&server, home.path()).arg("main"));
    assert_eq!(
        text,
        "Marked 12 packs (1.2 GiB). Deletion after 2026-10-24 12:00:00 UTC.\n"
    );
    server.only("POST", "/api/v1/repositories/r1/gc/run");
}

#[test]
fn gc_cli_run_conflict() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(vec![
        policies(),
        (
            "POST /api/v1/repositories/r1/gc/run",
            status(
                409,
                json!({
                    "message": "Cleanup can't run yet.",
                    "problems": [
                        {"kind": "server_key", "message": "The server's storage key cannot delete objects.", "hosts": []},
                        {"kind": "old_agents", "message": "2 hosts run an agent that does not support cleanup.",
                         "hosts": [{"id": "h1", "hostname": "db-1"}, {"id": "h2", "hostname": "web-1"}]}
                    ]
                }),
            ),
        ),
    ]);
    let err = failure(lode(&server, home.path()).arg("main"), 1);
    assert_eq!(
        err.trim(),
        "error: Cleanup can't run yet.\n\
         - The server's storage key cannot delete objects.\n\
         - 2 hosts run an agent that does not support cleanup. (db-1, web-1)"
    );

    let server = fake(vec![
        policies(),
        (
            "POST /api/v1/repositories/r1/gc/run",
            error(
                409,
                "conflict",
                "Turn on cleanup for this repository first.",
            ),
        ),
    ]);
    let err = failure(lode(&server, home.path()).arg("main"), 1);
    assert_eq!(
        err.trim(),
        "error: Turn on cleanup for this repository first."
    );
}

fn postponed() -> Value {
    let mut waiting = cycle("c1", "waiting", false);
    waiting["marked_packs"] = json!(12);
    waiting["marked_bytes"] = json!(1_288_490_189u64);
    waiting["delete_after"] = json!(AT);
    waiting["blocked_by_writers"] = json!([
        {"key": "locks/writer-aa", "host": "host-a", "started_at": 1_600_000_000_000i64},
        {"key": "locks/writer-bb", "host": "host-b", "started_at": null}
    ]);
    gc_status(waiting, Value::Null, summary("c0", false, 0, None))
}

#[test]
fn gc_cli_status_postponed() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(vec![
        policies(),
        ("GET /api/v1/repositories/r1/gc", ok(postponed())),
    ]);
    let text = stdout(lode(&server, home.path()).args(["status", "main"]));
    assert!(text.starts_with("Deletion is postponed\n"), "{text}");
    assert!(
        text.contains("  host-a · lode backup · started 2020-09-13 12:26:40 UTC\n"),
        "{text}"
    );
    assert!(
        text.contains("  host-b · lode backup · started -\n"),
        "{text}"
    );
    assert!(
        text.contains("Last cleanup, finished 2026-10-24 12:00:00 UTC\n"),
        "{text}"
    );
    assert!(text.contains("  Freed                12.4 GiB\n"), "{text}");
}

#[test]
fn gc_cli_status_idle() {
    let home = tempfile::tempdir().unwrap();
    let mut idle = gc_status(Value::Null, Value::Null, Value::Null);
    idle["schedule"] = json!({"summary": "weekly", "next_runs": [AT]});
    let server = fake(vec![
        policies(),
        ("GET /api/v1/repositories/r1/gc", ok(idle)),
    ]);
    let text = stdout(lode(&server, home.path()).args(["status", "main"]));
    assert_eq!(text, "Idle\n  Next run 2026-10-24 12:00:00 UTC.\n");

    let mut off = gc_status(Value::Null, Value::Null, Value::Null);
    off["enabled"] = json!(false);
    let server = fake(vec![
        policies(),
        ("GET /api/v1/repositories/r1/gc", ok(off)),
    ]);
    let text = stdout(lode(&server, home.path()).args(["status", "main"]));
    assert_eq!(
        text,
        "Cleanup is off\n  Nothing is deleted from this repository.\n"
    );
}

fn unlock_routes(status_body: Value) -> Vec<(&'static str, Responder)> {
    vec![
        policies(),
        ("GET /api/v1/repositories/r1/gc", ok(status_body)),
        (
            "POST /api/v1/repositories/r1/gc/unlock",
            ok(json!({"unlocked": 2})),
        ),
    ]
}

#[test]
fn gc_cli_unlock_requires_yes() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(unlock_routes(postponed()));
    let err = failure(
        lode(&server, home.path())
            .args(["unlock", "main"])
            .write_stdin(""),
        2,
    );
    assert!(
        err.contains("Use --yes to unlock without a prompt."),
        "{err}"
    );
    assert!(server.requests().iter().all(|r| r.method != "POST"));
}

#[test]
fn gc_cli_unlock_yes() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(unlock_routes(postponed()));
    let text = stdout(lode(&server, home.path()).args(["unlock", "--yes", "main"]));
    assert_eq!(
        text,
        "Cleanup stops waiting for: host-a, host-b. If one of these backups is still running, \
its snapshot can lose data and fail to restore. Unlock only backups you know were interrupted.\n\
Unlocked 2 markers. Cleanup continues.\n"
    );
    server.only("POST", "/api/v1/repositories/r1/gc/unlock");
}

#[test]
fn gc_cli_unlock_nothing() {
    let home = tempfile::tempdir().unwrap();
    let idle = gc_status(Value::Null, Value::Null, Value::Null);
    let server = fake(unlock_routes(idle));
    let text = stdout(lode(&server, home.path()).args(["unlock", "main"]));
    assert_eq!(text, "There are no markers to unlock.\n");
    assert!(server.requests().iter().all(|r| r.method != "POST"));
}
