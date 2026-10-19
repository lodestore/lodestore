mod common;

use common::{TestServer, WAIT, backends, wait_until};
use lodestore_policy::jiff::Timestamp;
use lodestore_server::maintenance::gc::schedule::tick;
use lodestore_server::maintenance::gc::{GcOrigin, GcSettings, db, start_cycle};
use sqlx::Row;

fn at(text: &str) -> Timestamp {
    text.parse().unwrap()
}

fn ms(text: &str) -> i64 {
    at(text).as_millisecond()
}

fn weekly(enabled: bool) -> GcSettings {
    GcSettings {
        enabled,
        ..GcSettings::default()
    }
}

async fn enable(ts: &TestServer, repo: &str) {
    db::save_settings(ts.state(), repo, &weekly(true))
        .await
        .unwrap();
}

async fn cursor(ts: &TestServer, repo: &str) -> Option<i64> {
    sqlx::query("SELECT last_slot_ms FROM gc_settings WHERE repository_id = $1")
        .bind(repo)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .get("last_slot_ms")
}

struct Seen {
    id: String,
    state: String,
    origin: String,
    started_by: String,
    error: Option<String>,
    error_after_delete: bool,
}

async fn cycles(ts: &TestServer, repo: &str) -> Vec<Seen> {
    sqlx::query(
        "SELECT id, state, origin, started_by, error, error_after_delete FROM gc_cycles \
         WHERE repository_id = $1 ORDER BY created_at, id",
    )
    .bind(repo)
    .fetch_all(&ts.state().db.pool)
    .await
    .unwrap()
    .iter()
    .map(|row| Seen {
        id: row.get("id"),
        state: row.get("state"),
        origin: row.get("origin"),
        started_by: row.get("started_by"),
        error: row.get("error"),
        error_after_delete: row.get::<i64, _>("error_after_delete") != 0,
    })
    .collect()
}

async fn settle(ts: &TestServer, repo: &str) {
    let done = wait_until(WAIT, || async {
        cycles(ts, repo)
            .await
            .iter()
            .all(|c| ["done", "failed", "aborted"].contains(&c.state.as_str()))
    })
    .await;
    assert!(done, "a cycle never ended");
}

async fn seed_agent(ts: &TestServer, repo: &str, id: &str, caps: Option<&str>) {
    sqlx::query(
        "INSERT INTO agents (id, hostname, repository_id, labels_json, os, arch, version, status, \
         enrolled_at, cert_serial, cert_issued_at, cert_not_after, capabilities_json) \
         VALUES ($1, $2, $3, '{}', 'linux', 'x86_64', 'test', 'online', 0, $1, 0, 0, $4)",
    )
    .bind(id)
    .bind(format!("host-{id}"))
    .bind(repo)
    .bind(caps)
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_schedule_runs_at_slot() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        enable(&ts, &repo).await;

        assert!(
            tick(ts.state(), at("2030-01-06T03:00:30Z"))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(cursor(&ts, &repo).await, Some(ms("2030-01-06T03:00:00Z")));
        assert!(
            tick(ts.state(), at("2030-01-08T12:00:00Z"))
                .await
                .unwrap()
                .is_empty()
        );

        let started = tick(ts.state(), at("2030-01-13T03:00:30Z")).await.unwrap();
        assert_eq!(started.len(), 1, "[{}]", backend.name());
        assert_eq!(cursor(&ts, &repo).await, Some(ms("2030-01-13T03:00:00Z")));
        let all = cycles(&ts, &repo).await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, started[0]);
        assert_eq!(all[0].origin, "schedule");
        assert_eq!(all[0].started_by, "schedule");
        assert!(
            tick(ts.state(), at("2030-01-13T03:00:50Z"))
                .await
                .unwrap()
                .is_empty()
        );
        settle(&ts, &repo).await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_schedule_no_immediate_run_after_change() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        enable(&ts, &repo).await;
        assert!(
            tick(ts.state(), Timestamp::now()).await.unwrap().is_empty(),
            "[{}] turning cleanup on must not start a cycle",
            backend.name()
        );
        assert!(cycles(&ts, &repo).await.is_empty());
        sqlx::query("UPDATE gc_settings SET last_slot_ms = NULL WHERE repository_id = $1")
            .bind(&repo)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();

        tick(ts.state(), at("2030-01-06T03:00:30Z")).await.unwrap();
        sqlx::query("UPDATE gc_settings SET updated_at = $1 WHERE repository_id = $2")
            .bind(ms("2030-01-08T10:00:00Z"))
            .bind(&repo)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();

        assert!(
            tick(ts.state(), at("2030-01-13T03:00:30Z"))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(cursor(&ts, &repo).await, Some(ms("2030-01-13T03:00:00Z")));
        assert!(cycles(&ts, &repo).await.is_empty());

        let started = tick(ts.state(), at("2030-01-20T03:00:30Z")).await.unwrap();
        assert_eq!(started.len(), 1);
        settle(&ts, &repo).await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_schedule_one_run_after_downtime() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        enable(&ts, &repo).await;
        tick(ts.state(), at("2030-01-06T03:00:30Z")).await.unwrap();

        let started = tick(ts.state(), at("2030-01-27T09:00:00Z")).await.unwrap();
        assert_eq!(started.len(), 1, "[{}]", backend.name());
        assert_eq!(cursor(&ts, &repo).await, Some(ms("2030-01-27T03:00:00Z")));
        assert_eq!(cycles(&ts, &repo).await.len(), 1);
        assert!(
            tick(ts.state(), at("2030-01-27T09:00:15Z"))
                .await
                .unwrap()
                .is_empty()
        );
        settle(&ts, &repo).await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_schedule_skips_when_active() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        enable(&ts, &repo).await;
        tick(ts.state(), at("2030-01-06T03:00:30Z")).await.unwrap();
        let running = start_cycle(ts.state(), &repo, false, GcOrigin::Manual, "admin")
            .await
            .unwrap();

        let started = tick(ts.state(), at("2030-01-13T03:00:30Z")).await.unwrap();
        assert!(started.is_empty(), "[{}]", backend.name());
        assert_eq!(cursor(&ts, &repo).await, Some(ms("2030-01-13T03:00:00Z")));
        let all = cycles(&ts, &repo).await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, running);
        assert_eq!(all[0].state, "marking");
        ts.stop().await;
    }
}

/// A dry run in progress does not eat the weekly slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_schedule_runs_beside_a_dry_run() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        enable(&ts, &repo).await;
        tick(ts.state(), at("2030-01-06T03:00:30Z")).await.unwrap();
        start_cycle(ts.state(), &repo, true, GcOrigin::Manual, "admin")
            .await
            .unwrap();

        let started = tick(ts.state(), at("2030-01-13T03:00:30Z")).await.unwrap();
        assert_eq!(started.len(), 1, "[{}]", backend.name());
        sqlx::query("UPDATE gc_cycles SET state = 'aborted' WHERE dry_run = 1")
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        settle(&ts, &repo).await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_schedule_preflight_failure_recorded() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        seed_agent(&ts, &repo, "old", None).await;
        enable(&ts, &repo).await;
        tick(ts.state(), at("2030-01-06T03:00:30Z")).await.unwrap();

        let started = tick(ts.state(), at("2030-01-13T03:00:30Z")).await.unwrap();
        assert_eq!(started.len(), 1, "[{}]", backend.name());
        let all = cycles(&ts, &repo).await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].state, "failed");
        assert_eq!(all[0].origin, "schedule");
        assert!(!all[0].error_after_delete);
        let error = all[0].error.as_deref().unwrap();
        assert!(error.contains("don't support cleanup"), "{error}");
        assert!(error.contains("host-old"), "{error}");
        assert_eq!(cursor(&ts, &repo).await, Some(ms("2030-01-13T03:00:00Z")));
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_schedule_disabled_noop() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        db::save_settings(ts.state(), &repo, &weekly(false))
            .await
            .unwrap();
        for when in ["2030-01-06T03:00:30Z", "2030-01-13T03:00:30Z"] {
            assert!(tick(ts.state(), at(when)).await.unwrap().is_empty());
        }
        assert_eq!(cursor(&ts, &repo).await, None);

        let blank = GcSettings {
            enabled: true,
            cron: String::new(),
            ..GcSettings::default()
        };
        db::save_settings(ts.state(), &repo, &blank).await.unwrap();
        for when in ["2030-01-20T03:00:30Z", "2030-01-27T03:00:30Z"] {
            assert!(tick(ts.state(), at(when)).await.unwrap().is_empty());
        }
        assert_eq!(cursor(&ts, &repo).await, None);
        assert!(cycles(&ts, &repo).await.is_empty());
        ts.stop().await;
    }
}
