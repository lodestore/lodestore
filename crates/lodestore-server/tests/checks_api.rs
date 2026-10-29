mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;

use common::{Api, TestServer, WAIT, backends, bearer_client, wait_until};
use lodestore_format::index::hex32;
use lodestore_format::repo::Repository;
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, Writer};
use serde_json::{Value, json};
use sqlx::Row;

const PASSWORD: &[u8] = b"test-password";

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

async fn server(backend: &common::Backend) -> TestServer {
    TestServer::start_with(backend, |c| c.schedule_tick_secs = 0).await
}

fn checks_url(repo: &str) -> String {
    format!("/api/v1/repositories/{repo}/checks")
}

async fn status(api: &Api, repo: &str) -> Value {
    let response = api.get(&checks_url(repo)).await;
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

async fn start(api: &Api, repo: &str, level: &str) -> reqwest::Response {
    api.post(&format!("{}/{level}/run", checks_url(repo)), &json!({}))
        .await
}

async fn run(api: &Api, repo: &str, run_id: &str) -> Value {
    let response = api
        .get(&format!("{}/runs/{run_id}", checks_url(repo)))
        .await;
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

async fn run_to_the_end(api: &Api, repo: &str, level: &str) -> Value {
    let response = start(api, repo, level).await;
    assert_eq!(response.status(), 202);
    let started: Value = response.json().await.unwrap();
    let run_id = started["run_id"].as_str().unwrap().to_string();
    let finished = wait_until(WAIT, || async {
        !matches!(
            run(api, repo, &run_id).await["state"].as_str(),
            Some("queued" | "running")
        )
    })
    .await;
    assert!(finished, "the {level} check did not finish");
    run(api, repo, &run_id).await
}

async fn report(api: &Api, repo: &str, run_id: &str) -> Value {
    let response = api
        .get(&format!("{}/runs/{run_id}/report", checks_url(repo)))
        .await;
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

fn pack_files(ts: &TestServer, name: &str) -> Vec<PathBuf> {
    fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(&ts.local_repo_root(name).join("packs"), &mut out);
    out.sort();
    out
}

async fn insert_run(ts: &TestServer, repo: &str, id: &str, state: &str) {
    sqlx::query(
        "INSERT INTO check_runs (id, repository_id, level, origin, started_by, state, created_at) \
         VALUES ($1, $2, 'structure', 'manual', 'test', $3, $4)",
    )
    .bind(id)
    .bind(repo)
    .bind(state)
    .bind(lodestore_server::db::now_ms())
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
}

async fn insert_cycle(ts: &TestServer, repo: &str, id: &str, state: &str) {
    sqlx::query(
        "INSERT INTO gc_cycles (id, repository_id, state, dry_run, origin, started_by, \
         settings_json, created_at, started_at) VALUES ($1, $2, $3, 0, 'manual', 'test', $4, $5, $5)",
    )
    .bind(id)
    .bind(repo)
    .bind(state)
    .bind(r#"{"pause_ms":86400000,"repack_below_pct":50,"max_repack_bytes":1073741824}"#)
    .bind(lodestore_server::db::now_ms())
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_api_structure_run() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        write_snapshot(&ts, "main", "a").await;
        write_snapshot(&ts, "main", "b").await;

        let before = status(&api, &repo).await;
        let levels = before["levels"].as_array().unwrap();
        assert_eq!(levels.len(), 3);
        assert_eq!(levels[0]["level"], "structure");
        assert_eq!(levels[0]["settings"]["cron"], "0 2 * * *");
        assert_eq!(levels[0]["settings"]["enabled"], true);
        assert_eq!(levels[1]["settings"]["cron"], "0 4 * * 3");
        assert_eq!(levels[1]["settings"]["read_per_run"], "5GiB");
        assert!(levels[0]["schedule"]["next_runs"][0].as_i64().is_some());
        assert!(levels[0]["last"].is_null());
        assert!(before["usage"].is_null());
        assert!(before["blocked"].is_null());

        let finished = run_to_the_end(&api, &repo, "structure").await;
        assert_eq!(finished["state"], "done", "[{}] {finished}", backend.name());
        assert_eq!(finished["outcome"], "passed");
        assert_eq!(finished["findings_total"], 0);
        assert_eq!(finished["summary"]["structure"]["snapshots_checked"], 2);
        assert_eq!(finished["origin"], "manual");

        let after = status(&api, &repo).await;
        assert_eq!(after["levels"][0]["last"]["outcome"], "passed");
        assert!(after["levels"][0]["running"].is_null());
        assert!(after["usage"]["physical_bytes"].as_u64().unwrap() > 0);
        assert!(after["usage"]["packs"].as_u64().unwrap() > 0);

        let fleet: Value = api.get("/api/v1/fleet").await.json().await.unwrap();
        assert!(fleet["storage"]["physical_bytes"].as_u64().unwrap() > 0);
        // The snapshots bypass the catalog, so the logical size is unknown and no ratio is shown.
        assert!(fleet["storage"]["dedup_ratio"].is_null());

        let full = report(&api, &repo, finished["id"].as_str().unwrap()).await;
        assert_eq!(full["run"]["id"], finished["id"]);
        assert!(full["findings"].as_array().unwrap().is_empty());
        assert_eq!(full["notes_total"], 0, "{full}");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_api_structure_missing_pack() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        write_snapshot(&ts, "main", "a").await;
        let packs = pack_files(&ts, "main");
        assert!(!packs.is_empty());
        std::fs::remove_file(&packs[0]).unwrap();

        let finished = run_to_the_end(&api, &repo, "structure").await;
        assert_eq!(
            finished["state"],
            "failed",
            "[{}] {finished}",
            backend.name()
        );
        assert_eq!(finished["outcome"], "problems");
        assert!(finished["findings_total"].as_u64().unwrap() >= 1);
        let full = report(&api, &repo, finished["id"].as_str().unwrap()).await;
        let findings = full["findings"].as_array().unwrap();
        assert!(
            findings.iter().any(|f| f["kind"] == "missing_pack"),
            "{full}"
        );
        assert_eq!(finished["findings"][0]["kind"], "missing_pack");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_api_jobs_rows() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        write_snapshot(&ts, "main", "a").await;
        let finished = run_to_the_end(&api, &repo, "structure").await;
        assert_eq!(finished["state"], "done");

        let page: Value = api
            .get("/api/v1/jobs?kind=check")
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(page["total"], 1, "[{}]", backend.name());
        let row = &page["items"][0];
        assert_eq!(row["id"], finished["id"]);
        assert_eq!(row["kind"], "check");
        assert_eq!(row["state"], "succeeded");
        assert_eq!(row["host_id"], "");
        assert_eq!(row["hostname"], "");
        assert_eq!(row["origin"], "manual");
        assert!(row["finished_at"].as_i64().is_some());
        let check = &row["check"];
        assert_eq!(check["level"], "structure");
        assert_eq!(check["repository_id"], repo);
        assert_eq!(check["repository_name"], "main");
        assert_eq!(check["outcome"], "passed");
        assert_eq!(check["findings_total"], 0);

        let all: Value = api.get("/api/v1/jobs").await.json().await.unwrap();
        assert_eq!(all["total"], 1);
        let backups: Value = api
            .get("/api/v1/jobs?kind=backup")
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(backups["total"], 0);
        let failed: Value = api
            .get("/api/v1/jobs?kind=check&status=failed")
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(failed["total"], 0);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_api_one_at_a_time() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        insert_run(&ts, &repo, "busy", "running").await;

        let response = start(&api, &repo, "structure").await;
        assert_eq!(response.status(), 409, "[{}]", backend.name());
        let body: Value = response.json().await.unwrap();
        assert_eq!(
            body["message"],
            "Another check is running on this repository."
        );
        let shown = status(&api, &repo).await;
        assert_eq!(shown["blocked"]["reason"], "check");
        assert_eq!(shown["levels"][0]["running"]["id"], "busy");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_api_blocked_by_sweeping() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        insert_cycle(&ts, &repo, "gc-1", "sweeping").await;

        let response = start(&api, &repo, "structure").await;
        assert_eq!(response.status(), 409, "[{}]", backend.name());
        let body: Value = response.json().await.unwrap();
        assert_eq!(
            body["message"],
            "A cleanup cycle is running. Checks wait for it."
        );
        let shown = status(&api, &repo).await;
        assert_eq!(shown["blocked"]["reason"], "cleanup");

        sqlx::query("UPDATE gc_cycles SET state = 'repacking' WHERE id = 'gc-1'")
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        assert_eq!(start(&api, &repo, "structure").await.status(), 409);

        sqlx::query("UPDATE gc_cycles SET state = 'waiting' WHERE id = 'gc-1'")
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        assert_eq!(start(&api, &repo, "structure").await.status(), 202);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_api_unknown_level_and_repo() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;

        let response = start(&api, &repo, "deep").await;
        assert_eq!(response.status(), 422, "[{}]", backend.name());
        assert_eq!(start(&api, "no-such-repo", "structure").await.status(), 404);
        assert_eq!(api.get(&checks_url("no-such-repo")).await.status(), 404);
        assert_eq!(
            api.get(&format!("{}/runs/no-such-run", checks_url(&repo)))
                .await
                .status(),
            404
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_api_run_needs_jobs_run() {
    for backend in backends() {
        let ts = server(&backend).await;
        let repo = ts.create_local_repository("main").await;
        let url = format!("{}{}/structure/run", ts.rest_url(), checks_url(&repo));

        let reader = bearer_client(&ts.issue_token(&["jobs:read"]).await);
        let response = reader.post(&url).json(&json!({})).send().await.unwrap();
        assert_eq!(response.status(), 403, "[{}]", backend.name());
        let status_url = format!("{}{}", ts.rest_url(), checks_url(&repo));
        assert_eq!(reader.get(&status_url).send().await.unwrap().status(), 200);

        let runner = bearer_client(&ts.issue_token(&["jobs:run"]).await);
        assert_eq!(runner.get(&status_url).send().await.unwrap().status(), 403);
        let response = runner.post(&url).json(&json!({})).send().await.unwrap();
        assert_eq!(response.status(), 202);
        ts.stop().await;
    }
}

fn pack_name(path: &std::path::Path) -> String {
    path.file_name().unwrap().to_string_lossy().into_owned()
}

fn pack_size(ts: &TestServer, name: &str, id: &str) -> u64 {
    pack_files(ts, name)
        .iter()
        .find(|path| pack_name(path) == id)
        .map(|path| std::fs::metadata(path).unwrap().len())
        .unwrap()
}

async fn pack_rows(ts: &TestServer) -> BTreeMap<String, (Option<i64>, bool)> {
    sqlx::query("SELECT pack_id, checked_at, bad FROM pack_checks ORDER BY pack_id")
        .fetch_all(&ts.state().db.pool)
        .await
        .unwrap()
        .iter()
        .map(|row| {
            (
                row.get::<String, _>("pack_id"),
                (
                    row.get::<Option<i64>, _>("checked_at"),
                    row.get::<i64, _>("bad") != 0,
                ),
            )
        })
        .collect()
}

async fn set_read_limit(ts: &TestServer, repo: &str, bytes: u64) {
    sqlx::query("DELETE FROM check_settings WHERE repository_id = $1 AND level = 'packs'")
        .bind(repo)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO check_settings (repository_id, level, enabled, cron, timezone, limits_json, \
         updated_at) VALUES ($1, 'packs', 1, '0 4 * * 3', 'UTC', $2, $3)",
    )
    .bind(repo)
    .bind(format!(r#"{{"read_per_run":{bytes}}}"#))
    .bind(lodestore_server::db::now_ms())
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
}

async fn five_snapshots(ts: &TestServer, name: &str) {
    for seed in ["a", "b", "c", "d", "e"] {
        write_snapshot(ts, name, seed).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_api_packs_rotation() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        five_snapshots(&ts, "main").await;
        let files = pack_files(&ts, "main");
        let total = files.len();
        assert!(total >= 4, "[{}] {total} packs", backend.name());

        let mut by_id: Vec<(String, u64)> = files
            .iter()
            .map(|p| (pack_name(p), std::fs::metadata(p).unwrap().len()))
            .collect();
        by_id.sort();
        set_read_limit(&ts, &repo, by_id[0].1 + by_id[1].1).await;
        let first = run_to_the_end(&api, &repo, "packs").await;
        assert_eq!(first["state"], "done", "[{}] {first}", backend.name());
        assert_eq!(first["summary"]["packs"]["packs_read"], 2);
        let rows = pack_rows(&ts).await;
        assert_eq!(rows.len(), total);
        for (index, (id, _)) in by_id.iter().enumerate() {
            assert_eq!(rows[id].0.is_some(), index < 2, "{id}");
        }
        let shown = status(&api, &repo).await;
        assert_eq!(shown["coverage"]["never_checked"], total - 2);
        assert!(shown["coverage"]["checked_30d_pct"].as_u64().unwrap() < 100);

        let mut runs = 1;
        while pack_rows(&ts).await.values().any(|(at, _)| at.is_none()) {
            runs += 1;
            assert!(runs <= total, "rotation did not cover every pack");
            let before = pack_rows(&ts).await;
            let never: Vec<&String> = before
                .iter()
                .filter(|(_, (at, _))| at.is_none())
                .map(|(id, _)| id)
                .collect();
            let due = never[0];
            set_read_limit(&ts, &repo, pack_size(&ts, "main", due)).await;
            let finished = run_to_the_end(&api, &repo, "packs").await;
            assert_eq!(finished["state"], "done", "{finished}");
            let after = pack_rows(&ts).await;
            assert!(
                after[due].0.is_some(),
                "{due} was never read, so it goes first"
            );
        }
        let shown = status(&api, &repo).await;
        assert_eq!(shown["coverage"]["never_checked"], 0);
        assert_eq!(shown["coverage"]["checked_30d_pct"], 100);

        let before = pack_rows(&ts).await;
        let oldest = before
            .iter()
            .min_by_key(|(id, (at, _))| (*at, (*id).clone()))
            .map(|(id, _)| id.clone())
            .unwrap();
        set_read_limit(&ts, &repo, 1).await;
        run_to_the_end(&api, &repo, "packs").await;
        let after = pack_rows(&ts).await;
        assert!(
            after[&oldest].0 > before[&oldest].0,
            "the longest unread pack goes first"
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_api_unreadable_pack_does_not_stall_rotation() {
    use std::os::unix::fs::PermissionsExt;

    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        five_snapshots(&ts, "main").await;
        let mut files = pack_files(&ts, "main");
        files.sort_by_key(|path| pack_name(path));
        let total = files.len();
        assert!(total >= 4, "[{}] {total} packs", backend.name());
        let locked = &files[0];
        std::fs::set_permissions(locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(locked).is_ok() {
            std::fs::set_permissions(locked, std::fs::Permissions::from_mode(0o644)).unwrap();
            ts.stop().await;
            return;
        }

        set_read_limit(&ts, &repo, u64::MAX / 2).await;
        let finished = run_to_the_end(&api, &repo, "packs").await;
        std::fs::set_permissions(locked, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            finished["state"],
            "failed",
            "[{}] {finished}",
            backend.name()
        );
        assert_eq!(finished["outcome"], "stopped");
        assert!(
            finished["error"]
                .as_str()
                .unwrap()
                .starts_with("Some packs could not be read"),
            "{finished}"
        );
        let rows = pack_rows(&ts).await;
        let broken = pack_name(locked);
        for (id, (checked, _)) in &rows {
            assert_eq!(checked.is_some(), *id != broken, "{id}");
        }
        assert!(status(&api, &repo).await["levels"][1]["last"].is_null());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_api_bad_pack_first() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        five_snapshots(&ts, "main").await;
        run_to_the_end(&api, &repo, "packs").await;
        let before = pack_rows(&ts).await;
        let last = before.keys().next_back().unwrap().clone();
        sqlx::query("UPDATE pack_checks SET bad = 1 WHERE pack_id = $1")
            .bind(&last)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        let marked = pack_rows(&ts).await;
        set_read_limit(&ts, &repo, 1).await;

        let finished = run_to_the_end(&api, &repo, "packs").await;
        assert_eq!(
            finished["summary"]["packs"]["packs_read"],
            1,
            "[{}]",
            backend.name()
        );
        let after = pack_rows(&ts).await;
        assert!(
            after[&last].0 > marked[&last].0,
            "the bad pack is read first"
        );
        assert!(!after[&last].1, "an intact pack stops being bad");
        for (id, row) in &after {
            if *id != last {
                assert_eq!(row, &marked[id], "{id} was not due");
            }
        }
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_api_corrupted_pack_fails() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        five_snapshots(&ts, "main").await;
        let total = pack_files(&ts, "main").len() as u64;

        let clean = run_to_the_end(&api, &repo, "packs").await;
        assert_eq!(clean["state"], "done", "[{}] {clean}", backend.name());
        assert_eq!(clean["summary"]["packs"]["packs_read"], total);
        let rows = pack_rows(&ts).await;
        assert!(rows.values().all(|(at, bad)| at.is_some() && !bad));
        let stamps: std::collections::BTreeSet<_> = rows.values().map(|(at, _)| *at).collect();
        assert_eq!(stamps.len(), 1, "one run, one time");
        let shown = status(&api, &repo).await;
        assert_eq!(shown["coverage"]["checked_30d_pct"], 100);
        assert_eq!(shown["coverage"]["never_checked"], 0);

        let victim = pack_files(&ts, "main")[0].clone();
        let name = pack_name(&victim);
        let original = std::fs::read(&victim).unwrap();
        let mut damaged = original.clone();
        let middle = damaged.len() / 2;
        damaged[middle] ^= 0xff;
        std::fs::write(&victim, &damaged).unwrap();

        let failed = run_to_the_end(&api, &repo, "packs").await;
        assert_eq!(failed["state"], "failed", "[{}] {failed}", backend.name());
        assert_eq!(failed["outcome"], "problems");
        assert!(failed["findings_total"].as_u64().unwrap() >= 1);
        let full = report(&api, &repo, failed["id"].as_str().unwrap()).await;
        assert!(
            full["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["key"].as_str().unwrap().ends_with(&name)),
            "{full}"
        );
        assert!(
            pack_rows(&ts).await[&name].1,
            "the pack is remembered as bad"
        );
        let listed: Value = api
            .get("/api/v1/jobs?kind=check&status=failed")
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(listed["items"][0]["check"]["outcome"], "problems");

        std::fs::write(&victim, &original).unwrap();
        let healed = run_to_the_end(&api, &repo, "packs").await;
        assert_eq!(healed["state"], "done", "{healed}");
        assert_eq!(healed["outcome"], "passed");
        assert!(!pack_rows(&ts).await[&name].1);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_api_pack_rows_pruned() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        five_snapshots(&ts, "main").await;
        run_to_the_end(&api, &repo, "packs").await;
        let before = pack_rows(&ts).await;

        let gone = pack_files(&ts, "main")[0].clone();
        let name = pack_name(&gone);
        std::fs::remove_file(&gone).unwrap();
        let finished = run_to_the_end(&api, &repo, "packs").await;
        assert_eq!(finished["state"], "done", "[{}] {finished}", backend.name());

        let after = pack_rows(&ts).await;
        assert_eq!(after.len(), before.len() - 1);
        assert!(!after.contains_key(&name));
        ts.stop().await;
    }
}

async fn cancel(api: &Api, repo: &str, run_id: &str) -> reqwest::Response {
    api.post(
        &format!("{}/runs/{run_id}/cancel", checks_url(repo)),
        &json!({}),
    )
    .await
}

async fn exec(ts: &TestServer, sql: &str) {
    sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_api_cancel() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let other = ts.create_local_repository("other").await;
        write_snapshot(&ts, "main", "a").await;

        insert_run(&ts, &repo, "queued-run", "queued").await;
        assert_eq!(cancel(&api, &repo, "queued-run").await.status(), 202);
        let ended = run(&api, &repo, "queued-run").await;
        assert_eq!(ended["state"], "aborted", "[{}]", backend.name());
        assert_eq!(cancel(&api, &repo, "queued-run").await.status(), 409);
        let jobs: Value = api
            .get("/api/v1/jobs?kind=check&status=cancelled")
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(jobs["items"][0]["id"], "queued-run");
        assert_eq!(jobs["items"][0]["state"], "cancelled");
        assert!(status(&api, &repo).await["levels"][0]["last"].is_null());

        assert_eq!(cancel(&api, &repo, "no-such-run").await.status(), 404);
        insert_run(&ts, &repo, "running-run", "running").await;
        assert_eq!(cancel(&api, &other, "running-run").await.status(), 404);
        assert_eq!(cancel(&api, &repo, "running-run").await.status(), 202);
        let requested =
            sqlx::query("SELECT cancel_requested FROM check_runs WHERE id = 'running-run'")
                .fetch_one(&ts.state().db.pool)
                .await
                .unwrap()
                .get::<i64, _>("cancel_requested");
        assert_eq!(requested, 1);
        assert_eq!(run(&api, &repo, "running-run").await["state"], "running");

        exec(
            &ts,
            "UPDATE check_runs SET state = 'failed' WHERE id = 'running-run'",
        )
        .await;
        insert_run(&ts, &repo, "driven-run", "queued").await;
        exec(
            &ts,
            "UPDATE check_runs SET cancel_requested = 1 WHERE id = 'driven-run'",
        )
        .await;
        lodestore_server::maintenance::check::run::run_check(ts.state(), "driven-run").await;
        let driven = run(&api, &repo, "driven-run").await;
        assert_eq!(driven["state"], "aborted", "{driven}");
        assert!(driven["outcome"].is_null());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_api_metrics_scope_issued() {
    for backend in backends() {
        let ts = server(&backend).await;
        let repo = ts.create_local_repository("main").await;
        let token = ts.issue_token(&["metrics:read"]).await;
        let listed: Value = ts
            .api()
            .await
            .get("/api/v1/tokens")
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(
            listed[0]["scopes"],
            json!(["metrics:read"]),
            "[{}] {listed}",
            backend.name()
        );
        let client = bearer_client(&token);
        let url = format!("{}{}", ts.rest_url(), checks_url(&repo));
        assert_eq!(client.get(&url).send().await.unwrap().status(), 403);
        ts.stop().await;
    }
}

fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

async fn write_files(ts: &TestServer, name: &str, seed: u64, files: usize, size: usize) {
    let store = LocalFsStore::new(ts.local_repo_root(name));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    let mut writer = Writer::new(&store, keys, repo.config()).await.unwrap();
    let data = ts.dir.path().join(format!("files-{seed}"));
    std::fs::create_dir_all(&data).unwrap();
    for n in 0..files {
        std::fs::write(
            data.join(format!("file-{n}.bin")),
            noise(seed * 1000 + n as u64, size),
        )
        .unwrap();
    }
    writer
        .backup(&[data], BackupOptions::default(), &|_| {})
        .await
        .expect("backup");
}

async fn set_restore_limits(ts: &TestServer, repo: &str, limits: &str) {
    exec(
        ts,
        &format!(
            "INSERT INTO check_settings (repository_id, level, enabled, cron, timezone, \
             limits_json, updated_at) VALUES ('{repo}', 'restore', 1, '0 5 2 * *', 'UTC', \
             '{limits}', {})",
            lodestore_server::db::now_ms()
        ),
    )
    .await;
}

fn scratch_of(ts: &TestServer, run_id: &str) -> PathBuf {
    ts.dir
        .path()
        .join("data")
        .join("check-restore")
        .join(run_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_restore_passes() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        for seed in 1..=3 {
            write_files(&ts, "main", seed, 10, 20_000).await;
        }
        set_restore_limits(&ts, &repo, r#"{"files_per_run":5}"#).await;

        let finished = run_to_the_end(&api, &repo, "restore").await;
        assert_eq!(finished["state"], "done", "[{}] {finished}", backend.name());
        assert_eq!(finished["outcome"], "passed");
        let summary = &finished["summary"]["restore"];
        assert_eq!(summary["files_restored"], 5, "{finished}");
        assert_eq!(summary["snapshots_sampled"], 3);
        assert_eq!(summary["hash_mismatches"], 0);
        assert_eq!(summary["stopped_at_limit"], false);
        assert_eq!(summary["bytes_restored"], 5 * 20_000);
        assert!(!scratch_of(&ts, finished["id"].as_str().unwrap()).exists());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_restore_damaged_file() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        write_files(&ts, "main", 1, 1, 200_000).await;
        let victim = pack_files(&ts, "main")
            .into_iter()
            .max_by_key(|path| std::fs::metadata(path).unwrap().len())
            .unwrap();
        let mut damaged = std::fs::read(&victim).unwrap();
        let middle = damaged.len() / 2;
        damaged[middle] ^= 0xff;
        std::fs::write(&victim, &damaged).unwrap();

        let finished = run_to_the_end(&api, &repo, "restore").await;
        assert_eq!(
            finished["state"],
            "failed",
            "[{}] {finished}",
            backend.name()
        );
        assert_eq!(finished["outcome"], "problems");
        assert_eq!(finished["summary"]["restore"]["hash_mismatches"], 1);
        let full = report(&api, &repo, finished["id"].as_str().unwrap()).await;
        assert!(
            full["findings"][0]["key"]
                .as_str()
                .unwrap()
                .ends_with("file-0.bin"),
            "{full}"
        );
        assert!(!scratch_of(&ts, finished["id"].as_str().unwrap()).exists());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_restore_size_limit() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        write_files(&ts, "main", 1, 2, 4 << 20).await;
        set_restore_limits(&ts, &repo, r#"{"max_data_per_run":1048576}"#).await;

        let finished = run_to_the_end(&api, &repo, "restore").await;
        assert_eq!(
            finished["state"],
            "failed",
            "[{}] {finished}",
            backend.name()
        );
        assert_eq!(finished["outcome"], "stopped");
        assert_eq!(finished["error"], "Stopped at the size limit.");
        assert_eq!(finished["summary"]["restore"]["stopped_at_limit"], true);
        assert_eq!(finished["summary"]["restore"]["files_restored"], 0);
        assert!(!scratch_of(&ts, finished["id"].as_str().unwrap()).exists());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_restore_without_files_is_no_verdict() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;

        let finished = run_to_the_end(&api, &repo, "restore").await;
        assert_eq!(
            finished["state"],
            "failed",
            "[{}] {finished}",
            backend.name()
        );
        assert_eq!(finished["outcome"], "stopped");
        assert_eq!(finished["error"], "There were no files to restore.");
        assert!(status(&api, &repo).await["levels"][2]["last"].is_null());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_restore_cancel() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        write_files(&ts, "main", 1, 3, 20_000).await;
        exec(
            &ts,
            &format!(
                "INSERT INTO check_runs (id, repository_id, level, origin, started_by, state, \
                 cancel_requested, created_at) VALUES ('restore-run', '{repo}', 'restore', \
                 'manual', 'test', 'queued', 1, {})",
                lodestore_server::db::now_ms()
            ),
        )
        .await;

        lodestore_server::maintenance::check::run::run_check(ts.state(), "restore-run").await;
        let driven = run(&api, &repo, "restore-run").await;
        assert_eq!(driven["state"], "aborted", "[{}] {driven}", backend.name());
        assert!(driven["outcome"].is_null());
        assert!(!scratch_of(&ts, "restore-run").exists());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_api_restore_defaults() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;

        let shown = status(&api, &repo).await;
        let levels = shown["levels"].as_array().unwrap();
        assert_eq!(levels.len(), 3, "[{}]", backend.name());
        let restore = &levels[2];
        assert_eq!(restore["level"], "restore");
        assert_eq!(restore["settings"]["enabled"], true);
        assert_eq!(restore["settings"]["cron"], "0 5 2 * *");
        assert_eq!(restore["settings"]["timezone"], "UTC");
        assert_eq!(restore["settings"]["files_per_run"], 20);
        assert_eq!(restore["settings"]["max_data_per_run"], "1GiB");
        assert!(restore["schedule"]["next_runs"][0].as_i64().is_some());
        ts.stop().await;
    }
}

fn settings_body() -> Value {
    let level = |cron: &str| json!({ "enabled": true, "cron": cron, "timezone": "UTC" });
    let mut packs = level("0 4 * * 3");
    packs["read_per_run"] = json!("5GiB");
    let mut restore = level("0 5 2 * *");
    restore["files_per_run"] = json!(20);
    restore["max_data_per_run"] = json!("1GiB");
    json!({ "structure": level("0 2 * * *"), "packs": packs, "restore": restore })
}

fn settings_url(ts: &TestServer, repo: &str) -> String {
    format!("{}{}/settings", ts.rest_url(), checks_url(repo))
}

async fn put_settings(ts: &TestServer, api: &Api, repo: &str, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .put(settings_url(ts, repo))
        .header("cookie", api.cookie())
        .header("x-csrf-token", &api.csrf)
        .json(body)
        .send()
        .await
        .unwrap()
}

async fn settings_rows(ts: &TestServer, repo: &str) -> i64 {
    sqlx::query("SELECT COUNT(*) AS n FROM check_settings WHERE repository_id = $1")
        .bind(repo)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .get("n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_api_settings_errors() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;

        let mut body = settings_body();
        body["restore"]["files_per_run"] = json!(0);
        body["packs"]["read_per_run"] = json!("5GB");
        body["restore"]["max_data_per_run"] = json!("100KiB");
        body["structure"]["cron"] = json!("every day");
        body["packs"]["timezone"] = json!("Mars/Base");
        let response = put_settings(&ts, &api, &repo, &body).await;
        assert_eq!(response.status(), 422, "[{}]", backend.name());
        let failed: Value = response.json().await.unwrap();
        let message = |path: &str| {
            failed["errors"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["path"] == path)
                .unwrap_or_else(|| panic!("no error for {path}: {failed}"))["message"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(
            message("restore.filesPerRun"),
            "Enter a whole number from 1 to 1000."
        );
        assert_eq!(
            message("packs.readPerRun"),
            "Use a size such as 5GiB or 500MiB."
        );
        assert_eq!(
            message("restore.maxDataPerRun"),
            "Use a size such as 5GiB or 500MiB."
        );
        assert!(!message("structure.schedule.cron").is_empty());
        assert!(!message("packs.schedule.timezone").is_empty());
        assert_eq!(failed["errors"].as_array().unwrap().len(), 5);
        assert_eq!(settings_rows(&ts, &repo).await, 0);

        body = settings_body();
        body["restore"]["files_per_run"] = json!(1001);
        let response = put_settings(&ts, &api, &repo, &body).await;
        assert_eq!(response.status(), 422);
        assert_eq!(
            put_settings(&ts, &api, "no-such-repo", &settings_body())
                .await
                .status(),
            404
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_api_settings_validate() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let url = format!("{}/settings/validate", checks_url(&repo));

        let mut body = settings_body();
        body["structure"]["cron"] = json!("30 1 * * *");
        let response = api.post(&url, &body).await;
        assert_eq!(response.status(), 200, "[{}]", backend.name());
        let checked: Value = response.json().await.unwrap();
        assert!(checked["errors"].as_array().unwrap().is_empty());
        let schedules = checked["schedules"].as_array().unwrap();
        assert_eq!(schedules.len(), 3);
        assert_eq!(schedules[0]["level"], "structure");
        assert_eq!(schedules[2]["level"], "restore");
        for schedule in schedules {
            assert!(schedule["schedule"]["next_runs"][0].as_i64().is_some());
        }
        assert_eq!(settings_rows(&ts, &repo).await, 0);
        assert_eq!(
            status(&api, &repo).await["levels"][0]["settings"]["cron"],
            "0 2 * * *"
        );

        body["packs"]["cron"] = json!("nope");
        let checked: Value = api.post(&url, &body).await.json().await.unwrap();
        assert_eq!(checked["errors"][0]["path"], "packs.schedule.cron");
        assert!(checked["schedules"][1]["schedule"].is_null());
        assert!(checked["schedules"][0]["schedule"].is_object());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_api_settings_scope() {
    for backend in backends() {
        let ts = server(&backend).await;
        let repo = ts.create_local_repository("main").await;
        let put = |client: reqwest::Client| {
            let url = settings_url(&ts, &repo);
            async move {
                client
                    .put(url)
                    .json(&settings_body())
                    .send()
                    .await
                    .unwrap()
                    .status()
            }
        };

        let runner = bearer_client(&ts.issue_token(&["jobs:run", "jobs:read"]).await);
        assert_eq!(put(runner).await, 403, "[{}]", backend.name());
        let writer = bearer_client(&ts.issue_token(&["policies:write"]).await);
        assert_eq!(put(writer.clone()).await, 200);
        let validate = format!("{}{}/settings/validate", ts.rest_url(), checks_url(&repo));
        let response = writer
            .post(&validate)
            .json(&settings_body())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
        let reader = bearer_client(&ts.issue_token(&["jobs:read"]).await);
        let response = reader
            .post(&validate)
            .json(&settings_body())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_api_settings_saved() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        exec(
            &ts,
            &format!(
                "INSERT INTO check_settings (repository_id, level, enabled, cron, timezone, \
                 updated_at) VALUES ('{repo}', 'structure', 1, '0 2 * * *', 'UTC', 1)"
            ),
        )
        .await;

        let mut body = settings_body();
        body["structure"]["cron"] = json!("15 3 * * *");
        body["structure"]["timezone"] = json!("Europe/Berlin");
        body["packs"]["enabled"] = json!(false);
        body["packs"]["read_per_run"] = json!("10GiB");
        body["restore"]["files_per_run"] = json!(50);
        body["restore"]["max_data_per_run"] = json!("2GiB");
        let response = put_settings(&ts, &api, &repo, &body).await;
        assert_eq!(response.status(), 200, "[{}]", backend.name());
        let saved: Value = response.json().await.unwrap();
        assert_eq!(saved["levels"][0]["settings"]["cron"], "15 3 * * *");

        let shown = status(&api, &repo).await;
        let levels = shown["levels"].as_array().unwrap();
        assert_eq!(levels[0]["settings"]["timezone"], "Europe/Berlin");
        assert_eq!(levels[1]["settings"]["enabled"], false);
        assert_eq!(levels[1]["settings"]["read_per_run"], "10GiB");
        assert_eq!(levels[2]["settings"]["files_per_run"], 50);
        assert_eq!(levels[2]["settings"]["max_data_per_run"], "2GiB");
        assert_eq!(settings_rows(&ts, &repo).await, 3);
        let stamp: i64 = sqlx::query(
            "SELECT updated_at FROM check_settings WHERE repository_id = $1 AND level = 'structure'",
        )
        .bind(&repo)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .get("updated_at");
        assert!(stamp > 1);

        let again = put_settings(&ts, &api, &repo, &body).await;
        assert_eq!(again.status(), 200);
        assert_eq!(settings_rows(&ts, &repo).await, 3);
        assert_eq!(status(&api, &repo).await["levels"], shown["levels"]);

        body["restore"]
            .as_object_mut()
            .unwrap()
            .remove("files_per_run");
        assert_eq!(put_settings(&ts, &api, &repo, &body).await.status(), 200);
        assert_eq!(
            status(&api, &repo).await["levels"][2]["settings"]["files_per_run"],
            50
        );
        ts.stop().await;
    }
}

async fn repo_checks(api: &Api, repo: &str) -> Value {
    let list: Value = api.get("/api/v1/repositories").await.json().await.unwrap();
    list.as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == repo)
        .unwrap()["checks"]
        .clone()
}

async fn finish_run_row(ts: &TestServer, repo: &str, level: &str, state: &str, at: i64) {
    exec(
        ts,
        &format!(
            "INSERT INTO check_runs (id, repository_id, level, origin, started_by, state, \
             outcome, created_at, finished_at) VALUES ('{level}-{at}', '{repo}', '{level}', \
             'manual', 'test', '{state}', 'passed', {at}, {at})"
        ),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_api_repos_summary() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;

        let never = repo_checks(&api, &repo).await;
        assert_eq!(never["state"], "never_run", "[{}]", backend.name());
        assert!(never["finished_at"].is_null());

        finish_run_row(&ts, &repo, "structure", "done", 1000).await;
        finish_run_row(&ts, &repo, "packs", "failed", 2000).await;
        let failed = repo_checks(&api, &repo).await;
        assert_eq!(failed["state"], "failed");
        assert_eq!(failed["finished_at"], 2000);

        finish_run_row(&ts, &repo, "packs", "done", 3000).await;
        let partial = repo_checks(&api, &repo).await;
        assert_eq!(partial["state"], "never_run");

        finish_run_row(&ts, &repo, "restore", "done", 4000).await;
        let passed = repo_checks(&api, &repo).await;
        assert_eq!(passed["state"], "passed");
        assert_eq!(passed["finished_at"], 4000);

        let mut off = settings_body();
        for level in ["structure", "packs", "restore"] {
            off[level]["enabled"] = json!(false);
        }
        assert_eq!(put_settings(&ts, &api, &repo, &off).await.status(), 200);
        assert_eq!(repo_checks(&api, &repo).await["state"], "off");

        let detail: Value = api
            .get(&format!("/api/v1/repositories/{repo}"))
            .await
            .json()
            .await
            .unwrap();
        assert!(detail["checks"].is_array());
        ts.stop().await;
    }
}

async fn stopped_while_deleting(ts: &TestServer, repo: &str, id: &str) {
    insert_cycle(ts, repo, id, "failed").await;
    exec(
        ts,
        &format!(
            "UPDATE gc_cycles SET error_after_delete = 1, sweep_started = 1, finished_at = 1 \
             WHERE id = '{id}'"
        ),
    )
    .await;
}

async fn cycle_state(ts: &TestServer, id: &str) -> String {
    sqlx::query("SELECT state FROM gc_cycles WHERE id = $1")
        .bind(id)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .try_get("state")
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_gc_resume_waits_for_running_check() {
    use lodestore_server::maintenance::gc;
    for backend in backends() {
        let ts = server(&backend).await;
        let repo = ts.create_local_repository("main").await;
        stopped_while_deleting(&ts, &repo, "gc-1").await;
        insert_run(&ts, &repo, "run-1", "running").await;

        gc::tick_once(ts.state(), lodestore_server::db::now_ms())
            .await
            .unwrap();
        assert_eq!(
            cycle_state(&ts, "gc-1").await,
            "failed",
            "[{}]",
            backend.name()
        );
        assert_eq!(
            gc::db::count_events(ts.state(), "gc-1", "resumed")
                .await
                .unwrap(),
            0
        );

        exec(&ts, "DELETE FROM check_runs WHERE id = 'run-1'").await;
        gc::tick_once(ts.state(), lodestore_server::db::now_ms())
            .await
            .unwrap();
        assert_eq!(
            gc::db::count_events(ts.state(), "gc-1", "resumed")
                .await
                .unwrap(),
            1,
            "[{}]",
            backend.name()
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_api_blocked_by_cleanup_stopped_while_deleting() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        stopped_while_deleting(&ts, &repo, "gc-1").await;

        let response = start(&api, &repo, "structure").await;
        assert_eq!(response.status(), 409, "[{}]", backend.name());
        let shown = status(&api, &repo).await;
        assert_eq!(shown["blocked"]["reason"], "cleanup");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_cancel_queued_run_that_started_meanwhile() {
    for backend in backends() {
        let ts = server(&backend).await;
        let repo = ts.create_local_repository("main").await;
        insert_run(&ts, &repo, "run-1", "running").await;

        let cancelled =
            lodestore_server::maintenance::check::db::cancel_queued(ts.state(), "run-1")
                .await
                .unwrap();
        assert!(cancelled);
        let row = sqlx::query("SELECT state, cancel_requested FROM check_runs WHERE id = 'run-1'")
            .fetch_one(&ts.state().db.pool)
            .await
            .unwrap();
        let state: String = row.try_get("state").unwrap();
        assert_eq!(state, "running", "[{}]", backend.name());
        assert_eq!(row.try_get::<i64, _>("cancel_requested").unwrap(), 1);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_restore_nothing_restored_is_stopped() {
    for backend in backends() {
        let ts = server(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        write_files(&ts, "main", 1, 1, 4 << 20).await;
        write_files(&ts, "main", 2, 1, 0).await;
        set_restore_limits(&ts, &repo, r#"{"max_data_per_run":1048576}"#).await;

        let finished = run_to_the_end(&api, &repo, "restore").await;
        assert_eq!(
            finished["state"],
            "failed",
            "[{}] {finished}",
            backend.name()
        );
        assert_eq!(finished["outcome"], "stopped");
        assert_eq!(finished["error"], "Stopped at the size limit.");
        assert_eq!(finished["summary"]["restore"]["stopped_at_limit"], true);
        ts.stop().await;
    }
}
