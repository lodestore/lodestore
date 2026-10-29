mod common;

use common::{ADMIN_PASSWORD, TestServer, backends};
use lodestore_agent::{EnrollArgs, NetOptions};
use lodestore_proto::agent::v1::JobSpec;
use prost::Message;
use reqwest::Method;
use serde_json::{Value, json};
use sqlx::Row;

struct Http {
    client: reqwest::Client,
    base: String,
    cookie: String,
    csrf: String,
}

impl Http {
    async fn login(ts: &TestServer) -> Http {
        let client = reqwest::Client::new();
        let base = ts.rest_url();
        let response = client
            .post(format!("{base}/api/v1/auth/login"))
            .json(&json!({ "username": "admin", "password": ADMIN_PASSWORD }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 204);
        let cookie = response.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let session: Value = client
            .get(format!("{base}/api/v1/auth/session"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        Http {
            client,
            base,
            cookie,
            csrf: session["csrf_token"].as_str().unwrap().to_string(),
        }
    }

    async fn send(&self, method: Method, path: &str, body: Option<&Value>) -> (u16, Value) {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .header("cookie", &self.cookie)
            .header("x-csrf-token", &self.csrf);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let bytes = response.bytes().await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        self.send(Method::GET, path, None).await
    }

    async fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        self.send(Method::POST, path, Some(body)).await
    }

    async fn post_empty(&self, path: &str) -> (u16, Value) {
        self.send(Method::POST, path, None).await
    }

    async fn put(&self, path: &str, body: &Value) -> (u16, Value) {
        self.send(Method::PUT, path, Some(body)).await
    }

    async fn delete(&self, path: &str) -> (u16, Value) {
        self.send(Method::DELETE, path, None).await
    }
}

async fn enroll(ts: &TestServer, http: &Http, repo: &str, hostname: &str, labels: Value) -> String {
    let token = common::Api::login(&ts.rest_url(), "admin", ADMIN_PASSWORD)
        .await
        .join_token(repo, labels)
        .await;
    lodestore_agent::enroll(EnrollArgs {
        token,
        paths: ts.agent_paths(hostname),
        hostname: Some(hostname.to_string()),
        net: NetOptions::default(),
    })
    .await
    .expect("enroll");
    let (_, hosts) = http.get("/api/v1/hosts").await;
    hosts
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["hostname"] == hostname)
        .map(|h| h["id"].as_str().unwrap().to_string())
        .expect("the enrolled host")
}

fn prod() -> Value {
    json!([{ "key": "env", "value": "prod" }])
}

fn policy_yaml(name: &str, repo: &str, extra: &str) -> String {
    format!(
        "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: {name}\n\
         spec:\n  repository: {repo}\n  selector:\n    matchLabels:\n      env: prod\n  \
         paths:\n    - /etc\n{extra}"
    )
}

async fn create(http: &Http, name: &str, repo: &str) -> (u16, Value) {
    http.post(
        "/api/v1/policies",
        &json!({ "yaml": policy_yaml(name, repo, "") }),
    )
    .await
}

async fn job_spec(ts: &TestServer, id: &str) -> JobSpec {
    let row = sqlx::query("SELECT spec FROM jobs WHERE id = $1")
        .bind(id)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap();
    JobSpec::decode(row.try_get::<Vec<u8>, _>("spec").unwrap().as_slice()).unwrap()
}

async fn count_jobs(ts: &TestServer) -> i64 {
    sqlx::query("SELECT COUNT(*) AS n FROM jobs")
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .try_get("n")
        .unwrap()
}

async fn set_facts(ts: &TestServer, host: &str, facts: Value) {
    sqlx::query("UPDATE agents SET facts_json = $1 WHERE id = $2")
        .bind(facts.to_string())
        .bind(host)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policies_create_run_lists_queued_job() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &http, &repo, "host-a", prod()).await;

        let (status, created) = create(&http, "nightly", "main").await;
        assert_eq!(status, 201, "[{}] {created}", backend.name());
        assert_eq!(created["revision"], 1);
        assert_eq!(created["matching"]["hosts"][0]["id"], host);

        let (status, first) = http.post_empty("/api/v1/policies/nightly/run").await;
        assert_eq!(status, 200);
        assert_eq!(first["jobs"].as_array().unwrap().len(), 1);
        assert_eq!(first["jobs"][0]["created"], true);
        assert_eq!(first["jobs"][0]["host_id"], host);

        let (_, second) = http.post_empty("/api/v1/policies/nightly/run").await;
        assert_eq!(second["jobs"][0]["created"], false);
        assert_eq!(second["jobs"][0]["id"], first["jobs"][0]["id"]);

        let (status, page) = http.get("/api/v1/jobs").await;
        assert_eq!(status, 200);
        assert_eq!(page["total"], 1);
        let job = &page["items"][0];
        assert_eq!(job["state"], "queued");
        assert_eq!(job["queue_reason"], "host_offline");
        assert_eq!(job["policy_name"], "nightly");
        assert_eq!(job["source"], "manual");

        let spec = job_spec(&ts, job["id"].as_str().unwrap()).await;
        assert_eq!(spec.tags, vec!["policy=nightly".to_string()]);
        assert_eq!(spec.paths, vec!["/etc".to_string()]);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policies_validate_reports_errors_and_matching() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let main = ts.create_local_repository("main").await;
        let other = ts.create_local_repository("other").await;
        enroll(&ts, &http, &main, "host-a", prod()).await;
        enroll(&ts, &http, &other, "host-b", prod()).await;
        let revoked = enroll(&ts, &http, &main, "host-c", prod()).await;
        http.post_empty(&format!("/api/v1/hosts/{revoked}/revoke"))
            .await;

        let (status, bad) = http
            .post(
                "/api/v1/policies/validate",
                &json!({ "yaml": policy_yaml("p", "main", "  schedule: daily\n") }),
            )
            .await;
        assert_eq!(status, 200);
        assert_eq!(bad["valid"], false, "[{}]", backend.name());
        assert!(bad["errors"][0]["line"].is_number());
        assert!(bad["matching"].is_null());

        let (_, missing) = http
            .post(
                "/api/v1/policies/validate",
                &json!({ "yaml": policy_yaml("p", "nowhere", "") }),
            )
            .await;
        assert_eq!(missing["valid"], false);
        assert_eq!(missing["errors"][0]["path"], "spec.repository");

        let object = json!({
            "apiVersion": "lodestore.io/v1alpha1",
            "kind": "BackupPolicy",
            "metadata": { "name": "p" },
            "spec": {
                "repository": "main",
                "selector": { "matchLabels": { "env": "prod" } },
                "paths": ["/etc"]
            }
        });
        let (_, good) = http
            .post("/api/v1/policies/validate", &json!({ "policy": object }))
            .await;
        assert_eq!(good["valid"], true, "{good}");
        assert!(
            good["yaml"]
                .as_str()
                .unwrap()
                .contains("kind: BackupPolicy")
        );
        assert_eq!(good["name_taken"], false);
        let hosts = good["matching"]["hosts"].as_array().unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0]["hostname"], "host-a");
        assert_eq!(hosts[0]["online"], false);
        let others = good["matching"]["other_repository"].as_array().unwrap();
        assert_eq!(others.len(), 1);
        assert_eq!(others[0]["hostname"], "host-b");
        assert_eq!(others[0]["repository_name"], "other");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policies_hook_warnings_from_facts() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &http, &repo, "host-a", prod()).await;
        let hooks = "  hooks:\n    before:\n      - script: dump-db\n      - script: other\n      \
                     - command: /bin/true\n";
        let body = json!({ "yaml": policy_yaml("p", "main", hooks) });

        let (_, none) = http.post("/api/v1/policies/validate", &body).await;
        assert_eq!(none["valid"], true, "[{}] {none}", backend.name());
        assert!(none["warnings"].as_array().unwrap().is_empty());

        set_facts(
            &ts,
            &host,
            json!({ "hooks": { "allow_inline": false, "scripts": [{ "name": "dump-db", "ok": false }] } }),
        )
        .await;
        let (_, warned) = http.post("/api/v1/policies/validate", &body).await;
        let warnings = warned["warnings"].as_array().unwrap();
        let find = |kind: &str| warnings.iter().find(|w| w["kind"] == kind).cloned();
        assert_eq!(find("script_rejected").unwrap()["script"], "dump-db");
        assert_eq!(find("script_missing").unwrap()["script"], "other");
        assert!(find("inline_disabled").is_some());
        assert_eq!(warnings.len(), 3);
        assert_eq!(warnings[0]["host_id"], host);

        let (status, saved) = http.post("/api/v1/policies", &body).await;
        assert_eq!(status, 201, "warnings must not block saving");
        assert_eq!(saved["warnings"].as_array().unwrap().len(), 3);
        let (status, run) = http.post_empty("/api/v1/policies/p/run").await;
        assert_eq!(status, 200);
        assert_eq!(run["jobs"][0]["created"], true);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policies_crud_and_name_rules() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let repo = ts.create_local_repository("main").await;
        enroll(&ts, &http, &repo, "host-a", prod()).await;

        let (status, created) = create(&http, "p", "main").await;
        assert_eq!(status, 201, "[{}]", backend.name());
        let (status, dup) = create(&http, "p", "main").await;
        assert_eq!(status, 409);
        assert_eq!(
            dup["message"],
            "A policy named \"p\" already exists. Choose another name."
        );
        let (status, bad) = create(&http, "q", "nowhere").await;
        assert_eq!(status, 422);
        assert!(bad["message"].as_str().unwrap().contains("does not exist"));
        let (status, _) = create(&http, "validate", "main").await;
        assert_eq!(status, 422);

        let (_, taken) = http
            .post(
                "/api/v1/policies/validate",
                &json!({ "yaml": policy_yaml("p", "main", "") }),
            )
            .await;
        assert_eq!(taken["name_taken"], true);
        let (_, own) = http
            .post(
                "/api/v1/policies/validate",
                &json!({ "yaml": policy_yaml("p", "main", ""), "original_name": "p" }),
            )
            .await;
        assert_eq!(own["name_taken"], false);

        let (status, got) = http.get("/api/v1/policies/p").await;
        assert_eq!(status, 200);
        assert_eq!(got["yaml"], created["yaml"]);
        assert_eq!(got["repository_name"], "main");
        assert_eq!(http.get("/api/v1/policies/unknown").await.0, 404);

        let edited = policy_yaml("p", "main", "  excludeCaches: true\n");
        let (status, put) = http
            .put(
                "/api/v1/policies/p",
                &json!({ "yaml": edited, "base_revision": 1 }),
            )
            .await;
        assert_eq!(status, 200, "{put}");
        assert_eq!(put["revision"], 2);
        assert_eq!(put["policy"]["spec"]["excludeCaches"], true);

        let (status, renamed) = http
            .put(
                "/api/v1/policies/p",
                &json!({ "yaml": policy_yaml("z", "main", ""), "base_revision": 2 }),
            )
            .await;
        assert_eq!(status, 422);
        assert_eq!(renamed["message"], "The name cannot be changed.");
        let (status, _) = http
            .put(
                "/api/v1/policies/ghost",
                &json!({ "yaml": policy_yaml("ghost", "main", ""), "base_revision": 1 }),
            )
            .await;
        assert_eq!(status, 404);

        create(&http, "a-first", "main").await;
        let (_, list) = http.get("/api/v1/policies").await;
        let names: Vec<_> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["a-first", "p"]);
        assert_eq!(list[1]["hosts"], 1);
        assert_eq!(list[1]["match_labels"][0]["key"], "env");

        http.post_empty("/api/v1/policies/p/run").await;
        let (status, blocked) = http.delete("/api/v1/policies/p").await;
        assert_eq!(status, 409);
        assert_eq!(
            blocked["message"],
            "Policy \"p\" has 1 jobs in progress. Cancel them first, then delete the policy."
        );
        assert_eq!(http.delete("/api/v1/policies/a-first").await.0, 204);
        assert_eq!(http.get("/api/v1/policies/a-first").await.0, 404);
        assert_eq!(http.delete("/api/v1/policies/a-first").await.0, 404);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policies_update_requires_current_revision() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let repo = ts.create_local_repository("main").await;
        enroll(&ts, &http, &repo, "host-a", prod()).await;
        create(&http, "p", "main").await;

        let first = policy_yaml("p", "main", "  excludeCaches: true\n");
        let (status, saved) = http
            .put(
                "/api/v1/policies/p",
                &json!({ "yaml": first, "base_revision": 1 }),
            )
            .await;
        assert_eq!(status, 200, "[{}] {saved}", backend.name());
        assert_eq!(saved["revision"], 2);

        let stale = policy_yaml("p", "main", "  oneFileSystem: true\n");
        let (status, body) = http
            .put(
                "/api/v1/policies/p",
                &json!({ "yaml": stale, "base_revision": 1 }),
            )
            .await;
        assert_eq!(status, 409, "[{}] {body}", backend.name());
        assert_eq!(body["code"], "policy_conflict");

        let (_, after) = http.get("/api/v1/policies/p").await;
        assert_eq!(after["revision"], 2);
        assert_eq!(after["yaml"], saved["yaml"]);
        assert_eq!(after["updated_at"], saved["updated_at"]);

        let missing = http
            .put("/api/v1/policies/p", &json!({ "yaml": stale }))
            .await;
        assert_eq!(missing.0, 422, "[{}]", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policies_edit_does_not_change_pinned_job() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let repo = ts.create_local_repository("main").await;
        enroll(&ts, &http, &repo, "host-a", prod()).await;
        create(&http, "p", "main").await;
        let (_, run) = http.post_empty("/api/v1/policies/p/run").await;
        let job = run["jobs"][0]["id"].as_str().unwrap().to_string();

        let edited = policy_yaml("p", "main", "").replace("- /etc", "- /etc\n    - /var");
        let (status, _) = http
            .put(
                "/api/v1/policies/p",
                &json!({ "yaml": edited, "base_revision": 1 }),
            )
            .await;
        assert_eq!(status, 200);
        assert_eq!(
            job_spec(&ts, &job).await.paths,
            vec!["/etc".to_string()],
            "[{}]",
            backend.name()
        );

        sqlx::query("UPDATE jobs SET state = 'cancelled', finished_at = 1 WHERE id = $1")
            .bind(&job)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        assert_eq!(http.delete("/api/v1/policies/p").await.0, 204);
        assert_eq!(job_spec(&ts, &job).await.paths, vec!["/etc".to_string()]);
        let (_, detail) = http.get(&format!("/api/v1/jobs/{job}")).await;
        assert_eq!(detail["policy_name"], "p");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policies_concurrent_create_one_wins() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        ts.create_local_repository("main").await;
        let (a, b) = tokio::join!(create(&http, "p", "main"), create(&http, "p", "main"));
        let mut statuses = [a.0, b.0];
        statuses.sort_unstable();
        assert_eq!(statuses, [201, 409], "[{}]", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policies_host_run_and_host_policies() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let repo = ts.create_local_repository("main").await;
        let one = enroll(&ts, &http, &repo, "host-1", prod()).await;
        let two = enroll(&ts, &http, &repo, "host-2", prod()).await;
        create(&http, "a", "main").await;
        create(&http, "b", "main").await;

        let (status, all) = http.post_empty(&format!("/api/v1/hosts/{one}/run")).await;
        assert_eq!(status, 200, "[{}] {all}", backend.name());
        let jobs = all["jobs"].as_array().unwrap();
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().all(|j| j["created"] == true));
        let (_, again) = http
            .post(&format!("/api/v1/hosts/{one}/run"), &json!({}))
            .await;
        assert!(
            again["jobs"]
                .as_array()
                .unwrap()
                .iter()
                .all(|j| j["created"] == false)
        );

        let (_, only_a) = http
            .post(
                &format!("/api/v1/hosts/{two}/run"),
                &json!({ "policies": ["a"] }),
            )
            .await;
        assert_eq!(only_a["jobs"].as_array().unwrap().len(), 1);
        assert_eq!(only_a["jobs"][0]["hostname"], "host-2");
        let (status, _) = http
            .post(
                &format!("/api/v1/hosts/{two}/run"),
                &json!({ "policies": ["zzz"] }),
            )
            .await;
        assert_eq!(status, 422);

        let (status, listed) = http.get(&format!("/api/v1/hosts/{two}/policies")).await;
        assert_eq!(status, 200);
        assert_eq!(listed[0]["name"], "a");
        assert_eq!(listed[0]["last_job"]["state"], "queued");
        assert_eq!(listed[1]["name"], "b");
        assert!(listed[1]["last_job"].is_null());

        assert_eq!(http.get("/api/v1/hosts/nope/policies").await.0, 404);
        assert_eq!(http.post_empty("/api/v1/hosts/nope/run").await.0, 404);
        http.post_empty(&format!("/api/v1/hosts/{two}/revoke"))
            .await;
        assert_eq!(
            http.post_empty(&format!("/api/v1/hosts/{two}/run")).await.0,
            409
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policies_run_skips_other_repository_and_revoked() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let main = ts.create_local_repository("main").await;
        let other = ts.create_local_repository("other").await;
        let kept = enroll(&ts, &http, &main, "host-a", prod()).await;
        enroll(&ts, &http, &other, "host-b", prod()).await;
        let revoked = enroll(&ts, &http, &main, "host-c", prod()).await;
        enroll(&ts, &http, &main, "host-d", json!([])).await;
        http.post_empty(&format!("/api/v1/hosts/{revoked}/revoke"))
            .await;
        create(&http, "p", "main").await;

        let (status, run) = http.post_empty("/api/v1/policies/p/run").await;
        assert_eq!(status, 200);
        let jobs = run["jobs"].as_array().unwrap();
        assert_eq!(jobs.len(), 1, "[{}] {run}", backend.name());
        assert_eq!(jobs[0]["host_id"], kept);
        assert_eq!(count_jobs(&ts).await, 1);

        let (_, summary) = http.get("/api/v1/policies").await;
        assert_eq!(summary[0]["hosts"], 1);
        assert_eq!(summary[0]["other_repository_hosts"], 1);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policies_host_list_selects_only_listed_hosts() {
    let backend = &backends()[0];
    let ts = TestServer::start(backend).await;
    let http = Http::login(&ts).await;
    let main = ts.create_local_repository("main").await;
    let a = enroll(&ts, &http, &main, "host-a", prod()).await;
    enroll(&ts, &http, &main, "host-b", prod()).await;

    let object = json!({
        "apiVersion": "lodestore.io/v1alpha1",
        "kind": "BackupPolicy",
        "metadata": { "name": "picked" },
        "spec": {
            "repository": "main",
            "selector": { "hosts": [a] },
            "paths": ["/etc"]
        }
    });
    let (_, checked) = http
        .post("/api/v1/policies/validate", &json!({ "policy": object }))
        .await;
    assert_eq!(checked["valid"], true, "{checked}");
    let hosts = checked["matching"]["hosts"].as_array().unwrap();
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0]["hostname"], "host-a");

    let (status, _) = http
        .post("/api/v1/policies", &json!({ "policy": object }))
        .await;
    assert_eq!(status, 201);
    let (_, list) = http.get("/api/v1/policies").await;
    assert_eq!(list[0]["host_list"], true);
    assert_eq!(list[0]["hosts"], 1);
    ts.stop().await;
}

const KYIV_SCHEDULE: &str = "  schedule:\n    cron: \"30 3 * * *\"\n    timezone: Europe/Kyiv\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policies_schedule_info_in_validate() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        ts.create_local_repository("main").await;

        let (_, checked) = http
            .post(
                "/api/v1/policies/validate",
                &json!({ "yaml": policy_yaml("p", "main", KYIV_SCHEDULE) }),
            )
            .await;
        assert_eq!(checked["valid"], true, "[{}] {checked}", backend.name());
        let schedule = &checked["schedule"];
        assert!(!schedule["summary"].as_str().unwrap().is_empty());
        assert_eq!(schedule["timezone"], "Europe/Kyiv");
        assert_eq!(schedule["jitter_secs"], 900);
        assert_eq!(schedule["catch_up_secs"], 86400);
        let runs: Vec<i64> = schedule["next_runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_i64().unwrap())
            .collect();
        assert_eq!(runs.len(), 3);
        assert!(runs.windows(2).all(|w| w[0] < w[1]));
        let kinds: Vec<&str> = schedule["dst_notes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["kind"].as_str().unwrap())
            .collect();
        // The 366-day horizon can miss one of the two transitions when the last Sunday drifts
        // later next year, but it always holds at least one.
        assert!(!kinds.is_empty(), "{kinds:?}");
        assert!(
            kinds.iter().all(|k| ["skipped", "repeated"].contains(k)),
            "{kinds:?}"
        );

        let (status, created) = http
            .post(
                "/api/v1/policies",
                &json!({ "yaml": policy_yaml("p", "main", KYIV_SCHEDULE) }),
            )
            .await;
        assert_eq!(status, 201);
        assert_eq!(created["schedule"]["timezone"], "Europe/Kyiv");
        let (_, list) = http.get("/api/v1/policies").await;
        assert_eq!(
            list[0]["schedule"]["next_runs"].as_array().unwrap().len(),
            3
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policies_schedule_absent_is_null() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        ts.create_local_repository("main").await;

        let (_, checked) = http
            .post(
                "/api/v1/policies/validate",
                &json!({ "yaml": policy_yaml("p", "main", "") }),
            )
            .await;
        assert_eq!(checked["valid"], true);
        assert!(checked["schedule"].is_null(), "[{}]", backend.name());
        let (_, invalid) = http
            .post(
                "/api/v1/policies/validate",
                &json!({ "yaml": policy_yaml("p", "nowhere", "") }),
            )
            .await;
        assert!(invalid["schedule"].is_null());

        let (_, created) = create(&http, "p", "main").await;
        assert!(created["schedule"].is_null());
        let (_, list) = http.get("/api/v1/policies").await;
        assert!(list[0]["schedule"].is_null());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policies_managed_by_in_list_and_detail() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        ts.create_local_repository("main").await;
        let (status, _) = create(&http, "ui-made", "main").await;
        assert_eq!(status, 201);

        let token = ts.issue_token(&["policies:write"]).await;
        let applied = common::bearer_client(&token)
            .post(format!("{}/api/v1/policies/apply", ts.rest_url()))
            .json(&json!({ "documents": [{
                "source": "a.yaml",
                "yaml": policy_yaml("catalog-made", "main", ""),
            }] }))
            .send()
            .await
            .unwrap();
        assert_eq!(applied.status(), 200, "[{}]", backend.name());

        let (_, list) = http.get("/api/v1/policies").await;
        let managed = |name: &str| {
            list.as_array()
                .unwrap()
                .iter()
                .find(|p| p["name"] == name)
                .map(|p| p["managed_by"].clone())
                .unwrap()
        };
        assert_eq!(managed("catalog-made"), "lode-apply");
        assert!(managed("ui-made").is_null());
        let (_, detail) = http.get("/api/v1/policies/catalog-made").await;
        assert_eq!(detail["managed_by"], "lode-apply");

        let edited_yaml = policy_yaml("catalog-made", "main", "  exclude:\n    - '*.tmp'\n");
        let (status, edited) = http
            .put(
                "/api/v1/policies/catalog-made",
                &json!({ "base_revision": detail["revision"], "yaml": edited_yaml }),
            )
            .await;
        assert_eq!(status, 200, "{edited}");
        assert_eq!(edited["managed_by"], "lode-apply");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policies_host_next_run() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let http = Http::login(&ts).await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &http, &repo, "host-a", prod()).await;
        let (status, _) = http
            .post(
                "/api/v1/policies",
                &json!({ "yaml": policy_yaml("timed", "main", KYIV_SCHEDULE) }),
            )
            .await;
        assert_eq!(status, 201, "[{}]", backend.name());
        let (status, _) = create(&http, "manual", "main").await;
        assert_eq!(status, 201);

        let (status, policies) = http.get(&format!("/api/v1/hosts/{host}/policies")).await;
        assert_eq!(status, 200);
        let next = |name: &str| {
            policies
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["name"] == name)
                .map(|p| p["next_run"].clone())
                .unwrap()
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        assert!(next("timed").as_i64().unwrap() > now - 5000);
        assert!(next("manual").is_null());
        ts.stop().await;
    }
}
