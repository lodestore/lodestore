mod common;

use common::{Api, TestServer, backends};
use lodestore_agent::{EnrollArgs, NetOptions};
use serde_json::{Value, json};

async fn enroll(ts: &TestServer, api: &Api, repo: &str, hostname: &str) -> String {
    let token = api.join_token(repo, json!([])).await;
    lodestore_agent::enroll(EnrollArgs {
        token,
        paths: ts.agent_paths(hostname),
        hostname: Some(hostname.to_string()),
        net: NetOptions::default(),
    })
    .await
    .expect("enroll");
    api.hosts()
        .await
        .iter()
        .find(|h| h["hostname"] == hostname)
        .map(|h| h["id"].as_str().unwrap().to_string())
        .expect("the enrolled host")
}

struct NewJob<'a> {
    host: &'a str,
    policy_name: Option<&'a str>,
    source: &'a str,
    state: &'a str,
    queued_at: i64,
}

async fn insert_job(ts: &TestServer, job: NewJob<'_>) -> String {
    let id = format!("job-{}-{}", job.queued_at, job.host);
    let finished = matches!(
        job.state,
        "succeeded" | "warnings" | "failed" | "cancelled" | "interrupted" | "not_started"
    )
    .then_some(job.queued_at + 1);
    sqlx::query(
        "INSERT INTO jobs (id, host_id, policy_name, source, state, spec, queued_at, finished_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(&id)
    .bind(job.host)
    .bind(job.policy_name)
    .bind(job.source)
    .bind(job.state)
    .bind(Vec::<u8>::new())
    .bind(job.queued_at)
    .bind(finished)
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
    id
}

async fn get_json(api: &Api, path: &str) -> (u16, Value) {
    let response = api.get(path).await;
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

async fn post_json(api: &Api, path: &str) -> (u16, Value) {
    let response = api.post(path, &json!({})).await;
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jobs_api_list_filters_and_order() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let hosts = [
            enroll(&ts, &api, &repo, "host-a").await,
            enroll(&ts, &api, &repo, "host-b").await,
            enroll(&ts, &api, &repo, "host-c").await,
        ];
        for (i, state) in ["queued", "queued", "queued", "running", "assigned"]
            .iter()
            .enumerate()
        {
            insert_job(
                &ts,
                NewJob {
                    host: &hosts[i % 3],
                    policy_name: Some("p1"),
                    source: if *state == "running" || *state == "assigned" {
                        "cli"
                    } else {
                        "manual"
                    },
                    state,
                    queued_at: 1000 + i as i64,
                },
            )
            .await;
        }
        let mut failed = 0;
        let mut cli_done = 0;
        let mut on_a = 2usize;
        for i in 0..55 {
            let state = if i % 4 == 0 { "failed" } else { "succeeded" };
            let source = if i % 5 == 0 { "cli" } else { "manual" };
            failed += usize::from(state == "failed");
            cli_done += usize::from(source == "cli");
            on_a += usize::from(i % 3 == 0);
            insert_job(
                &ts,
                NewJob {
                    host: &hosts[i % 3],
                    policy_name: Some(if i % 2 == 0 { "p1" } else { "p2" }),
                    source,
                    state,
                    queued_at: 100 + i as i64,
                },
            )
            .await;
        }

        let (status, page) = get_json(&api, "/api/v1/jobs").await;
        assert_eq!(status, 200);
        assert_eq!(page["total"], 60, "[{}]", backend.name());
        assert_eq!(page["page"], 1);
        assert_eq!(page["page_size"], 50);
        let items = page["items"].as_array().unwrap();
        assert_eq!(items.len(), 50);
        for item in &items[..5] {
            assert!(matches!(
                item["state"].as_str().unwrap(),
                "queued" | "running"
            ));
        }
        let finished: Vec<i64> = items[5..]
            .iter()
            .map(|j| j["queued_at"].as_i64().unwrap())
            .collect();
        assert!(finished.windows(2).all(|w| w[0] > w[1]));
        let (_, second) = get_json(&api, "/api/v1/jobs?page=2").await;
        assert_eq!(second["items"].as_array().unwrap().len(), 10);

        let total = |query: &str| {
            let api = &api;
            let path = format!("/api/v1/jobs?{query}");
            async move { get_json(api, &path).await.1["total"].as_u64().unwrap() as usize }
        };
        assert_eq!(total("status=failed").await, failed);
        assert_eq!(total("status=queued").await, 4);
        assert_eq!(total("source=cli").await, cli_done + 2);
        assert_eq!(total("policy=p2").await, 27);
        assert_eq!(total("host=HOST-A").await, on_a);
        assert_eq!(total("host=ost-").await, 60);
        assert_eq!(total("host=100%25").await, 0);
        assert_eq!(total(&format!("host_id={}", hosts[0])).await, on_a);
        assert_eq!(total("host_id=nope").await, 0);
        assert_eq!(
            total(&format!("host_id={}&status=failed", hosts[0])).await,
            5
        );
        assert_eq!(total("status=failed&policy=p1&source=manual").await, 11);
        assert_eq!(get_json(&api, "/api/v1/jobs?status=bogus").await.0, 422);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jobs_api_detail_fields() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let busy = enroll(&ts, &api, &repo, "host-busy").await;
        let idle = enroll(&ts, &api, &repo, "host-idle").await;
        let away = enroll(&ts, &api, &repo, "host-away").await;
        let registry = &ts.state().registry;
        let _busy_session = registry.register(&busy, &repo);
        let _idle_session = registry.register(&idle, &repo);

        let running = insert_job(
            &ts,
            NewJob {
                host: &busy,
                policy_name: Some("p"),
                source: "manual",
                state: "running",
                queued_at: 10,
            },
        )
        .await;
        let waiting = insert_job(
            &ts,
            NewJob {
                host: &busy,
                policy_name: Some("p"),
                source: "cli",
                state: "queued",
                queued_at: 11,
            },
        )
        .await;
        let free = insert_job(
            &ts,
            NewJob {
                host: &idle,
                policy_name: Some("p"),
                source: "manual",
                state: "queued",
                queued_at: 12,
            },
        )
        .await;
        let offline = insert_job(
            &ts,
            NewJob {
                host: &away,
                policy_name: Some("p"),
                source: "manual",
                state: "queued",
                queued_at: 13,
            },
        )
        .await;
        let assigned = insert_job(
            &ts,
            NewJob {
                host: &away,
                policy_name: Some("p"),
                source: "cli",
                state: "assigned",
                queued_at: 14,
            },
        )
        .await;
        let done = insert_job(
            &ts,
            NewJob {
                host: &away,
                policy_name: None,
                source: "cli",
                state: "failed",
                queued_at: 1,
            },
        )
        .await;
        sqlx::query(
            "UPDATE jobs SET phase = 'backup', progress_json = $1, started_at = 10, \
             log_seq = 7, log_lines = 5, cancel_requested = 1 WHERE id = $2",
        )
        .bind(
            json!({ "files_done": 3, "files_total": 9, "bytes_done": 100, "bytes_total": 900,
                    "bytes_uploaded": 40, "scan_done": true, "speed_bps": 50, "eta_s": null,
                    "updated_at": 99 })
            .to_string(),
        )
        .bind(&running)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE jobs SET stats_json = $1, error_detail_json = $2, error_code = 'hook_failed', \
             error_message = 'boom', caller_uid = 1000, caller_gid = 100, incomplete = 1, \
             errors_total = 2, warnings_total = 1, snapshot_id = 'abc', logs_purged = 1 \
             WHERE id = $3",
        )
        .bind(
            json!({ "files_new": 1, "files_changed": 2, "files_unchanged": 3, "dirs": 4,
                    "bytes_total": 5, "bytes_read": 6, "bytes_added": 7 })
            .to_string(),
        )
        .bind(json!({ "script": "dump-db", "exit": "3" }).to_string())
        .bind(&done)
        .execute(&ts.state().db.pool)
        .await
        .unwrap();

        let (status, job) = get_json(&api, &format!("/api/v1/jobs/{running}")).await;
        assert_eq!(status, 200, "[{}]", backend.name());
        assert_eq!(job["state"], "running");
        assert_eq!(job["phase"], "backup");
        assert_eq!(job["host_online"], true);
        assert_eq!(job["repository_id"], repo);
        assert_eq!(job["cancel_requested"], true);
        assert_eq!(job["log_seq"], 7);
        assert_eq!(job["log_lines"], 5);
        assert_eq!(job["progress"]["files_total"], 9);
        assert_eq!(job["progress"]["scan_done"], true);
        assert!(job["progress"]["eta_s"].is_null());
        assert!(job["queue_reason"].is_null());
        assert_eq!(job["policy_exists"], false);

        let reason = |id: &str| {
            let api = &api;
            let path = format!("/api/v1/jobs/{id}");
            async move { get_json(api, &path).await.1 }
        };
        let waiting = reason(&waiting).await;
        assert_eq!(waiting["queue_reason"], "host_busy");
        assert_eq!(waiting["source"], "cli");
        assert!(reason(&free).await["queue_reason"].is_null());
        assert_eq!(reason(&offline).await["queue_reason"], "host_offline");
        let assigned = reason(&assigned).await;
        assert_eq!(assigned["state"], "queued");
        assert!(assigned["queue_reason"].is_null());

        let done = reason(&done).await;
        assert_eq!(done["state"], "failed");
        assert_eq!(done["stats"]["bytes_added"], 7);
        assert_eq!(done["error_detail"]["script"], "dump-db");
        assert_eq!(done["error_code"], "hook_failed");
        assert_eq!(done["caller_uid"], 1000);
        assert_eq!(done["incomplete"], true);
        assert_eq!(done["snapshot_id"], "abc");
        assert_eq!(done["logs_purged"], true);
        assert_eq!(done["errors_total"], 2);

        assert_eq!(get_json(&api, "/api/v1/jobs/nope").await.0, 404);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jobs_api_events_after_and_level() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let job = insert_job(
            &ts,
            NewJob {
                host: &host,
                policy_name: None,
                source: "cli",
                state: "running",
                queued_at: 1,
            },
        )
        .await;
        for (seq, level) in ["info", "warning", "info", "error", "warning", "info"]
            .iter()
            .enumerate()
        {
            sqlx::query(
                "INSERT INTO job_events (job_id, seq, at, level, kind, path, script, message) \
                 VALUES ($1, $2, $3, $4, 'file', $5, NULL, $6)",
            )
            .bind(&job)
            .bind(seq as i64 + 1)
            .bind(1000 + seq as i64)
            .bind(*level)
            .bind("/etc/x")
            .bind(format!("event {}", seq + 1))
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        }
        let seqs = |page: &Value| -> Vec<i64> {
            page["events"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["seq"].as_i64().unwrap())
                .collect()
        };
        let base = format!("/api/v1/jobs/{job}/events");

        let (status, all) = get_json(&api, &base).await;
        assert_eq!(status, 200, "[{}]", backend.name());
        assert_eq!(seqs(&all), [1, 2, 3, 4, 5, 6]);
        assert_eq!(all["last_seq"], 6);
        assert_eq!(all["events"][0]["path"], "/etc/x");
        assert!(all["events"][0]["script"].is_null());

        let (_, tail) = get_json(&api, &format!("{base}?after=3")).await;
        assert_eq!(seqs(&tail), [4, 5, 6]);
        let (_, warnings) = get_json(&api, &format!("{base}?level=warning")).await;
        assert_eq!(seqs(&warnings), [2, 5]);
        assert_eq!(warnings["last_seq"], 5);
        let (_, errors) = get_json(&api, &format!("{base}?after=1&level=error")).await;
        assert_eq!(seqs(&errors), [4]);
        let (_, empty) = get_json(&api, &format!("{base}?after=6")).await;
        assert!(seqs(&empty).is_empty());
        assert_eq!(empty["last_seq"], 6);

        assert_eq!(get_json(&api, &format!("{base}?level=info")).await.0, 422);
        assert_eq!(get_json(&api, "/api/v1/jobs/nope/events").await.0, 404);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jobs_api_cancel_queued_and_running() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let job = |state: &'static str, at: i64| {
            let ts = &ts;
            let host = host.clone();
            async move {
                insert_job(
                    ts,
                    NewJob {
                        host: &host,
                        policy_name: None,
                        source: "cli",
                        state,
                        queued_at: at,
                    },
                )
                .await
            }
        };
        let queued = job("queued", 1).await;
        let running = job("running", 2).await;
        let done = job("succeeded", 3).await;

        let (status, cancelled) = post_json(&api, &format!("/api/v1/jobs/{queued}/cancel")).await;
        assert_eq!(status, 200, "[{}] {cancelled}", backend.name());
        assert_eq!(cancelled["state"], "cancelled");
        assert!(cancelled["finished_at"].is_i64());

        let (status, requested) = post_json(&api, &format!("/api/v1/jobs/{running}/cancel")).await;
        assert_eq!(status, 200);
        assert_eq!(requested["state"], "running");
        assert_eq!(requested["cancel_requested"], true);
        assert!(requested["finished_at"].is_null());

        assert_eq!(
            post_json(&api, &format!("/api/v1/jobs/{done}/cancel"))
                .await
                .0,
            409
        );
        assert_eq!(
            post_json(&api, &format!("/api/v1/jobs/{queued}/cancel"))
                .await
                .0,
            409
        );
        assert_eq!(post_json(&api, "/api/v1/jobs/nope/cancel").await.0, 404);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jobs_api_rerun() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let yaml = "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: p\n\
                    spec:\n  repository: main\n  paths:\n    - /etc\n";
        let created = api.post("/api/v1/policies", &json!({ "yaml": yaml })).await;
        assert_eq!(created.status(), 201);
        let (_, run) = post_json(&api, "/api/v1/policies/p/run").await;
        let first = run["jobs"][0]["id"].as_str().unwrap().to_string();

        let pool = &ts.state().db.pool;
        sqlx::query("UPDATE jobs SET state = 'failed', finished_at = 5 WHERE id = $1")
            .bind(&first)
            .execute(pool)
            .await
            .unwrap();

        let (status, again) = post_json(&api, &format!("/api/v1/jobs/{first}/rerun")).await;
        assert_eq!(status, 200, "[{}] {again}", backend.name());
        assert_eq!(again["created"], true);
        assert_ne!(again["id"], first);
        assert_eq!(again["host_id"], host);
        assert_eq!(again["hostname"], "host-a");
        let (_, job) = get_json(
            &api,
            &format!("/api/v1/jobs/{}", again["id"].as_str().unwrap()),
        )
        .await;
        assert_eq!(job["state"], "queued");
        assert_eq!(job["policy_name"], "p");

        let (_, twice) = post_json(&api, &format!("/api/v1/jobs/{first}/rerun")).await;
        assert_eq!(twice["created"], false);
        assert_eq!(twice["id"], again["id"]);

        let cli = insert_job(
            &ts,
            NewJob {
                host: &host,
                policy_name: Some("p"),
                source: "cli",
                state: "succeeded",
                queued_at: 50,
            },
        )
        .await;
        let policy_exists = |id: &str| {
            let api = &api;
            let id = id.to_string();
            async move {
                let (_, page) = get_json(api, "/api/v1/jobs").await;
                page["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|j| j["id"] == id)
                    .map(|j| j["policy_exists"].as_bool().unwrap())
                    .unwrap()
            }
        };
        assert!(policy_exists(&first).await, "[{}]", backend.name());
        assert!(!policy_exists(&cli).await, "[{}]", backend.name());
        assert_eq!(
            post_json(&api, &format!("/api/v1/jobs/{cli}/rerun"))
                .await
                .0,
            409
        );

        let other = ts.create_local_repository("other").await;
        sqlx::query("UPDATE policies SET repository_id = $1 WHERE name = 'p'")
            .bind(&other)
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(
            post_json(&api, &format!("/api/v1/jobs/{first}/rerun"))
                .await
                .0,
            409,
            "[{}]",
            backend.name()
        );
        sqlx::query("UPDATE policies SET repository_id = $1 WHERE name = 'p'")
            .bind(&repo)
            .execute(pool)
            .await
            .unwrap();

        sqlx::query("UPDATE jobs SET state = 'cancelled', finished_at = 6 WHERE id = $1")
            .bind(again["id"].as_str().unwrap())
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM policies WHERE name = 'p'")
            .execute(pool)
            .await
            .unwrap();
        let (_, detail) = get_json(&api, &format!("/api/v1/jobs/{first}")).await;
        assert_eq!(detail["policy_exists"], false);
        assert!(!policy_exists(&first).await, "[{}]", backend.name());
        assert_eq!(
            post_json(&api, &format!("/api/v1/jobs/{first}/rerun"))
                .await
                .0,
            409
        );
        assert_eq!(post_json(&api, "/api/v1/jobs/nope/rerun").await.0, 404);
        ts.stop().await;
    }
}

async fn insert_restore(
    ts: &TestServer,
    host: &str,
    repo: &str,
    state: &str,
    queued_at: i64,
) -> String {
    let id = format!("restore-{queued_at}");
    let record = json!({
        "snapshot_id": "a".repeat(64),
        "source_repository_id": repo,
        "source_host_id": null,
        "source_hostname": "old-box",
        "items_b64": ["L2V0Yw"],
        "destination": "/srv/out",
        "in_place": false,
        "conflict": "overwrite_changed",
        "restore_owners": false,
        "started_by": "admin",
    });
    let finished = (state != "queued").then_some(queued_at + 1);
    sqlx::query(
        "INSERT INTO jobs (id, host_id, source, state, spec, queued_at, finished_at, kind, \
         restore_json) VALUES ($1, $2, 'manual', $3, $4, $5, $6, 'restore', $7)",
    )
    .bind(&id)
    .bind(host)
    .bind(state)
    .bind(Vec::<u8>::new())
    .bind(queued_at)
    .bind(finished)
    .bind(record.to_string())
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
    id
}

async fn insert_download(
    ts: &TestServer,
    repo: &str,
    host: Option<&str>,
    hostname: &str,
    state: &str,
    created_at: i64,
) -> String {
    let id = format!("download-{created_at}");
    sqlx::query(
        "INSERT INTO downloads (id, repository_id, host_id, hostname, snapshot_id, items_json, \
         format, started_by, state, created_at, bytes_sent) \
         VALUES ($1, $2, $3, $4, $5, $6, 'tar_zst', 'admin', $7, $8, 4096)",
    )
    .bind(&id)
    .bind(repo)
    .bind(host)
    .bind(hostname)
    .bind("b".repeat(64))
    .bind(json!(["L2V0Yw", "L3Zhcg"]).to_string())
    .bind(state)
    .bind(created_at)
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
    id
}

fn ids(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["id"].as_str().unwrap().to_string())
        .collect()
}

async fn seed_mixed(ts: &TestServer, host: &str, repo: &str) -> Vec<String> {
    let now = lodestore_server::db::now_ms();
    let mut expected = Vec::new();
    expected.push(insert_download(ts, repo, Some(host), "host-a", "pending", now).await);
    expected.push(insert_restore(ts, host, repo, "queued", 200).await);
    expected.push(insert_download(ts, repo, None, "old-box", "succeeded", 150).await);
    for queued_at in [102, 101, 100] {
        expected.push(
            insert_job(
                ts,
                NewJob {
                    host,
                    policy_name: Some("p1"),
                    source: "manual",
                    state: "succeeded",
                    queued_at,
                },
            )
            .await,
        );
    }
    expected.push(insert_restore(ts, host, repo, "succeeded", 50).await);
    expected
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jobs_api_merges_downloads_and_restores() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let expected = seed_mixed(&ts, &host, &repo).await;

        let (status, all) = get_json(&api, "/api/v1/jobs").await;
        assert_eq!(status, 200);
        assert_eq!(all["total"], 7, "[{}]", backend.name());
        assert_eq!(ids(&all), expected);
        let (_, second) = get_json(&api, "/api/v1/jobs?page=2").await;
        assert_eq!(second["total"], 7);
        assert!(second["items"].as_array().unwrap().is_empty());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jobs_api_kind_filter() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        seed_mixed(&ts, &host, &repo).await;

        for (kind, want, block) in [
            ("download", 2, Some("download")),
            ("restore", 2, Some("restore")),
            ("backup", 3, None),
        ] {
            let (status, page) = get_json(&api, &format!("/api/v1/jobs?kind={kind}")).await;
            assert_eq!(status, 200);
            assert_eq!(page["total"], want, "{kind} [{}]", backend.name());
            let items = page["items"].as_array().unwrap();
            assert_eq!(items.len(), want);
            for item in items {
                assert_eq!(item["kind"], kind);
                if let Some(block) = block {
                    assert!(item[block].is_object(), "{kind} has no {block}");
                }
            }
        }

        let (_, by_host) = get_json(&api, "/api/v1/jobs?host=OLD").await;
        assert_eq!(by_host["total"], 1);
        assert_eq!(by_host["items"][0]["kind"], "download");
        let (_, by_host) = get_json(&api, "/api/v1/jobs?kind=download&host=host-a").await;
        assert_eq!(by_host["total"], 1);
        assert_eq!(by_host["items"][0]["hostname"], "host-a");

        let (_, queued) = get_json(&api, "/api/v1/jobs?status=queued&kind=download").await;
        assert_eq!(queued["total"], 1);
        let (_, done) = get_json(&api, "/api/v1/jobs?status=succeeded&kind=download").await;
        assert_eq!(done["total"], 1);
        let (_, failed) = get_json(&api, "/api/v1/jobs?status=failed&kind=download").await;
        assert_eq!(failed["total"], 0);
        let (_, by_policy) = get_json(&api, "/api/v1/jobs?kind=download&policy=p1").await;
        assert_eq!(by_policy["total"], 0);

        let (status, _) = get_json(&api, "/api/v1/jobs?kind=other").await;
        assert_eq!(status, 422);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jobs_api_download_detail_and_refusals() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let now = lodestore_server::db::now_ms();
        let done = insert_download(&ts, &repo, None, "old-box", "succeeded", 150).await;
        let expired = insert_download(&ts, &repo, None, "old-box", "pending", now - 120_000).await;
        let broken = insert_download(&ts, &repo, None, "old-box", "interrupted", 160).await;
        sqlx::query("UPDATE downloads SET error_code = 'client_closed' WHERE id = $1")
            .bind(&broken)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();

        let (status, detail) = get_json(&api, &format!("/api/v1/jobs/{done}")).await;
        assert_eq!(status, 200, "[{}]", backend.name());
        assert_eq!(detail["kind"], "download");
        assert_eq!(detail["state"], "succeeded");
        assert_eq!(detail["repository_id"], repo);
        assert_eq!(detail["download"]["format"], "tar_zst");
        assert_eq!(detail["download"]["bytes_sent"], 4096);
        assert_eq!(detail["download"]["items"], 2);
        assert_eq!(detail["download"]["started_by"], "admin");
        assert_eq!(detail["source"], "manual");

        let (_, detail) = get_json(&api, &format!("/api/v1/jobs/{expired}")).await;
        assert_eq!(detail["state"], "not_started");
        let (_, detail) = get_json(&api, &format!("/api/v1/jobs/{broken}")).await;
        assert_eq!(detail["state"], "interrupted");
        assert_eq!(detail["error_code"], "client_closed");

        for action in ["cancel", "rerun"] {
            let (status, body) = post_json(&api, &format!("/api/v1/jobs/{done}/{action}")).await;
            assert_eq!(status, 409, "{action}");
            assert_eq!(
                body["message"],
                "Downloads cannot be cancelled or run again."
            );
            let (status, _) = post_json(&api, &format!("/api/v1/jobs/nope/{action}")).await;
            assert_eq!(status, 404, "{action}");
        }
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jobs_api_agentless_download_host() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let agent = enroll(&ts, &api, &repo, "host-a").await;
        let now = lodestore_server::db::now_ms();
        insert_download(&ts, &repo, None, "old-box", "succeeded", now - 10).await;
        insert_download(&ts, &repo, Some(&agent), "host-a", "succeeded", now - 20).await;

        let (_, page) = get_json(&api, "/api/v1/jobs?kind=download").await;
        let hosts: Vec<(&str, &str)> = page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|j| {
                (
                    j["host_id"].as_str().unwrap(),
                    j["hostname"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            hosts,
            [("agentless:old-box", "old-box"), (agent.as_str(), "host-a")],
            "[{}]",
            backend.name()
        );
        assert_eq!(page["items"][0]["host_online"], false);

        let (_, by_id) = get_json(&api, "/api/v1/jobs?host_id=agentless:old-box").await;
        assert_eq!(by_id["total"], 1);
        assert_eq!(by_id["items"][0]["hostname"], "old-box");
        let (_, by_agent) = get_json(&api, &format!("/api/v1/jobs?host_id={agent}")).await;
        assert_eq!(by_agent["total"], 1);
        ts.stop().await;
    }
}

struct Seed<'a> {
    id: &'a str,
    origin: &'a str,
    missed_runs: Option<i64>,
    retention: Option<&'a str>,
    snapshot: Option<&'a str>,
    spec: Vec<u8>,
}

impl<'a> Seed<'a> {
    fn new(id: &'a str) -> Seed<'a> {
        Seed {
            id,
            origin: "manual",
            missed_runs: None,
            retention: None,
            snapshot: None,
            spec: Vec::new(),
        }
    }
}

async fn seed_job(ts: &TestServer, host: &str, seed: Seed<'_>) {
    sqlx::query(
        "INSERT INTO jobs (id, host_id, policy_name, source, state, spec, queued_at, finished_at, \
         origin, missed_runs, retention_json, snapshot_id) \
         VALUES ($1, $2, 'p', 'manual', 'succeeded', $3, 100, 101, $4, $5, $6, $7)",
    )
    .bind(seed.id)
    .bind(host)
    .bind(seed.spec)
    .bind(seed.origin)
    .bind(seed.missed_runs)
    .bind(seed.retention)
    .bind(seed.snapshot)
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
}

async fn seed_snapshot(ts: &TestServer, id: &str, suspicious: i64) {
    sqlx::query(
        "INSERT INTO snapshots (id, repository_id, hostname, time_ms, paths_json, tags_json, \
         incomplete, errors_total, warnings_total, stats_json, discovered_at, suspicious) \
         SELECT $1, id, 'h', 0, '[]', '[]', 0, 0, 0, '{}', 0, $2 FROM repositories",
    )
    .bind(id)
    .bind(suspicious)
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
}

async fn job_detail(api: &Api, id: &str) -> Value {
    let (status, body) = get_json(api, &format!("/api/v1/jobs/{id}")).await;
    assert_eq!(status, 200, "{body}");
    body
}

async fn single_host(ts: &TestServer) -> (Api, String) {
    let api = ts.api().await;
    let repo = ts.create_local_repository("main").await;
    let host = enroll(ts, &api, &repo, "host-a").await;
    (api, host)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jobs_api_origin_and_missed_runs() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let (api, host) = single_host(&ts).await;
        seed_job(
            &ts,
            &host,
            Seed {
                origin: "catchup",
                missed_runs: Some(3),
                ..Seed::new("catch")
            },
        )
        .await;
        seed_job(&ts, &host, Seed::new("hand")).await;
        seed_job(
            &ts,
            &host,
            Seed {
                origin: "schedule",
                ..Seed::new("slot")
            },
        )
        .await;

        let (_, page) = get_json(&api, "/api/v1/jobs").await;
        let find = |id: &str| {
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|j| j["id"] == id)
                .unwrap()
                .clone()
        };
        assert_eq!(find("catch")["origin"], "catchup", "{}", backend.name());
        assert_eq!(find("catch")["missed_runs"], 3);
        assert_eq!(find("hand")["origin"], "manual");
        assert_eq!(find("hand")["missed_runs"], Value::Null);
        assert_eq!(find("slot")["origin"], "schedule");
        let detail = job_detail(&api, "catch").await;
        assert_eq!(detail["origin"], "catchup");
        assert_eq!(detail["missed_runs"], 3);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jobs_api_retention_result() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let (api, host) = single_host(&ts).await;
        let kept = "a".repeat(64);
        let gone = "b".repeat(64);
        seed_snapshot(&ts, &kept, 0).await;
        let done = format!(
            r#"{{"state":"done","forgot":2,"kept":3,"blocked":[{{"id":"{kept}","reason":"object locked"}},{{"id":"{gone}","reason":"denied"}}]}}"#
        );
        seed_job(
            &ts,
            &host,
            Seed {
                retention: Some(&done),
                ..Seed::new("done")
            },
        )
        .await;
        seed_job(
            &ts,
            &host,
            Seed {
                retention: Some(r#"{"state":"skipped"}"#),
                ..Seed::new("skipped")
            },
        )
        .await;
        seed_job(&ts, &host, Seed::new("none")).await;

        let retention = &job_detail(&api, "done").await["retention"];
        assert_eq!(retention["state"], "done", "{}", backend.name());
        assert_eq!(retention["forgot"], 2);
        assert_eq!(retention["kept"], 3);
        assert_eq!(retention["blocked"][0]["id"], kept.as_str());
        assert_eq!(retention["blocked"][0]["reason"], "object locked");
        assert_eq!(retention["blocked"][0]["kept_in_catalog"], true);
        assert_eq!(retention["blocked"][1]["kept_in_catalog"], false);
        let skipped = job_detail(&api, "skipped").await;
        assert_eq!(skipped["retention"]["state"], "skipped");
        assert_eq!(skipped["retention"]["blocked"], json!([]));
        assert_eq!(job_detail(&api, "none").await["retention"], Value::Null);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jobs_api_limits_in_detail() {
    use lodestore_proto::agent::v1::{IoniceClass, JobSpec, Limits};
    use prost::Message;

    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let (api, host) = single_host(&ts).await;
        let limited = JobSpec {
            limits: Some(Limits {
                upload_rate_bps: 52_428_800,
                nice: 10,
                ionice_class: IoniceClass::Idle as i32,
                ionice_level: 0,
            }),
            ..Default::default()
        };
        let best_effort = JobSpec {
            limits: Some(Limits {
                ionice_class: IoniceClass::BestEffort as i32,
                ionice_level: 5,
                ..Default::default()
            }),
            ..Default::default()
        };
        for (id, spec) in [
            ("limited", limited.encode_to_vec()),
            ("effort", best_effort.encode_to_vec()),
            ("free", JobSpec::default().encode_to_vec()),
            ("empty", Vec::new()),
        ] {
            seed_job(
                &ts,
                &host,
                Seed {
                    spec,
                    ..Seed::new(id)
                },
            )
            .await;
        }

        assert_eq!(
            job_detail(&api, "limited").await["limits"],
            json!({
                "upload_rate_bps": 52_428_800,
                "nice": 10,
                "ionice_class": "idle",
                "ionice_level": null,
            }),
            "{}",
            backend.name()
        );
        assert_eq!(
            job_detail(&api, "effort").await["limits"],
            json!({
                "upload_rate_bps": null,
                "nice": null,
                "ionice_class": "best-effort",
                "ionice_level": 5,
            })
        );
        assert_eq!(job_detail(&api, "free").await["limits"], Value::Null);
        assert_eq!(job_detail(&api, "empty").await["limits"], Value::Null);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jobs_api_upload_speed_in_progress() {
    use lodestore_server::grpc::dispatch::live::LiveProgress;

    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let (api, host) = single_host(&ts).await;
        let id = insert_job(
            &ts,
            NewJob {
                host: &host,
                policy_name: Some("p"),
                source: "manual",
                state: "running",
                queued_at: 10,
            },
        )
        .await;
        ts.state().registry.live.set(
            &id,
            LiveProgress {
                files_done: 4,
                speed_bps: 2_097_152,
                upload_speed_bps: 1_048_576,
                updated_at: 20,
                ..Default::default()
            },
        );

        let detail = job_detail(&api, &id).await;
        assert_eq!(
            detail["progress"]["upload_speed_bps"],
            1_048_576,
            "{}",
            backend.name()
        );
        assert_eq!(detail["progress"]["speed_bps"], 2_097_152);
        let (_, page) = get_json(&api, "/api/v1/jobs").await;
        assert_eq!(page["items"][0]["progress"]["upload_speed_bps"], 1_048_576);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jobs_api_snapshot_suspicious_flag() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let (api, host) = single_host(&ts).await;
        let odd = "c".repeat(64);
        let fine = "d".repeat(64);
        seed_snapshot(&ts, &odd, 1).await;
        seed_snapshot(&ts, &fine, 0).await;
        for (id, snapshot) in [
            ("odd", Some(odd.as_str())),
            ("fine", Some(fine.as_str())),
            ("bare", None),
        ] {
            seed_job(
                &ts,
                &host,
                Seed {
                    snapshot,
                    ..Seed::new(id)
                },
            )
            .await;
        }

        let (_, page) = get_json(&api, "/api/v1/jobs").await;
        let flag = |id: &str| {
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|j| j["id"] == id)
                .unwrap()["snapshot_suspicious"]
                .clone()
        };
        assert_eq!(flag("odd"), true, "{}", backend.name());
        assert_eq!(flag("fine"), false);
        assert_eq!(flag("bare"), false);
        assert_eq!(page["total"], 3);
        assert_eq!(job_detail(&api, "odd").await["snapshot_suspicious"], true);
        ts.stop().await;
    }
}
