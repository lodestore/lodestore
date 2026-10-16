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
