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
const APPLY: &str = "POST /api/v1/policies/apply";

#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    path: String,
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

    fn bodies(&self) -> Vec<Value> {
        self.requests()
            .iter()
            .map(|r| serde_json::from_str(&r.body).unwrap())
            .collect()
    }
}

fn reply(code: u16, value: Value) -> Responder {
    let text = value.to_string();
    Box::new(move |_, _| (code, text.clone()))
}

async fn handle(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, 1 << 20).await.unwrap();
    let recorded = Recorded {
        method: parts.method.to_string(),
        path: parts.uri.path().to_owned(),
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
    let mut cmd = Command::cargo_bin("lode").unwrap();
    cmd.env_remove("LODESTORE_SERVER")
        .env_remove("LODESTORE_TOKEN")
        .env_remove("LODE_REPOSITORY")
        .env_remove("NO_COLOR")
        .env("XDG_CONFIG_HOME", home)
        .env("HOME", home)
        .env("LODESTORE_SERVER", &server.url)
        .env("LODESTORE_TOKEN", TOKEN)
        .arg("apply");
    cmd
}

fn out(cmd: &mut Command, code: i32) -> (String, String) {
    let output = cmd.assert().code(code).get_output().clone();
    (
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

fn policy(name: &str) -> String {
    format!(
        "apiVersion: lodestore.io/v1\nkind: BackupPolicy\nmetadata:\n  name: {name}\nspec:\n  paths: [/etc]\n"
    )
}

fn mixed_plan() -> Value {
    json!({
        "dry_run": true, "manager": "lode-apply", "unchanged": 4,
        "changes": [
            {"action": "create", "name": "etc-and-home", "source": "a.yaml",
             "adopted": false, "managed_by": null, "fields": []},
            {"action": "update", "name": "db-nightly", "source": "sub/b.yaml",
             "adopted": false, "managed_by": null, "fields": [
                {"path": "spec.schedule.cron", "from": "0 2 * * *", "to": "30 2 * * *"},
                {"path": "spec.retention.keepDaily", "from": null, "to": 14}
             ]},
            {"action": "delete", "name": "old-test", "source": null,
             "adopted": false, "managed_by": "lode-apply", "fields": []}
        ]
    })
}

fn no_changes() -> Value {
    json!({"dry_run": true, "manager": "lode-apply", "unchanged": 3, "changes": []})
}

#[test]
fn apply_cli_dry_run_diff_exit_4() {
    let home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.yaml"), policy("etc-and-home")).unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/b.yml"), policy("db-nightly")).unwrap();
    let server = fake(vec![(APPLY, reply(200, mixed_plan()))]);
    let (stdout, _) = out(
        lode(&server, home.path())
            .arg("-f")
            .arg(dir.path())
            .arg("--dry-run"),
        4,
    );
    assert_eq!(
        stdout,
        "+ policy/etc-and-home    create\n\
         ~ policy/db-nightly      update\n\
         \x20   spec.schedule.cron          \"0 2 * * *\" -> \"30 2 * * *\"\n\
         \x20   spec.retention.keepDaily    (unset) -> 14\n\
         - policy/old-test        delete  (managed by lode-apply)\n\
         Plan: 1 to create, 1 to update, 1 to delete, 4 unchanged.\n"
    );
    let bodies = server.bodies();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["dry_run"], true);
    assert_eq!(bodies[0]["manager"], "lode-apply");
    assert_eq!(bodies[0]["prune"], false);
    let documents = bodies[0]["documents"].as_array().unwrap();
    assert_eq!(documents.len(), 2);
    assert_eq!(documents[0]["source"], "a.yaml");
    assert_eq!(documents[0]["yaml"], policy("etc-and-home"));
    assert_eq!(documents[1]["source"], "sub/b.yml");
    assert_eq!(documents[1]["yaml"], policy("db-nightly"));
}

#[test]
fn apply_cli_no_changes_exit_0() {
    let home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.yaml"), policy("a")).unwrap();
    let server = fake(vec![(
        APPLY,
        Box::new(|_, request: &Recorded| {
            let dry_run = serde_json::from_str::<Value>(&request.body).unwrap()["dry_run"]
                .as_bool()
                .unwrap();
            let mut plan = no_changes();
            plan["dry_run"] = json!(dry_run);
            (200, plan.to_string())
        }),
    )]);
    let (stdout, _) = out(
        lode(&server, home.path())
            .arg("-f")
            .arg(dir.path())
            .arg("--dry-run"),
        0,
    );
    assert_eq!(
        stdout,
        "Plan: 0 to create, 0 to update, 0 to delete, 3 unchanged.\n"
    );
    let (stdout, _) = out(lode(&server, home.path()).arg("-f").arg(dir.path()), 0);
    assert_eq!(
        stdout,
        "Applied: 0 to create, 0 to update, 0 to delete, 3 unchanged.\n"
    );
}

#[test]
fn apply_cli_reads_dir_recursively_multi_doc() {
    let home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("z/deep")).unwrap();
    std::fs::write(
        root.join("a.yaml"),
        format!("{}---\n{}", policy("one"), policy("two")),
    )
    .unwrap();
    std::fs::write(root.join("z/deep/c.yml"), policy("three")).unwrap();
    std::fs::write(root.join("z/b.yaml"), policy("four")).unwrap();
    std::fs::write(root.join("notes.txt"), "not a policy").unwrap();
    std::fs::write(root.join("z/deep/skip.json"), "{}").unwrap();
    let server = fake(vec![(APPLY, reply(200, no_changes()))]);
    out(
        lode(&server, home.path())
            .arg("-f")
            .arg(root)
            .arg("--dry-run"),
        0,
    );
    let bodies = server.bodies();
    let documents = bodies[0]["documents"].as_array().unwrap();
    let sources: Vec<_> = documents
        .iter()
        .map(|d| d["source"].as_str().unwrap())
        .collect();
    assert_eq!(sources, ["a.yaml", "z/b.yaml", "z/deep/c.yml"]);
    assert!(
        documents[0]["yaml"]
            .as_str()
            .unwrap()
            .contains("---\napiVersion")
    );

    let single = root.join("z/b.yaml");
    out(lode(&server, home.path()).arg("-f").arg(&single), 0);
    let last = server.bodies().pop().unwrap();
    assert_eq!(last["documents"][0]["source"], "b.yaml");

    let empty = tempfile::tempdir().unwrap();
    let missing = server.requests().len();
    let (_, stderr) = out(lode(&server, home.path()).arg("-f").arg(empty.path()), 2);
    assert!(
        stderr.contains("No .yaml or .yml files found in"),
        "{stderr}"
    );
    assert_eq!(server.requests().len(), missing);
}

fn one_file() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.yaml"), policy("a")).unwrap();
    dir
}

#[test]
fn apply_cli_applies_exit_0() {
    let home = tempfile::tempdir().unwrap();
    let dir = one_file();
    let plan = json!({
        "dry_run": false, "manager": "lode-apply", "unchanged": 1,
        "changes": [{"action": "create", "name": "a", "source": "a.yaml",
                     "adopted": false, "managed_by": null, "fields": []}]
    });
    let server = fake(vec![(APPLY, reply(200, plan))]);
    let (stdout, _) = out(lode(&server, home.path()).arg("-f").arg(dir.path()), 0);
    assert_eq!(
        stdout,
        "+ policy/a    create\n\
         Applied: 1 to create, 0 to update, 0 to delete, 1 unchanged.\n"
    );
    assert_eq!(server.bodies()[0]["dry_run"], false);
}

#[test]
fn apply_cli_prune_without_tty_is_consent() {
    let home = tempfile::tempdir().unwrap();
    let dir = one_file();
    let plan = json!({
        "dry_run": false, "manager": "lode-apply", "unchanged": 1,
        "changes": [
            {"action": "delete", "name": "old-a", "source": null, "adopted": false,
             "managed_by": "lode-apply", "fields": []},
            {"action": "delete", "name": "old-b", "source": null, "adopted": false,
             "managed_by": "lode-apply", "fields": []}
        ]
    });
    for extra in [&["--prune"][..], &["--prune", "--yes"][..]] {
        let server = fake(vec![(APPLY, reply(200, plan.clone()))]);
        let (stdout, stderr) = out(
            lode(&server, home.path())
                .arg("-f")
                .arg(dir.path())
                .args(extra),
            0,
        );
        let bodies = server.bodies();
        assert_eq!(bodies.len(), 1, "{extra:?}");
        assert_eq!(bodies[0]["prune"], true);
        assert_eq!(bodies[0]["dry_run"], false);
        assert!(stdout.contains("- policy/old-a    delete"), "{stdout}");
        assert!(stdout.contains("- policy/old-b    delete"), "{stdout}");
        assert!(
            stdout.ends_with("Applied: 0 to create, 0 to update, 2 to delete, 1 unchanged.\n"),
            "{stdout}"
        );
        assert!(!stdout.contains("[y/N]") && !stderr.contains("[y/N]"));
    }
}

#[test]
fn apply_cli_validation_errors_exit_2() {
    let home = tempfile::tempdir().unwrap();
    let dir = one_file();
    let body = json!({
        "code": "validation", "message": "b.yaml: line 7: unknown field",
        "errors": [
            {"source": "b.yaml", "line": 7, "column": 3, "path": null,
             "message": "unknown field `crn`, expected one of `cron`"},
            {"source": "c.yaml", "line": null, "column": null, "path": "spec.paths",
             "message": "Add at least one path."}
        ]
    });
    let server = fake(vec![(APPLY, reply(422, body))]);
    let (stdout, stderr) = out(lode(&server, home.path()).arg("-f").arg(dir.path()), 2);
    assert_eq!(stdout, "");
    assert_eq!(
        stderr,
        "b.yaml:7:3: unknown field `crn`, expected one of `cron`\n\
         c.yaml: spec.paths: Add at least one path.\n"
    );
}

#[test]
fn apply_cli_adopt_flag_sent() {
    let home = tempfile::tempdir().unwrap();
    let dir = one_file();
    let plan = json!({
        "dry_run": false, "manager": "ops", "unchanged": 0,
        "changes": [{"action": "update", "name": "a", "source": "a.yaml",
                     "adopted": true, "managed_by": null, "fields": []}]
    });
    let server = fake(vec![(APPLY, reply(200, plan))]);
    let (stdout, _) = out(
        lode(&server, home.path())
            .arg("-f")
            .arg(dir.path())
            .args(["--adopt", "--manager", "ops"]),
        0,
    );
    let body = &server.bodies()[0];
    assert_eq!(body["adopt"], true);
    assert_eq!(body["manager"], "ops");
    assert!(
        stdout.starts_with("~ policy/a    update  (adopted)\n"),
        "{stdout}"
    );
}

#[test]
fn apply_cli_conflict_needs_adopt() {
    let home = tempfile::tempdir().unwrap();
    let dir = one_file();
    let text = "policy/a exists and is not managed by lode-apply. Use --adopt to take it over.";
    let body = json!({
        "code": "validation", "message": format!("a.yaml: {text}"),
        "errors": [{"source": "a.yaml", "line": null, "column": null,
                    "path": "metadata.name", "message": text}]
    });
    let server = fake(vec![(APPLY, reply(422, body))]);
    let (_, stderr) = out(lode(&server, home.path()).arg("-f").arg(dir.path()), 2);
    assert!(stderr.contains(text), "{stderr}");
    assert_eq!(server.bodies()[0]["adopt"], false);
}

#[test]
fn apply_cli_token_refused_exit_13() {
    let home = tempfile::tempdir().unwrap();
    let dir = one_file();
    let body = json!({"code": "forbidden", "message": "The token needs the policies:write scope."});
    let server = fake(vec![(APPLY, reply(403, body))]);
    let (_, stderr) = out(lode(&server, home.path()).arg("-f").arg(dir.path()), 13);
    assert!(
        stderr.contains("The token was refused: it needs the policies:write scope."),
        "{stderr}"
    );
    assert!(!stderr.contains(TOKEN));
}

#[test]
fn apply_cli_server_error_exit_1() {
    let home = tempfile::tempdir().unwrap();
    let dir = one_file();
    let body =
        json!({"code": "conflict", "message": "Policies changed while applying. Run apply again."});
    let server = fake(vec![(APPLY, reply(409, body))]);
    let (_, stderr) = out(lode(&server, home.path()).arg("-f").arg(dir.path()), 1);
    assert!(
        stderr.contains("Policies changed while applying."),
        "{stderr}"
    );
}

#[test]
fn apply_cli_json_ndjson() {
    let home = tempfile::tempdir().unwrap();
    let dir = one_file();
    let server = fake(vec![(APPLY, reply(200, mixed_plan()))]);
    let (stdout, _) = out(
        lode(&server, home.path())
            .arg("-f")
            .arg(dir.path())
            .args(["--dry-run", "--json"]),
        4,
    );
    let lines: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 4);
    assert_eq!(
        lines[0],
        json!({"v": 1, "type": "change", "action": "create", "name": "etc-and-home",
               "adopted": false, "fields": []})
    );
    assert_eq!(lines[1]["action"], "update");
    assert_eq!(lines[1]["fields"][1]["path"], "spec.retention.keepDaily");
    assert_eq!(lines[1]["fields"][1]["from"], Value::Null);
    assert_eq!(lines[1]["fields"][1]["to"], 14);
    assert_eq!(
        lines[3],
        json!({"v": 1, "type": "summary", "create": 1, "update": 1, "delete": 1,
               "unchanged": 4, "dry_run": true})
    );
}

#[test]
fn apply_cli_no_color() {
    let home = tempfile::tempdir().unwrap();
    let dir = one_file();
    let server = fake(vec![(APPLY, reply(200, mixed_plan()))]);
    let (stdout, stderr) = out(
        lode(&server, home.path())
            .env("NO_COLOR", "1")
            .arg("-f")
            .arg(dir.path())
            .arg("--dry-run"),
        4,
    );
    assert!(!stdout.contains('\x1b') && !stderr.contains('\x1b'));
    assert!(stdout.starts_with("+ policy/etc-and-home"));
}
