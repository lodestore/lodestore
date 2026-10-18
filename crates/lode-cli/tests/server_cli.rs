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
    query: String,
    auth: Option<String>,
    body: String,
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

    /// The requests without the host lookup that turns a hostname into an id.
    fn calls(&self) -> Vec<Recorded> {
        let mut log = self.requests();
        log.retain(|r| r.path != "/api/v1/hosts");
        log
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
    let body = to_bytes(body, 1 << 20).await.unwrap();
    let recorded = Recorded {
        method: parts.method.to_string(),
        path: parts.uri.path().to_owned(),
        query: parts.uri.query().unwrap_or_default().to_owned(),
        auth: parts
            .headers
            .get("authorization")
            .map(|v| v.to_str().unwrap().to_owned()),
        body: String::from_utf8(body.to_vec()).unwrap(),
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
        .arg("server");
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

fn hosts_routes() -> Vec<(&'static str, Responder)> {
    vec![
        (
            "GET /api/v1/hosts",
            ok(json!([
                {
                    "id": "h1", "hostname": "db-1", "status": "online", "repository_id": "r1",
                    "labels": [{"key": "env", "value": "prod"}, {"key": "role", "value": "db"}],
                    "last_seen_at": 1_600_000_000_000i64, "version": "0.1.0", "os": "linux",
                    "arch": "x86_64", "distro": null, "cert_not_after": 1, "enrolled_at": 1
                },
                {
                    "id": "h2", "hostname": "web-1", "status": "revoked", "repository_id": "r1",
                    "labels": [], "last_seen_at": null, "version": "0.1.0", "os": "linux",
                    "arch": "aarch64", "distro": null, "cert_not_after": 1, "enrolled_at": 1
                }
            ])),
        ),
        (
            "GET /api/v1/restore/hosts",
            ok(json!([
                {"key": "h1", "host_id": "h1", "hostname": "db-1", "agent": true,
                 "repository_name": "main", "latest_snapshot_ms": 1_600_000_100_000i64,
                 "snapshot_count": 3, "labels": []}
            ])),
        ),
    ]
}

#[test]
fn server_cli_hosts_list_env() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(hosts_routes());
    let text = stdout(lode(&server, home.path()).args(["hosts", "list"]));
    assert_eq!(
        text,
        "HOST   ID  STATUS   REPOSITORY  LABELS            LAST BACKUP\n\
         db-1   h1  Online   main        env=prod,role=db  2020-09-13 12:28:20\n\
         web-1  h2  Revoked  r1          -                 -\n"
    );
    let request = server.only("GET", "/api/v1/hosts");
    assert_eq!(request.auth.as_deref(), Some("Bearer lsk_test_token"));
    assert_eq!(request.query, "");
}

#[test]
fn server_cli_flag_overrides_env() {
    let home = tempfile::tempdir().unwrap();
    let wrong = fake(vec![]);
    let right = fake(hosts_routes());
    let mut cmd = lode(&wrong, home.path());
    cmd.args(["--server", &right.url, "hosts", "list"]);
    stdout(&mut cmd);
    assert!(wrong.requests().is_empty());
    assert_eq!(right.requests().len(), 2);
}

#[test]
fn server_cli_profile_supplies_server_and_token() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(hosts_routes());
    std::fs::create_dir_all(home.path().join("lode")).unwrap();
    std::fs::write(
        home.path().join("lode/config.toml"),
        format!(
            "[profiles.ci]\nserver = \"{}\"\ntoken = \"lsk_from_profile\"\n",
            server.url
        ),
    )
    .unwrap();
    stdout(bare(home.path()).args(["--profile", "ci", "server", "hosts", "list"]));
    assert_eq!(
        server.only("GET", "/api/v1/hosts").auth.as_deref(),
        Some("Bearer lsk_from_profile")
    );
}

#[test]
fn server_cli_warns_when_the_profile_with_a_token_is_open() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().unwrap();
    let server = fake(hosts_routes());
    std::fs::create_dir_all(home.path().join("lode")).unwrap();
    let config = home.path().join("lode/config.toml");
    std::fs::write(
        &config,
        format!(
            "[profiles.ci]\nserver = \"{}\"\ntoken = \"lsk_from_profile\"\n",
            server.url
        ),
    )
    .unwrap();
    let run = || {
        let output = bare(home.path())
            .args(["--profile", "ci", "server", "hosts", "list"])
            .assert()
            .success()
            .get_output()
            .clone();
        String::from_utf8(output.stderr).unwrap()
    };

    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(run().contains("readable by others"));
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(!run().contains("readable by others"));
}

#[test]
fn server_cli_missing_config_exit_2() {
    let home = tempfile::tempdir().unwrap();
    let err = failure(bare(home.path()).args(["server", "hosts", "list"]), 2);
    assert!(
        err.contains(
            "Set LODESTORE_SERVER and LODESTORE_TOKEN, or add server and token to the profile."
        ),
        "{err}"
    );
    let server = fake(vec![]);
    let err = failure(
        bare(home.path())
            .env("LODESTORE_SERVER", &server.url)
            .args(["server", "hosts", "list"]),
        2,
    );
    assert!(err.contains("Set LODESTORE_SERVER"), "{err}");
}

#[test]
fn server_cli_token_refused_exit_13() {
    let home = tempfile::tempdir().unwrap();
    let unauthorized = fake(vec![(
        "GET /api/v1/hosts",
        error(401, "unauthorized", "authentication required"),
    )]);
    let err = failure(lode(&unauthorized, home.path()).args(["hosts", "list"]), 13);
    assert!(
        err.contains("The token was refused: it is unknown, expired or revoked."),
        "{err}"
    );
    assert!(!err.contains(TOKEN));

    let forbidden = fake(vec![(
        "GET /api/v1/hosts",
        error(403, "forbidden", "The token needs the hosts:read scope."),
    )]);
    let err = failure(lode(&forbidden, home.path()).args(["hosts", "list"]), 13);
    assert!(
        err.contains("The token was refused: it needs the hosts:read scope."),
        "{err}"
    );
}

#[test]
fn server_cli_server_error_exit_1() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(vec![(
        "GET /api/v1/hosts/nope",
        error(404, "not_found", "host not found"),
    )]);
    let err = failure(
        lode(&server, home.path()).args(["hosts", "show", "nope"]),
        1,
    );
    assert!(err.contains("host not found"), "{err}");
}

#[test]
fn server_cli_hosts_json() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(hosts_routes());
    let text = stdout(lode(&server, home.path()).args(["--json", "hosts", "list"]));
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["v"], 1);
    assert_eq!(lines[0]["type"], "host");
    assert_eq!(lines[0]["hostname"], "db-1");
    assert_eq!(lines[0]["repository"], "main");
    assert_eq!(lines[1]["repository"], Value::Null);
    assert!(!text.contains(TOKEN));
}

#[test]
fn server_cli_hosts_label_replaces_labels() {
    let home = tempfile::tempdir().unwrap();
    let detail = json!({
        "id": "h1", "hostname": "db-1", "status": "online", "repository_id": "r1",
        "repository_name": "main", "labels": [{"key": "env", "value": "prod"}],
        "version": "0.1.0", "os": "linux", "arch": "x86_64", "cert_not_after": 1,
        "enrolled_at": 1
    });
    let server = fake(vec![("PUT /api/v1/hosts/h1/labels", ok(detail))]);
    let text = stdout(lode(&server, home.path()).args(["hosts", "label", "h1", "env=prod"]));
    assert!(text.contains("Labels:     env=prod"), "{text}");
    let body: Value =
        serde_json::from_str(&server.only("PUT", "/api/v1/hosts/h1/labels").body).unwrap();
    assert_eq!(body, json!({"labels": [{"key": "env", "value": "prod"}]}));
    failure(
        lode(&server, home.path()).args(["hosts", "label", "h1", "oops"]),
        2,
    );
}

#[test]
fn server_cli_hosts_label_takes_a_hostname() {
    let home = tempfile::tempdir().unwrap();
    let detail = json!({
        "id": "h1", "hostname": "db-1", "status": "online", "repository_id": "r1",
        "repository_name": "main", "labels": [{"key": "env", "value": "dev"}],
        "version": "0.1.0", "os": "linux", "arch": "x86_64", "cert_not_after": 1,
        "enrolled_at": 1
    });
    let mut routes = hosts_routes();
    routes.push(("PUT /api/v1/hosts/h1/labels", ok(detail)));
    let server = fake(routes);
    let text = stdout(lode(&server, home.path()).args(["hosts", "label", "db-1", "env=dev"]));
    assert!(text.contains("Repository: main"), "{text}");
    server.only("PUT", "/api/v1/hosts/h1/labels");
}

#[test]
fn server_cli_hostname_skips_revoked_hosts() {
    let home = tempfile::tempdir().unwrap();
    let hosts = json!([
        {"id": "old", "hostname": "db-1", "status": "revoked", "repository_id": "r1", "labels": []},
        {"id": "new", "hostname": "db-1", "status": "online", "repository_id": "r1", "labels": []}
    ]);
    let detail = json!({
        "id": "new", "hostname": "db-1", "status": "online", "repository_id": "r1",
        "repository_name": "main", "labels": [], "version": "0.1.0", "os": "linux",
        "arch": "x86_64", "cert_not_after": 1, "enrolled_at": 1
    });
    let server = fake(vec![
        ("GET /api/v1/hosts", ok(hosts)),
        ("GET /api/v1/hosts/new", ok(detail)),
    ]);
    let text = stdout(lode(&server, home.path()).args(["hosts", "show", "db-1"]));
    assert!(text.contains("Id:         new"), "{text}");
}

const POLICY_YAML: &str =
    "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: nightly\n";

fn job_json(id: &str, state: &str) -> Value {
    json!({
        "id": id, "kind": "backup", "host_id": "h1", "hostname": "db-1",
        "policy_name": "nightly", "state": state, "queued_at": 1_600_000_000_000i64,
        "started_at": null, "finished_at": null, "snapshot_id": null,
        "warnings_total": 0, "errors_total": 0, "error_message": null
    })
}

#[test]
fn server_cli_policies_list_and_get() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(vec![
        (
            "GET /api/v1/policies",
            ok(json!([
                {
                    "name": "nightly", "repository_id": "r1", "repository_name": "main",
                    "hosts": 4, "last_job": {"id": "j1", "state": "warnings",
                    "queued_at": 1_600_000_000_000i64, "finished_at": null}
                },
                {"name": "weekly", "repository_id": "r1", "repository_name": "main",
                 "hosts": 0, "last_job": null}
            ])),
        ),
        (
            "GET /api/v1/policies/nightly",
            ok(json!({"name": "nightly", "revision": 3, "yaml": POLICY_YAML})),
        ),
    ]);
    let text = stdout(lode(&server, home.path()).args(["policies", "list"]));
    assert_eq!(
        text,
        "NAME     REPOSITORY  HOSTS  LAST RUN\n\
         nightly  main        4      2020-09-13 12:26:40 Completed with warnings\n\
         weekly   main        0      -\n"
    );
    let yaml = stdout(lode(&server, home.path()).args(["policies", "get", "nightly"]));
    assert_eq!(yaml, POLICY_YAML);
    let json_text = stdout(lode(&server, home.path()).args(["--json", "policies", "list"]));
    let first: Value = serde_json::from_str(json_text.lines().next().unwrap()).unwrap();
    assert_eq!(first["type"], "policy");
    assert_eq!(first["name"], "nightly");
}

#[test]
fn server_cli_policy_delete_needs_yes() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(vec![(
        "DELETE /api/v1/policies/nightly",
        dynamic(|_, _| (204, String::new())),
    )]);
    let err = failure(
        lode(&server, home.path()).args(["policies", "delete", "nightly"]),
        2,
    );
    assert!(
        err.contains("Use --yes to delete without a prompt."),
        "{err}"
    );
    assert!(server.requests().is_empty());
    let text = stdout(lode(&server, home.path()).args(["policies", "delete", "nightly", "--yes"]));
    assert_eq!(text, "Policy nightly deleted.\n");
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn server_cli_policy_run_coalesced() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(vec![(
        "POST /api/v1/policies/nightly/run",
        dynamic(|call, _| {
            let body = json!({"jobs": [
                {"id": "j1", "host_id": "h1", "hostname": "db-1", "created": call == 0}
            ]});
            (200, body.to_string())
        }) as Responder,
    )]);
    let first = stdout(lode(&server, home.path()).args(["policies", "run", "nightly"]));
    assert_eq!(first, "Queued job j1 on db-1\n");
    let second = stdout(lode(&server, home.path()).args(["policies", "run", "nightly"]));
    assert_eq!(second, "Job j1 on db-1 is already queued\n");
    let json_text =
        stdout(lode(&server, home.path()).args(["--json", "policies", "run", "nightly"]));
    let line: Value = serde_json::from_str(json_text.trim()).unwrap();
    assert_eq!(line["type"], "queued");
    assert_eq!(line["created"], false);
}

#[test]
fn server_cli_jobs_list_filters() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(vec![(
        "GET /api/v1/jobs",
        ok(json!({
            "items": [job_json("j1", "running"), job_json("j2", "not_started")],
            "total": 2, "page": 1, "page_size": 50
        })),
    )]);
    let text = stdout(
        lode(&server, home.path())
            .args(["jobs", "list", "--state", "running", "--policy", "nightly"]),
    );
    assert_eq!(
        text,
        "JOB  HOST  POLICY   STATE        QUEUED\n\
         j1   db-1  nightly  Running      2020-09-13 12:26:40\n\
         j2   db-1  nightly  Not started  2020-09-13 12:26:40\n"
    );
    assert_eq!(
        server.only("GET", "/api/v1/jobs").query,
        "status=running&policy=nightly"
    );
}

#[test]
fn server_cli_jobs_logs_follow() {
    let home = tempfile::tempdir().unwrap();
    let event = |seq: i64, message: &str| {
        json!({"seq": seq, "at": 1_600_000_000_000i64 + seq * 1000, "level": "warning",
               "kind": "file", "path": null, "message": message})
    };
    let server = fake(vec![
        (
            "GET /api/v1/jobs/j1/events",
            dynamic(move |call, request| {
                let body = match call {
                    0 => json!({"events": [event(1, "first"), event(2, "second")], "last_seq": 2}),
                    1 => json!({"events": [event(3, "third")], "last_seq": 3}),
                    _ => json!({"events": [], "last_seq": 3}),
                };
                assert!(request.query.starts_with("after="));
                (200, body.to_string())
            }) as Responder,
        ),
        (
            "GET /api/v1/jobs/j1",
            dynamic(|call, _| {
                let state = if call == 0 { "running" } else { "succeeded" };
                (200, job_json("j1", state).to_string())
            }) as Responder,
        ),
    ]);
    let text = stdout(lode(&server, home.path()).args(["jobs", "logs", "j1", "--follow"]));
    assert_eq!(text.lines().count(), 3, "{text}");
    for message in ["first", "second", "third"] {
        assert_eq!(text.matches(message).count(), 1, "{text}");
    }
    let afters: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|r| r.path.ends_with("/events"))
        .map(|r| r.query)
        .collect();
    assert_eq!(afters, ["after=0", "after=2", "after=3"]);
}

#[test]
fn server_cli_jobs_actions() {
    let home = tempfile::tempdir().unwrap();
    let queued =
        json!({"jobs": [{"id": "j9", "host_id": "h1", "hostname": "db-1", "created": true}]});
    let server = fake(vec![
        ("POST /api/v1/hosts/h1/run", ok(queued)),
        (
            "POST /api/v1/jobs/j1/cancel",
            ok(job_json("j1", "cancelled")),
        ),
        ("POST /api/v1/jobs/j2/cancel", ok(job_json("j2", "running"))),
        (
            "POST /api/v1/jobs/j1/rerun",
            ok(json!({"id": "j10", "host_id": "h1", "hostname": "db-1", "created": true})),
        ),
    ]);
    let text = stdout(lode(&server, home.path()).args([
        "jobs", "run", "--host", "h1", "--policy", "a", "--policy", "b",
    ]));
    assert_eq!(text, "Queued job j9 on db-1\n");
    let body: Value =
        serde_json::from_str(&server.only("POST", "/api/v1/hosts/h1/run").body).unwrap();
    assert_eq!(body, json!({"policies": ["a", "b"]}));
    assert_eq!(
        stdout(lode(&server, home.path()).args(["jobs", "cancel", "j1"])),
        "Job j1 cancelled.\n"
    );
    assert_eq!(
        stdout(lode(&server, home.path()).args(["jobs", "cancel", "j2"])),
        "Cancel requested for job j2.\n"
    );
    assert_eq!(
        stdout(lode(&server, home.path()).args(["jobs", "rerun", "j1"])),
        "Queued job j10 on db-1\n"
    );
}

const SNAPSHOT_ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn snapshot_json(id: &str, incomplete: bool) -> Value {
    json!({
        "id": id, "time": 1_600_000_000_000i64, "hostname": "db-1",
        "policy_name": "nightly", "stdin_name": null, "incomplete": incomplete,
        "stats": {"files_new": 1, "files_changed": 2, "files_unchanged": 3,
                  "dirs": 1, "bytes_total": 3_145_728, "bytes_read": 0, "bytes_added": 0},
        "repository_id": "r1", "host_key": "h1"
    })
}

#[test]
fn server_cli_snapshots_list_pages() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(vec![(
        "GET /api/v1/restore/snapshots",
        dynamic(|call, request| {
            let body = if call == 0 {
                assert_eq!(request.query, "host=h1&limit=200");
                json!({"items": [snapshot_json("aa", false)], "total": 2, "next": "cursor-1"})
            } else {
                assert_eq!(request.query, "host=h1&limit=200&before=cursor-1");
                json!({"items": [snapshot_json("bb", true)], "total": 2, "next": null})
            };
            (200, body.to_string())
        }),
    )]);
    let text = stdout(lode(&server, home.path()).args(["snapshots", "list", "--host", "h1"]));
    assert_eq!(
        text,
        "SNAPSHOT  TIME                 POLICY   FILES  SIZE     STATUS\n\
         aa        2020-09-13 12:26:40  nightly  6      3.0 MiB  Complete\n\
         bb        2020-09-13 12:26:40  nightly  6      3.0 MiB  Incomplete\n"
    );
    assert_eq!(server.calls().len(), 2);
}

#[test]
fn server_cli_snapshots_list_limit_stops_early() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(vec![(
        "GET /api/v1/restore/snapshots",
        ok(json!({"items": [snapshot_json("aa", false)], "total": 9, "next": "cursor-1"})),
    )]);
    let json_text = stdout(lode(&server, home.path()).args([
        "--json",
        "snapshots",
        "list",
        "--host",
        "h1",
        "--limit",
        "1",
    ]));
    assert_eq!(json_text.lines().count(), 1);
    assert_eq!(server.calls().len(), 1);
    assert_eq!(server.calls()[0].query, "host=h1&limit=1");
}

#[test]
fn server_cli_restore_defaults() {
    let home = tempfile::tempdir().unwrap();
    let snapshot_route = format!("GET /api/v1/snapshots/{SNAPSHOT_ID}");
    let server = fake(vec![
        (
            "POST /api/v1/restores",
            status(201, json!({"job_id": "job-7"})),
        ),
        (&snapshot_route, ok(snapshot_json(SNAPSHOT_ID, false))),
    ]);
    let text = stdout(lode(&server, home.path()).args(["restore", SNAPSHOT_ID, "--host", "h2"]));
    assert_eq!(text, "Restore queued: job job-7\n");
    let body: Value = serde_json::from_str(&server.only("POST", "/api/v1/restores").body).unwrap();
    assert_eq!(
        body,
        json!({
            "snapshot_id": SNAPSHOT_ID,
            "items": ["Lw"],
            "target_host_id": "h2",
            "destination": "/var/lodestore/restore/2020-09-13-01234567",
            "in_place": false,
            "conflict": "overwrite_changed",
            "restore_owners": true
        })
    );
}

#[test]
fn server_cli_restore_options() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(vec![(
        "POST /api/v1/restores",
        status(201, json!({"job_id": "job-8"})),
    )]);
    stdout(lode(&server, home.path()).args([
        "restore",
        SNAPSHOT_ID,
        "--host",
        "h2",
        "--path",
        "/etc",
        "--path",
        "/var/www",
        "--conflict",
        "skip-existing",
        "--target",
        "/srv/r",
        "--no-owners",
    ]));
    let body: Value = serde_json::from_str(&server.only("POST", "/api/v1/restores").body).unwrap();
    assert_eq!(body["items"], json!(["L2V0Yw", "L3Zhci93d3c"]));
    assert_eq!(body["conflict"], "skip_existing");
    assert_eq!(body["destination"], "/srv/r");
    assert_eq!(body["restore_owners"], false);
    assert_eq!(server.calls().len(), 1);
}

#[test]
fn server_cli_restore_in_place_needs_yes() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(vec![(
        "POST /api/v1/restores",
        status(201, json!({"job_id": "job-9"})),
    )]);
    let err = failure(
        lode(&server, home.path()).args(["restore", SNAPSHOT_ID, "--host", "h2", "--in-place"]),
        2,
    );
    assert!(err.contains("Use --yes to restore in place."), "{err}");
    assert!(server.requests().is_empty());
    stdout(lode(&server, home.path()).args([
        "restore",
        SNAPSHOT_ID,
        "--host",
        "h2",
        "--in-place",
        "--yes",
    ]));
    let body: Value = serde_json::from_str(&server.only("POST", "/api/v1/restores").body).unwrap();
    assert_eq!(body["in_place"], true);
    assert_eq!(body["destination"], "/");
}

#[test]
fn server_cli_restore_needs_full_snapshot_id() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(vec![]);
    failure(
        lode(&server, home.path()).args(["restore", "0123abcd", "--host", "h2"]),
        2,
    );
    assert!(server.requests().is_empty());
}

fn suspicious_route() -> Vec<(&'static str, Responder)> {
    vec![(
        "PUT /api/v1/snapshots/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef/suspicious",
        dynamic(|_, request| {
            let body: Value = serde_json::from_str(&request.body).unwrap();
            let mut item = snapshot_json(SNAPSHOT_ID, false);
            if body["suspicious"] == true {
                item["suspicious"] = json!({"automatic": false, "reasons": [],
                    "note": body["reason"], "marked_by": "ci", "marked_at": 1});
            }
            (200, item.to_string())
        }),
    )]
}

#[test]
fn server_cli_snapshot_mark_and_clear() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(suspicious_route());
    let text = stdout(lode(&server, home.path()).args([
        "snapshots",
        "mark",
        SNAPSHOT_ID,
        "--reason",
        "ransom note found",
    ]));
    assert_eq!(text, "Snapshot 01234567 marked as suspicious.\n");
    let text = stdout(lode(&server, home.path()).args([
        "snapshots",
        "clear",
        SNAPSHOT_ID,
        "--reason",
        "restored and checked",
    ]));
    assert_eq!(text, "Suspicious mark cleared on snapshot 01234567.\n");
    let bodies: Vec<Value> = server
        .requests()
        .iter()
        .map(|r| serde_json::from_str(&r.body).unwrap())
        .collect();
    assert_eq!(
        bodies[0],
        json!({"suspicious": true, "reason": "ransom note found"})
    );
    assert_eq!(
        bodies[1],
        json!({"suspicious": false, "reason": "restored and checked"})
    );
    assert_eq!(server.requests()[0].method, "PUT");

    failure(
        lode(&server, home.path()).args(["snapshots", "mark", SNAPSHOT_ID]),
        2,
    );
    failure(
        lode(&server, home.path()).args(["snapshots", "mark", "0123abcd", "--reason", "x"]),
        2,
    );
    assert_eq!(server.requests().len(), 2);
}

fn delete_route(answer: Value) -> Vec<(&'static str, Responder)> {
    vec![(
        "DELETE /api/v1/snapshots/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ok(answer),
    )]
}

#[test]
fn server_cli_snapshot_delete_needs_yes() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(delete_route(
        json!({"id": SNAPSHOT_ID, "storage_kept": false, "reason": null}),
    ));
    let err = failure(
        lode(&server, home.path()).args(["snapshots", "delete", SNAPSHOT_ID]),
        2,
    );
    assert!(
        err.contains("Use --yes to delete without a prompt."),
        "{err}"
    );
    assert!(server.requests().is_empty());
    let text =
        stdout(lode(&server, home.path()).args(["snapshots", "delete", SNAPSHOT_ID, "--yes"]));
    assert_eq!(text, "Snapshot 01234567 deleted.\n");
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn server_cli_snapshot_delete_object_lock() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(delete_route(
        json!({"id": SNAPSHOT_ID, "storage_kept": true, "reason": "locked until 2030"}),
    ));
    let mut cmd = lode(&server, home.path());
    cmd.args(["snapshots", "delete", SNAPSHOT_ID, "--yes"]);
    let output = cmd.assert().success().get_output().clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains(
            "Snapshot 01234567 was removed from the catalog, but storage kept the file because of Object Lock. It stays out of Restore."
        ),
        "{stderr}"
    );
    assert!(
        !String::from_utf8(output.stdout)
            .unwrap()
            .contains("deleted.")
    );
}

#[test]
fn server_cli_snapshot_actions_scope_exit_13() {
    let home = tempfile::tempdir().unwrap();
    let denied = || {
        error(
            403,
            "forbidden",
            "The token needs the snapshots:delete scope.",
        )
    };
    let server = fake(vec![
        (
            "DELETE /api/v1/snapshots/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            denied(),
        ),
        (
            "PUT /api/v1/snapshots/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef/suspicious",
            denied(),
        ),
    ]);
    let err = failure(
        lode(&server, home.path()).args(["snapshots", "delete", SNAPSHOT_ID, "--yes"]),
        13,
    );
    assert!(err.contains("snapshots:delete"), "{err}");
    let err = failure(
        lode(&server, home.path()).args(["snapshots", "mark", SNAPSHOT_ID, "--reason", "x"]),
        13,
    );
    assert!(err.contains("snapshots:delete"), "{err}");
}

#[test]
fn server_cli_snapshots_list_statuses() {
    let home = tempfile::tempdir().unwrap();
    let mut suspicious = snapshot_json("aa", false);
    suspicious["suspicious"] = json!({"automatic": true, "reasons": [], "note": null,
        "marked_by": "anomaly", "marked_at": 1});
    let mut orphan = snapshot_json("bb", true);
    orphan["policy_deleted"] = json!(true);
    let mut all = snapshot_json("cc", true);
    all["policy_deleted"] = json!(true);
    all["suspicious"] = suspicious["suspicious"].clone();
    let server = fake(vec![(
        "GET /api/v1/restore/snapshots",
        ok(json!({"items": [suspicious, orphan, all], "total": 3, "next": null})),
    )]);
    let text = stdout(lode(&server, home.path()).args(["snapshots", "list", "--host", "h1"]));
    assert!(
        text.contains("aa        2020-09-13 12:26:40  nightly  6      3.0 MiB  Suspicious\n"),
        "{text}"
    );
    assert!(text.contains("Incomplete, Policy deleted\n"), "{text}");
    assert!(
        text.contains("Suspicious, Incomplete, Policy deleted\n"),
        "{text}"
    );
}

#[test]
fn server_cli_policy_retention_preview() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(vec![
        (
            "GET /api/v1/policies/nightly",
            ok(json!({
                "name": "nightly", "revision": 3, "yaml": "",
                "policy": {"spec": {
                    "retention": {"keepDaily": 7},
                    "schedule": {"cron": "0 2 * * *", "timezone": "Europe/Berlin"}
                }}
            })),
        ),
        (
            "POST /api/v1/policies/nightly/retention-preview",
            ok(json!({
                "total": 40, "forget": 3, "suspicious": 2, "truncated": 0,
                "hosts": [
                    {"host_id": "h1", "hostname": "db-1", "total": 30, "forget": 3,
                     "snapshots": [
                        {"id": "aa", "time": 1_600_000_000_000i64, "received_at": 1},
                        {"id": "bb", "time": 1_600_000_100_000i64, "received_at": 2},
                        {"id": "cc", "time": 1_600_000_200_000i64, "received_at": 3}
                     ]},
                    {"host_id": "h2", "hostname": "web-1", "total": 10, "forget": 0,
                     "snapshots": []}
                ]
            })),
        ),
    ]);
    let text = stdout(lode(&server, home.path()).args(["policies", "retention", "nightly"]));
    assert_eq!(
        text,
        "Would forget 3 of 40 snapshots on 2 hosts.\n\
         2 suspicious snapshots are not counted and are never forgotten.\n\
         \n\
         db-1 (forget 3 of 30)\n\
         \x20 aa  2020-09-13 12:26:40\n\
         \x20 bb  2020-09-13 12:28:20\n\
         \x20 cc  2020-09-13 12:30:00\n"
    );
    let sent: Value = serde_json::from_str(&server.requests()[1].body).unwrap();
    assert_eq!(
        sent,
        json!({"retention": {"keepDaily": 7}, "timezone": "Europe/Berlin"})
    );
    assert!(server.requests().iter().all(|r| r.method != "DELETE"));

    let json_text =
        stdout(lode(&server, home.path()).args(["--json", "policies", "retention", "nightly"]));
    let lines: Vec<Value> = json_text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["type"], "retention_host");
    assert_eq!(lines[0]["hostname"], "db-1");
    assert_eq!(lines[2]["type"], "retention_summary");
    assert_eq!(lines[2]["forget"], 3);
}

#[test]
fn server_cli_policy_retention_nothing_to_forget() {
    let home = tempfile::tempdir().unwrap();
    let server = fake(vec![
        (
            "GET /api/v1/policies/nightly",
            ok(json!({"name": "nightly", "revision": 1, "yaml": "",
                      "policy": {"spec": {"retention": {"keepLast": 5}}}})),
        ),
        (
            "POST /api/v1/policies/nightly/retention-preview",
            ok(
                json!({"total": 4, "forget": 0, "suspicious": 0, "truncated": 0,
                      "hosts": [{"host_id": "h1", "hostname": "db-1", "total": 4,
                                 "forget": 0, "snapshots": []}]}),
            ),
        ),
    ]);
    let text = stdout(lode(&server, home.path()).args(["policies", "retention", "nightly"]));
    assert_eq!(text, "Nothing would be forgotten right now.\n");
}
