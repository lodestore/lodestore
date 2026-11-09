mod common;

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use common::{Api, Backend, TestServer, WAIT, backends, bearer_client, wait_until};
use lodestore_format::crypto::Entropy;
use lodestore_format::index::{hex32, parse_hex32};
use lodestore_format::locks::{lock_id_hex, new_lock_id, publish_gc_manifest};
use lodestore_format::pack::pack_object_key;
use lodestore_format::repo::Repository;
use lodestore_format::snapshot::snapshot_object_key;
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, Writer};
use serde_json::{Value, json};
use sqlx::Row;

const PASSWORD: &[u8] = b"test-password";
const FROZEN: &str = r#"{"pause_ms":86400000,"repack_below_pct":50,"max_repack_bytes":1073741824}"#;

async fn write_snapshot(ts: &TestServer, name: &str, seed: &str) -> String {
    let store = LocalFsStore::new(ts.local_repo_root(name));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    let mut writer = Writer::new(&store, keys, repo.config()).await.unwrap();
    let data = ts.dir.path().join(format!("data-{seed}"));
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("file.txt"),
        format!("content of {seed} ").repeat(4000),
    )
    .unwrap();
    let outcome = writer
        .backup(&[data], BackupOptions::default(), &|_| {})
        .await
        .expect("backup");
    hex32(&outcome.snapshot_id)
}

fn forget_snapshot(ts: &TestServer, name: &str, id: &str) {
    let id = parse_hex32(id).unwrap();
    std::fs::remove_file(ts.local_repo_root(name).join(snapshot_object_key(&id))).unwrap();
}

fn age_packs(ts: &TestServer, name: &str) {
    let old = SystemTime::now() - Duration::from_secs(3 * 86_400);
    fn walk(dir: &Path, old: SystemTime) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, old);
            } else {
                File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_modified(old)
                    .unwrap();
            }
        }
    }
    walk(&ts.local_repo_root(name).join("packs"), old);
}

fn bucket(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                out.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(&path).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

async fn status(api: &Api, repo: &str) -> Value {
    let response = api.get(&format!("/api/v1/repositories/{repo}/gc")).await;
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

async fn start_dry_run(api: &Api, repo: &str) -> reqwest::Response {
    api.post(
        &format!("/api/v1/repositories/{repo}/gc/dry-run"),
        &json!({}),
    )
    .await
}

async fn dry_run_to_the_end(api: &Api, repo: &str) -> (String, Value) {
    let response = start_dry_run(api, repo).await;
    assert_eq!(response.status(), 202);
    let started: Value = response.json().await.unwrap();
    let cycle_id = started["cycle_id"].as_str().unwrap().to_string();
    let finished = wait_until(WAIT, || async {
        let s = status(api, repo).await;
        s["cycle"].is_null() && !s["last_dry_run"].is_null()
    })
    .await;
    assert!(finished, "the dry run did not finish");
    (cycle_id, status(api, repo).await)
}

async fn insert_cycle(ts: &TestServer, repo: &str, id: &str, state: &str, dry_run: i64) {
    sqlx::query(
        "INSERT INTO gc_cycles (id, repository_id, state, dry_run, origin, started_by, \
         settings_json, created_at, started_at) VALUES ($1, $2, $3, $4, 'manual', 'test', $5, $6, $6)",
    )
    .bind(id)
    .bind(repo)
    .bind(state)
    .bind(dry_run)
    .bind(FROZEN)
    .bind(lodestore_server::db::now_ms())
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
}

async fn two_snapshots_one_forgotten(ts: &TestServer, name: &str) {
    let a = write_snapshot(ts, name, "a").await;
    write_snapshot(ts, name, "b").await;
    forget_snapshot(ts, name, &a);
    age_packs(ts, name);
}

async fn server(backend: &Backend) -> TestServer {
    TestServer::start_with(backend, |c| c.schedule_tick_secs = 0).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_api_dry_run_reports_freed_space() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        two_snapshots_one_forgotten(&ts, "main").await;

        let (cycle_id, status) = dry_run_to_the_end(&api, &repo).await;
        let report = &status["last_dry_run"];
        assert!(report["bytes_freed"].as_u64().unwrap() > 0, "{report}");
        assert!(report["packs_deleted"].as_u64().unwrap() >= 1, "{report}");
        assert_eq!(report["locked_bytes"], 0);
        assert!(status["last_cleanup"].is_null());
        assert!(status["events_total"].as_u64().unwrap() >= 2);

        let response = api
            .get(&format!(
                "/api/v1/repositories/{repo}/gc/cycles/{cycle_id}/report"
            ))
            .await;
        assert_eq!(response.status(), 200);
        let full: Value = response.json().await.unwrap();
        let listed = full["delete"].as_array().unwrap();
        assert_eq!(
            listed.len() as u64,
            report["packs_deleted"].as_u64().unwrap()
        );
        let mut bytes = 0;
        for pack in listed {
            let id = parse_hex32(pack["id"].as_str().unwrap()).unwrap();
            assert!(
                ts.local_repo_root("main")
                    .join(pack_object_key(&id))
                    .exists(),
                "[{}] a dry run must leave {} in place",
                backend.name(),
                pack["id"]
            );
            assert_eq!(pack["live_bytes"], 0);
            bytes += pack["size"].as_u64().unwrap();
        }
        for pack in full["repack"].as_array().unwrap() {
            bytes += pack["size"].as_u64().unwrap() - pack["live_bytes"].as_u64().unwrap();
        }
        assert_eq!(bytes, report["bytes_freed"].as_u64().unwrap());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_api_dry_run_bucket_unchanged() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        two_snapshots_one_forgotten(&ts, "main").await;

        let before = bucket(&ts.local_repo_root("main"));
        dry_run_to_the_end(&api, &repo).await;
        assert!(
            bucket(&ts.local_repo_root("main")) == before,
            "[{}] the dry run changed the repository",
            backend.name()
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_api_dry_run_single_active() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        insert_cycle(&ts, &repo, "active-cycle", "waiting", 0).await;
        insert_cycle(&ts, &repo, "dry-cycle", "marking", 1).await;

        let response = start_dry_run(&api, &repo).await;
        assert_eq!(response.status(), 409, "[{}]", backend.name());
        let body: Value = response.json().await.unwrap();
        assert_eq!(
            body["message"],
            "A dry run is already running in this repository."
        );

        let shown = status(&api, &repo).await;
        assert_eq!(shown["cycle"]["id"], "active-cycle");
        assert_eq!(shown["cycle"]["state"], "waiting");
        assert_eq!(shown["active_dry_run"]["id"], "dry-cycle");

        let response = api
            .post("/api/v1/repositories/unknown/gc/dry-run", &json!({}))
            .await;
        assert_eq!(response.status(), 404);
        ts.stop().await;
    }
}

/// The status says how the last real cleanup failed, whatever dry runs and
/// events came after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_api_last_failure() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        assert!(status(&api, &repo).await["last_failure"].is_null());

        insert_cycle(&ts, &repo, "a-real", "failed", 0).await;
        sqlx::query(
            "UPDATE gc_cycles SET error = 'Deleting x failed', error_after_delete = 1, \
             sweep_started = 1, finished_at = 5 WHERE id = 'a-real'",
        )
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
        insert_cycle(&ts, &repo, "b-dry", "done", 1).await;

        let failure = &status(&api, &repo).await["last_failure"];
        assert_eq!(failure["cycle_id"], "a-real", "[{}]", backend.name());
        assert_eq!(failure["after_delete"], true);
        assert_eq!(failure["error"], "Deleting x failed");
        assert_eq!(failure["failed_at"], 5);
        assert_eq!(failure["origin"], "manual");

        insert_cycle(&ts, &repo, "c-real", "done", 0).await;
        assert!(status(&api, &repo).await["last_failure"].is_null());
        ts.stop().await;
    }
}

/// A dry run is always available (D-04): a real cycle waiting out its pause
/// does not take its slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_api_dry_run_beside_waiting_cycle() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        insert_cycle(&ts, &repo, "active-cycle", "waiting", 0).await;

        let response = start_dry_run(&api, &repo).await;
        assert_eq!(response.status(), 202, "[{}]", backend.name());
        let shown = status(&api, &repo).await;
        assert_eq!(shown["cycle"]["id"], "active-cycle");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_api_dry_run_interrupted_on_restart() {
    for backend in backends() {
        let mut ts = server(&backend).await;
        let repo = ts.create_local_repository("main").await;
        insert_cycle(&ts, &repo, "dry-cycle", "marking", 1).await;
        let other = ts.create_local_repository("other").await;
        insert_cycle(&ts, &other, "real-cycle", "waiting", 0).await;
        sqlx::query("UPDATE gc_cycles SET delete_after = $1 WHERE id = 'real-cycle'")
            .bind(lodestore_server::db::now_ms() + 86_400_000)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();

        ts.restart().await;

        let api = ts.api().await;
        let shown = status(&api, &repo).await;
        assert!(shown["cycle"].is_null(), "[{}]", backend.name());
        let row = sqlx::query("SELECT state, error FROM gc_cycles WHERE id = 'dry-cycle'")
            .fetch_one(&ts.state().db.pool)
            .await
            .unwrap();
        assert_eq!(row.get::<String, _>("state"), "failed");
        assert_eq!(
            row.get::<String, _>("error"),
            "The server restarted during the dry run."
        );
        let real = sqlx::query("SELECT state FROM gc_cycles WHERE id = 'real-cycle'")
            .fetch_one(&ts.state().db.pool)
            .await
            .unwrap();
        assert_eq!(real.get::<String, _>("state"), "waiting");
        ts.stop().await;
    }
}

async fn put_settings(ts: &TestServer, api: &Api, repo: &str, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .put(format!(
            "{}/api/v1/repositories/{repo}/gc/settings",
            ts.rest_url()
        ))
        .header("cookie", api.cookie())
        .header("x-csrf-token", &api.csrf)
        .json(body)
        .send()
        .await
        .unwrap()
}

fn input(enabled: bool) -> Value {
    json!({
        "enabled": enabled,
        "cron": "0 3 * * 0",
        "timezone": "UTC",
        "pause": "24h",
        "repack_below_pct": 50,
        "max_repack": "10GiB",
    })
}

async fn seed_agent(ts: &TestServer, repo: &str, id: &str, status: &str, caps: Option<&str>) {
    sqlx::query(
        "INSERT INTO agents (id, hostname, repository_id, labels_json, os, arch, version, status, \
         enrolled_at, cert_serial, cert_issued_at, cert_not_after, capabilities_json) \
         VALUES ($1, $2, $3, '{}', 'linux', 'x86_64', 'test', $4, 0, $1, 0, 0, $5)",
    )
    .bind(id)
    .bind(format!("host-{id}"))
    .bind(repo)
    .bind(status)
    .bind(caps)
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
}

async fn publish_manifest(ts: &TestServer, name: &str) -> String {
    let store = LocalFsStore::new(ts.local_repo_root(name));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let keys = repo.read_keys();
    let mut entropy = Entropy::os();
    let gc_id = new_lock_id(&mut entropy);
    publish_gc_manifest(
        &store,
        keys.k_idx(),
        keys.repo_id(),
        &gc_id,
        &[[7u8; 32]],
        &mut entropy,
    )
    .await
    .unwrap();
    lock_id_hex(&gc_id)
}

fn problems(status: &Value) -> Vec<String> {
    status["preflight"]["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["kind"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_api_settings_defaults() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;

        let shown = status(&api, &repo).await;
        assert_eq!(shown["enabled"], false, "[{}]", backend.name());
        assert_eq!(
            shown["settings"],
            json!({
                "cron": "0 3 * * 0",
                "timezone": "UTC",
                "pause": "24h",
                "repack_below_pct": 50,
                "max_repack": "10GiB",
            })
        );
        assert_eq!(shown["pause_floor"], "24h");
        assert_eq!(shown["schedule"]["next_runs"].as_array().unwrap().len(), 3);
        assert_eq!(shown["preflight"]["dry_run_allowed"], true);
        assert!(problems(&shown).is_empty());
        assert_eq!(shown["has_object_lock"], false);
        assert!(shown["cycle"].is_null() && shown["last_cleanup"].is_null());
        assert_eq!(shown["events_total"], 0);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_api_settings_pause_floor() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let yaml = "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: long\n\
                    spec:\n  repository: main\n  selector:\n    matchLabels:\n      env: e2e\n  \
                    paths:\n    - /srv\n  limits:\n    maxRuntime: 30h\n";
        let created = api.post("/api/v1/policies", &json!({ "yaml": yaml })).await;
        assert_eq!(created.status(), 201, "[{}]", backend.name());

        assert_eq!(status(&api, &repo).await["pause_floor"], "31h");
        let response = put_settings(&ts, &api, &repo, &input(false)).await;
        assert_eq!(
            response.status(),
            200,
            "[{}] turning cleanup off with the saved pause must work",
            backend.name()
        );
        let mut changed = input(false);
        changed["pause"] = json!("25h");
        let response = put_settings(&ts, &api, &repo, &changed).await;
        assert_eq!(response.status(), 422);
        let body: Value = response.json().await.unwrap();
        assert_eq!(
            body["errors"],
            json!([{
                "path": "spec.pause",
                "message": "Use a duration of 31h or more, such as 24h or 2d.",
            }])
        );

        let mut longer = input(false);
        longer["pause"] = json!("2d");
        let response = put_settings(&ts, &api, &repo, &longer).await;
        assert_eq!(response.status(), 200);
        let saved: Value = response.json().await.unwrap();
        assert_eq!(saved["settings"]["pause"], "2d");
        ts.stop().await;
    }
}

/// A policy whose stored spec can't be read could allow any time limit, so
/// it counts with the longest one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_api_unreadable_policy_raises_the_floor() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        sqlx::query(
            "INSERT INTO policies (id, name, repository_id, yaml, spec_json, revision, \
             created_at, updated_at) VALUES ('p1', 'broken', $1, '', '{', 1, 0, 0)",
        )
        .bind(&repo)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
        let longest = lodestore_policy::MAX_RUNTIME + lodestore_format::locks::RUNTIME_MARGIN;
        assert_eq!(
            status(&api, &repo).await["pause_floor"],
            lodestore_policy::format_duration(longest),
            "[{}]",
            backend.name()
        );
        ts.stop().await;
    }
}

async fn field_errors(
    ts: &TestServer,
    api: &Api,
    repo: &str,
    body: Value,
) -> Vec<(String, String)> {
    let response = put_settings(ts, api, repo, &body).await;
    assert_eq!(response.status(), 422);
    let body: Value = response.json().await.unwrap();
    body["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["path"].as_str().unwrap().to_string(),
                e["message"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_api_settings_field_errors() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;

        for pct in [0, 100, -3] {
            let mut body = input(false);
            body["repack_below_pct"] = json!(pct);
            assert_eq!(
                field_errors(&ts, &api, &repo, body).await,
                [(
                    "spec.repackBelow".to_string(),
                    "Enter a whole number from 1 to 99.".to_string()
                )],
                "[{}]",
                backend.name()
            );
        }
        for size in ["10GB", "0B", "512KiB", "ten"] {
            let mut body = input(false);
            body["max_repack"] = json!(size);
            assert_eq!(
                field_errors(&ts, &api, &repo, body).await,
                [(
                    "spec.maxRepack".to_string(),
                    "Use a size such as 10GiB or 500MiB.".to_string()
                )]
            );
        }
        let mut body = input(false);
        body["cron"] = json!("every day");
        body["timezone"] = json!("Nowhere/City");
        let paths: Vec<String> = field_errors(&ts, &api, &repo, body)
            .await
            .into_iter()
            .map(|e| e.0)
            .collect();
        assert_eq!(paths, ["spec.schedule.cron", "spec.schedule.timezone"]);
        assert_eq!(status(&api, &repo).await["settings"]["cron"], "0 3 * * 0");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_api_settings_validate_preview() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let path = format!("/api/v1/repositories/{repo}/gc/settings/validate");

        let mut body = input(true);
        body["cron"] = json!("0 5 * * 1");
        let response = api.post(&path, &body).await;
        assert_eq!(response.status(), 200, "[{}]", backend.name());
        let check: Value = response.json().await.unwrap();
        assert_eq!(check["schedule"]["next_runs"].as_array().unwrap().len(), 3);
        assert_eq!(check["problems"], json!([]));
        assert_eq!(check["pause_floor"], "24h");

        body["cron"] = json!("nonsense");
        body["pause"] = json!("1h");
        let check: Value = api.post(&path, &body).await.json().await.unwrap();
        assert!(check["schedule"].is_null());
        assert_eq!(check["problems"].as_array().unwrap().len(), 2);

        let shown = status(&api, &repo).await;
        assert_eq!(shown["enabled"], false);
        assert_eq!(shown["settings"]["cron"], "0 3 * * 0");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_api_enable_blocked_by_old_agent() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        seed_agent(&ts, &repo, "old", "active", Some(r#"["restore_v1"]"#)).await;
        seed_agent(&ts, &repo, "unknown", "active", None).await;
        seed_agent(&ts, &repo, "gone", "revoked", None).await;
        seed_agent(
            &ts,
            &repo,
            "new",
            "active",
            Some(r#"["restore_v1","gc_v1"]"#),
        )
        .await;

        let shown = status(&api, &repo).await;
        assert_eq!(problems(&shown), ["old_agents"], "[{}]", backend.name());
        let hosts: Vec<&str> = shown["preflight"]["problems"][0]["hosts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["hostname"].as_str().unwrap())
            .collect();
        assert_eq!(hosts, ["host-old", "host-unknown"]);
        assert_eq!(shown["preflight"]["dry_run_allowed"], true);

        let response = put_settings(&ts, &api, &repo, &input(true)).await;
        assert_eq!(response.status(), 409);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["message"], "Cleanup can't run yet.");
        assert_eq!(body["problems"][0]["kind"], "old_agents");
        assert_eq!(status(&api, &repo).await["enabled"], false);

        sqlx::query("UPDATE agents SET status = 'revoked' WHERE id IN ('old', 'unknown')")
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        let response = put_settings(&ts, &api, &repo, &input(true)).await;
        assert_eq!(response.status(), 200);
        assert_eq!(status(&api, &repo).await["enabled"], true);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_api_foreign_manifest_reported() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let gc_id = publish_manifest(&ts, "main").await;

        let shown = status(&api, &repo).await;
        assert_eq!(shown["enabled"], false);
        assert_eq!(problems(&shown), ["foreign_cycle"], "[{}]", backend.name());
        let response = put_settings(&ts, &api, &repo, &input(true)).await;
        assert_eq!(response.status(), 409);

        insert_cycle(&ts, &repo, "own-cycle", "waiting", 0).await;
        sqlx::query("UPDATE gc_cycles SET gc_id = $1 WHERE id = 'own-cycle'")
            .bind(&gc_id)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        assert!(problems(&status(&api, &repo).await).is_empty());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_api_enable_local_ok() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;

        for _ in 0..2 {
            let response = put_settings(&ts, &api, &repo, &input(true)).await;
            assert_eq!(response.status(), 200, "[{}]", backend.name());
            let saved: Value = response.json().await.unwrap();
            assert_eq!(saved["enabled"], true);
            assert_eq!(saved["settings"]["pause"], "24h");
            assert_eq!(saved["settings"]["max_repack"], "10GiB");
        }
        let off = put_settings(&ts, &api, &repo, &input(false)).await;
        assert_eq!(off.status(), 200);
        assert_eq!(status(&api, &repo).await["enabled"], false);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_api_settings_scope() {
    for backend in backends() {
        let ts = server(&backend).await;
        let repo = ts.create_local_repository("main").await;
        let url = format!("{}/api/v1/repositories/{repo}/gc/settings", ts.rest_url());

        let weak = bearer_client(&ts.issue_token(&["jobs:run"]).await);
        let response = weak.put(&url).json(&input(false)).send().await.unwrap();
        assert_eq!(response.status(), 403, "[{}]", backend.name());

        let strong = bearer_client(&ts.issue_token(&["snapshots:delete"]).await);
        let response = strong.put(&url).json(&input(false)).send().await.unwrap();
        assert_eq!(response.status(), 200);
        ts.stop().await;
    }
}

const SUMMARY: &str = r#"{"cycle_id":"c","dry_run":true,"finished_at":5,"duration_ms":10,"bytes_freed":4096,"packs_deleted":2,"packs_repacked":0,"repack_transfer_bytes":0,"locked_bytes":100,"locked_until":null,"locked_until_unknown":false}"#;

async fn jobs(api: &Api, query: &str) -> Value {
    let response = api.get(&format!("/api/v1/jobs?{query}")).await;
    assert_eq!(response.status(), 200, "{query}");
    response.json().await.unwrap()
}

fn kinds(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| {
            format!(
                "{}:{}",
                j["kind"].as_str().unwrap(),
                j["state"].as_str().unwrap()
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_api_jobs_list_rows() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        insert_cycle(&ts, &repo, "c-done", "done", 1).await;
        sqlx::query(
            "UPDATE gc_cycles SET summary_json = $1, finished_at = created_at + 5 \
             WHERE id = 'c-done'",
        )
        .bind(SUMMARY)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();

        let page = jobs(&api, "").await;
        assert_eq!(page["total"], 1, "[{}]", backend.name());
        let row = &page["items"][0];
        assert_eq!(row["id"], "c-done");
        assert_eq!(row["kind"], "gc");
        assert_eq!(row["state"], "succeeded");
        assert_eq!(row["host_id"], "");
        assert_eq!(row["hostname"], "");
        assert_eq!(row["origin"], "manual");
        assert!(row["finished_at"].as_i64().is_some());
        let gc = &row["gc"];
        assert_eq!(gc["dry_run"], true);
        assert_eq!(gc["repository_id"], repo);
        assert_eq!(gc["repository_name"], "main");
        assert_eq!(gc["phase"], "done");
        assert_eq!(gc["bytes_freed"], 4096);
        assert_eq!(gc["locked_bytes"], 100);
        assert_eq!(gc["postponed"], false);
        assert_eq!(gc["stopped_while_deleting"], false);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_api_jobs_filters() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let a = ts.create_local_repository("a").await;
        let b = ts.create_local_repository("b").await;
        let c = ts.create_local_repository("c").await;
        insert_cycle(&ts, &c, "c-done", "done", 1).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        insert_cycle(&ts, &a, "c-marking", "marking", 0).await;
        insert_cycle(&ts, &b, "c-waiting", "waiting", 0).await;
        sqlx::query(
            "UPDATE gc_cycles SET delete_after = created_at + 86400000 WHERE id = 'c-waiting'",
        )
        .execute(&ts.state().db.pool)
        .await
        .unwrap();

        let all = jobs(&api, "").await;
        assert_eq!(all["total"], 3, "[{}]", backend.name());
        let order = kinds(&all);
        assert_eq!(order[2], "gc:succeeded", "{order:?}");
        assert!(order[..2].contains(&"gc:running".to_string()));
        assert!(order[..2].contains(&"gc:waiting".to_string()));

        assert_eq!(jobs(&api, "kind=gc").await["total"], 3);
        assert_eq!(jobs(&api, "kind=backup").await["total"], 0);
        assert_eq!(jobs(&api, "kind=download").await["total"], 0);
        assert_eq!(jobs(&api, "status=running").await["total"], 2);
        assert_eq!(jobs(&api, "status=waiting").await["total"], 1);
        assert_eq!(jobs(&api, "status=succeeded").await["total"], 1);
        assert_eq!(jobs(&api, "status=failed").await["total"], 0);
        assert_eq!(jobs(&api, "status=queued").await["total"], 0);
        assert_eq!(jobs(&api, "host_id=anything").await["total"], 0);
        assert_eq!(jobs(&api, "host=a").await["total"], 0);

        let waiting = jobs(&api, "status=waiting").await;
        assert_eq!(waiting["items"][0]["gc"]["phase"], "waiting");
        assert!(waiting["items"][0]["gc"]["delete_after"].as_i64().is_some());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_api_jobs_state_mapping() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        insert_cycle(&ts, &repo, "cycle", "marking", 0).await;

        let cases = [
            ("marking", "running"),
            ("repacking", "running"),
            ("waiting", "waiting"),
            ("verifying", "running"),
            ("sweeping", "running"),
            ("done", "succeeded"),
            ("aborted", "cancelled"),
            ("failed", "failed"),
        ];
        for (db_state, expected) in cases {
            sqlx::query("UPDATE gc_cycles SET state = $1 WHERE id = 'cycle'")
                .bind(db_state)
                .execute(&ts.state().db.pool)
                .await
                .unwrap();
            let row = jobs(&api, "").await["items"][0].clone();
            assert_eq!(row["state"], expected, "[{}] {db_state}", backend.name());
            assert_eq!(row["gc"]["phase"], db_state);
            assert_eq!(row["gc"]["stopped_while_deleting"], false);
        }

        sqlx::query(
            "UPDATE gc_cycles SET sweep_started = 1, error = 'disk full', progress_json = $1 \
             WHERE id = 'cycle'",
        )
        .bind(r#"{"waiting_for_restores":true}"#)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
        let row = jobs(&api, "").await["items"][0].clone();
        assert_eq!(row["state"], "failed");
        assert_eq!(row["gc"]["stopped_while_deleting"], true);
        assert_eq!(row["gc"]["postponed"], true);
        assert_eq!(row["error_message"], "disk full");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_api_jobs_detail_404() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        insert_cycle(&ts, &repo, "cycle", "waiting", 0).await;

        let response = api.get("/api/v1/jobs/cycle").await;
        assert_eq!(response.status(), 404, "[{}]", backend.name());
        let response = api.get("/api/v1/jobs/cycle/events").await;
        assert_eq!(response.status(), 404);
        ts.stop().await;
    }
}

async fn cleanup(api: &Api) -> Value {
    let list: Value = api.get("/api/v1/repositories").await.json().await.unwrap();
    list[0]["cleanup"].clone()
}

/// The repository list carries a cleanup state read from the catalog alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_api_repository_list_cleanup_summary() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        assert_eq!(
            cleanup(&api).await,
            json!({"state": "off", "at": null}),
            "[{}]",
            backend.name()
        );

        assert_eq!(
            put_settings(&ts, &api, &repo, &input(true)).await.status(),
            200
        );
        let idle = cleanup(&api).await;
        assert_eq!(idle["state"], "idle");
        assert!(idle["at"].as_i64().is_some());

        insert_cycle(&ts, &repo, "real", "marking", 0).await;
        assert_eq!(cleanup(&api).await["state"], "running");

        sqlx::query("UPDATE gc_cycles SET state = 'waiting', delete_after = 77 WHERE id = 'real'")
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        assert_eq!(cleanup(&api).await, json!({"state": "waiting", "at": 77}));

        sqlx::query("UPDATE gc_cycles SET state = 'failed', finished_at = 5 WHERE id = 'real'")
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        assert_eq!(cleanup(&api).await["state"], "stopped");

        sqlx::query("UPDATE gc_cycles SET state = 'garbled' WHERE id = 'real'")
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        assert_eq!(
            cleanup(&api).await["state"],
            "unknown",
            "a broken cleanup row must not take the list down"
        );
        ts.stop().await;
    }
}

const OPEN_ID: &str = "11111111111111111111111111111111111111111111111111111111111111aa";
const BOUND_ID: &str = "22222222222222222222222222222222222222222222222222222222222222bb";

async fn quarantine_row(ts: &TestServer, repo: &str, id: &str, confirmable: i64) {
    sqlx::query(
        "INSERT INTO gc_quarantine (repository_id, snapshot_id, object_key, reason, confirmable, \
         detected_at) VALUES ($1, $2, $3, 'does not open', $4, 1)",
    )
    .bind(repo)
    .bind(id)
    .bind(format!("snapshots/{id}"))
    .bind(confirmable)
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
}

fn exclude_path(repo: &str, id: &str) -> String {
    format!("/api/v1/repositories/{repo}/gc/quarantine/{id}/exclude")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_api_exclude_requires_snapshots_delete() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        quarantine_row(&ts, &repo, OPEN_ID, 1).await;
        let path = exclude_path(&repo, OPEN_ID);
        let url = format!("{}{path}", ts.rest_url());

        for scope in ["jobs:run", "jobs:read"] {
            let weak = bearer_client(&ts.issue_token(&[scope]).await);
            let response = weak.post(&url).json(&json!({})).send().await.unwrap();
            assert_eq!(response.status(), 403, "[{}] {scope}", backend.name());
        }
        let response = reqwest::Client::new()
            .post(&url)
            .header("cookie", api.cookie())
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "[{}] without csrf", backend.name());
        assert!(status(&api, &repo).await["quarantine"][0]["confirmed_at"].is_null());

        let response = api.post(&path, &json!({})).await;
        assert_eq!(response.status(), 200, "[{}]", backend.name());
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["quarantine"][0]["snapshot_id"], OPEN_ID);
        assert!(body["quarantine"][0]["confirmed_at"].is_i64());
        assert!(body["quarantine"][0]["confirmed_by"].is_string());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_api_exclude_refuses_revoked_expired_and_other_repository() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let other = ts.create_local_repository("other").await;
        quarantine_row(&ts, &repo, OPEN_ID, 1).await;
        let path = exclude_path(&repo, OPEN_ID);
        let url = format!("{}{path}", ts.rest_url());

        let mut issued = Vec::new();
        for name in ["revoked", "expired", "valid"] {
            let created = api
                .post(
                    "/api/v1/tokens",
                    &json!({ "name": name, "scopes": ["snapshots:delete"], "expires_in_days": null }),
                )
                .await;
            assert_eq!(created.status(), 201);
            let body: Value = created.json().await.unwrap();
            issued.push((
                body["item"]["id"].as_str().unwrap().to_string(),
                body["token"].as_str().unwrap().to_string(),
            ));
        }
        let revoke = api
            .post(
                &format!("/api/v1/tokens/{}/revoke", issued[0].0),
                &json!({}),
            )
            .await;
        assert_eq!(revoke.status(), 204);
        sqlx::query("UPDATE api_tokens SET expires_at = $1 WHERE id = $2")
            .bind(1_i64)
            .bind(&issued[1].0)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();

        for (what, (_, token)) in ["revoked", "expired"].iter().zip(&issued) {
            let response = bearer_client(token)
                .post(&url)
                .json(&json!({}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 401, "[{}] {what}", backend.name());
        }
        assert!(status(&api, &repo).await["quarantine"][0]["confirmed_at"].is_null());

        let across = api.post(&exclude_path(&other, OPEN_ID), &json!({})).await;
        assert_eq!(
            across.status(),
            404,
            "[{}] other repository",
            backend.name()
        );
        assert!(status(&api, &repo).await["quarantine"][0]["confirmed_at"].is_null());

        let valid = bearer_client(&issued[2].1)
            .post(&url)
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(valid.status(), 200, "[{}]", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_api_exclude_refuses_bound_and_unknown() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        quarantine_row(&ts, &repo, OPEN_ID, 1).await;
        quarantine_row(&ts, &repo, BOUND_ID, 0).await;

        let bound = api.post(&exclude_path(&repo, BOUND_ID), &json!({})).await;
        assert_eq!(bound.status(), 409, "[{}]", backend.name());
        let unknown = "3".repeat(64);
        let response = api.post(&exclude_path(&repo, &unknown), &json!({})).await;
        assert_eq!(response.status(), 404);
        let response = api.post(&exclude_path(&repo, "not-hex"), &json!({})).await;
        assert_eq!(response.status(), 404);
        let response = api
            .post(&exclude_path("missing", OPEN_ID), &json!({}))
            .await;
        assert_eq!(response.status(), 404);

        let first = api.post(&exclude_path(&repo, OPEN_ID), &json!({})).await;
        assert_eq!(first.status(), 200);
        let again = api.post(&exclude_path(&repo, OPEN_ID), &json!({})).await;
        assert_eq!(again.status(), 409);
        let listed = status(&api, &repo).await["quarantine"].clone();
        let bound_row = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["snapshot_id"] == BOUND_ID)
            .unwrap();
        assert_eq!(bound_row["confirmable"], false);
        assert!(bound_row["confirmed_at"].is_null());
        ts.stop().await;
    }
}
