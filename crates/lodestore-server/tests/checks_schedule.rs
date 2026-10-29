mod common;

use common::{TestServer, WAIT, backends, wait_until};
use lodestore_policy::jiff::Timestamp;
use lodestore_server::maintenance::check::schedule::tick;
use lodestore_server::maintenance::check::{CheckLevel, CheckLevelSettings, db};
use sqlx::Row;

fn at(text: &str) -> Timestamp {
    text.parse().unwrap()
}

fn ms(text: &str) -> i64 {
    at(text).as_millisecond()
}

struct Seen {
    id: String,
    level: String,
    state: String,
    origin: String,
    started_by: String,
}

async fn runs(ts: &TestServer, repo: &str) -> Vec<Seen> {
    sqlx::query(
        "SELECT id, level, state, origin, started_by FROM check_runs \
         WHERE repository_id = $1 ORDER BY created_at, id",
    )
    .bind(repo)
    .fetch_all(&ts.state().db.pool)
    .await
    .unwrap()
    .iter()
    .map(|row| Seen {
        id: row.get("id"),
        level: row.get("level"),
        state: row.get("state"),
        origin: row.get("origin"),
        started_by: row.get("started_by"),
    })
    .collect()
}

async fn scheduled(ts: &TestServer, repo: &str) -> Vec<Seen> {
    runs(ts, repo)
        .await
        .into_iter()
        .filter(|run| run.origin == "schedule")
        .collect()
}

async fn cursor(ts: &TestServer, repo: &str, level: &str) -> Option<i64> {
    sqlx::query("SELECT last_slot_ms FROM check_settings WHERE repository_id = $1 AND level = $2")
        .bind(repo)
        .bind(level)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap()
        .get("last_slot_ms")
}

async fn insert_run(ts: &TestServer, repo: &str, level: &str, state: &str, created_at: i64) {
    sqlx::query(
        "INSERT INTO check_runs (id, repository_id, level, origin, started_by, state, created_at) \
         VALUES ($1, $2, $3, 'manual', 'test', $4, $5)",
    )
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(repo)
    .bind(level)
    .bind(state)
    .bind(created_at)
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
}

async fn seed_structure(ts: &TestServer, repo: &str) {
    insert_run(ts, repo, "structure", "done", 1).await;
}

async fn settle(ts: &TestServer, repo: &str) {
    let done = wait_until(WAIT, || async {
        runs(ts, repo)
            .await
            .iter()
            .all(|run| !["queued", "running"].contains(&run.state.as_str()))
    })
    .await;
    assert!(done, "a check never ended");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_schedule_runs_at_slot() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        seed_structure(&ts, &repo).await;

        let early = tick(ts.state(), at("2030-01-06T01:59:00Z")).await.unwrap();
        assert!(early.is_empty(), "[{}]", backend.name());
        assert_eq!(
            cursor(&ts, &repo, "structure").await,
            Some(ms("2030-01-05T02:00:00Z"))
        );

        let started = tick(ts.state(), at("2030-01-06T02:01:00Z")).await.unwrap();
        assert_eq!(started.len(), 1);
        assert_eq!(
            cursor(&ts, &repo, "structure").await,
            Some(ms("2030-01-06T02:00:00Z"))
        );
        let all = scheduled(&ts, &repo).await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, started[0]);
        assert_eq!(all[0].level, "structure");
        assert_eq!(all[0].started_by, "schedule");
        assert!(
            tick(ts.state(), at("2030-01-06T02:02:00Z"))
                .await
                .unwrap()
                .is_empty()
        );
        settle(&ts, &repo).await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_schedule_settings_change_waits() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        seed_structure(&ts, &repo).await;
        tick(ts.state(), at("2030-01-06T01:59:00Z")).await.unwrap();

        let levels = db::load_settings(ts.state(), &repo).await.unwrap();
        db::save_settings(ts.state(), &repo, &levels).await.unwrap();
        sqlx::query("UPDATE check_settings SET updated_at = $1 WHERE repository_id = $2")
            .bind(ms("2030-01-06T12:00:00Z"))
            .bind(&repo)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();

        let after = tick(ts.state(), at("2030-01-06T12:30:00Z")).await.unwrap();
        assert!(after.is_empty(), "[{}]", backend.name());
        assert_eq!(
            cursor(&ts, &repo, "structure").await,
            Some(ms("2030-01-06T12:00:00Z"))
        );
        assert!(scheduled(&ts, &repo).await.is_empty());

        let next = tick(ts.state(), at("2030-01-07T02:01:00Z")).await.unwrap();
        assert_eq!(next.len(), 1);
        settle(&ts, &repo).await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_schedule_catch_up_once() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        seed_structure(&ts, &repo).await;
        tick(ts.state(), at("2030-01-06T01:59:00Z")).await.unwrap();

        let started = tick(ts.state(), at("2030-01-09T03:00:00Z")).await.unwrap();
        assert_eq!(started.len(), 1, "[{}]", backend.name());
        settle(&ts, &repo).await;
        assert!(
            tick(ts.state(), at("2030-01-09T03:01:00Z"))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(scheduled(&ts, &repo).await.len(), 1);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_schedule_postponed_while_busy() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        seed_structure(&ts, &repo).await;
        tick(ts.state(), at("2030-01-06T01:59:00Z")).await.unwrap();
        let before = cursor(&ts, &repo, "structure").await;

        insert_run(&ts, &repo, "packs", "running", 2).await;
        let busy = tick(ts.state(), at("2030-01-06T02:01:00Z")).await.unwrap();
        assert!(busy.is_empty(), "[{}]", backend.name());
        assert_eq!(cursor(&ts, &repo, "structure").await, before);
        assert!(scheduled(&ts, &repo).await.is_empty());

        sqlx::query("UPDATE check_runs SET state = 'done' WHERE state = 'running'")
            .execute(&ts.state().db.pool)
            .await
            .unwrap();
        let free = tick(ts.state(), at("2030-01-06T02:02:00Z")).await.unwrap();
        assert_eq!(free.len(), 1);
        assert_eq!(
            cursor(&ts, &repo, "structure").await,
            Some(ms("2030-01-06T02:00:00Z"))
        );
        settle(&ts, &repo).await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_schedule_first_structure_now() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;

        let started = tick(ts.state(), at("2030-01-06T10:00:00Z")).await.unwrap();
        assert_eq!(started.len(), 1, "[{}]", backend.name());
        let all = scheduled(&ts, &repo).await;
        assert_eq!(all[0].level, "structure");
        assert!(
            tick(ts.state(), at("2030-01-06T10:01:00Z"))
                .await
                .unwrap()
                .is_empty()
        );
        settle(&ts, &repo).await;
        assert!(
            tick(ts.state(), at("2030-01-06T10:02:00Z"))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(scheduled(&ts, &repo).await.len(), 1);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_schedule_first_structure_not_repeated_after_cancel() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        insert_run(&ts, &repo, "structure", "aborted", 1).await;

        let started = tick(ts.state(), at("2030-01-06T10:00:00Z")).await.unwrap();
        assert!(started.is_empty(), "[{}]", backend.name());
        assert!(scheduled(&ts, &repo).await.is_empty());

        let next = tick(ts.state(), at("2030-01-07T02:01:00Z")).await.unwrap();
        assert_eq!(next.len(), 1);
        settle(&ts, &repo).await;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_schedule_disabled_level() {
    for backend in backends() {
        let ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        let mut levels = db::load_settings(ts.state(), &repo).await.unwrap();
        for (_, settings) in levels.iter_mut() {
            *settings = CheckLevelSettings {
                enabled: false,
                ..settings.clone()
            };
        }
        db::save_settings(ts.state(), &repo, &levels).await.unwrap();
        sqlx::query("UPDATE check_settings SET updated_at = 0 WHERE repository_id = $1")
            .bind(&repo)
            .execute(&ts.state().db.pool)
            .await
            .unwrap();

        for time in ["2030-01-06T10:00:00Z", "2030-03-02T05:01:00Z"] {
            let started = tick(ts.state(), at(time)).await.unwrap();
            assert!(started.is_empty(), "[{}]", backend.name());
        }
        assert!(runs(&ts, &repo).await.is_empty());
        let _ = CheckLevel::ALL;
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_schedule_restart_marks_failed() {
    for backend in backends() {
        let mut ts = TestServer::start_with(&backend, |c| c.schedule_tick_secs = 0).await;
        let repo = ts.create_local_repository("main").await;
        let scratch = ts.config.data_dir.join("check-restore");
        for state in ["running", "queued"] {
            insert_run(&ts, &repo, "restore", state, 5).await;
            std::fs::create_dir_all(scratch.join("run-1")).unwrap();
            std::fs::write(scratch.join("run-1").join("x"), b"data").unwrap();

            ts.restart().await;

            let all = sqlx::query(
                "SELECT state, outcome, error, finished_at FROM check_runs \
                 WHERE repository_id = $1 AND created_at = 5",
            )
            .bind(&repo)
            .fetch_all(&ts.state().db.pool)
            .await
            .unwrap();
            assert_eq!(all.len(), 1, "[{}]", backend.name());
            assert_eq!(all[0].get::<String, _>("state"), "failed");
            assert_eq!(
                all[0].get::<Option<String>, _>("outcome").as_deref(),
                Some("stopped")
            );
            assert_eq!(
                all[0].get::<Option<String>, _>("error").as_deref(),
                Some("The server restarted during the check.")
            );
            assert!(all[0].get::<Option<i64>, _>("finished_at").is_some());
            assert!(!scratch.exists(), "check-restore must be gone");
            sqlx::query("DELETE FROM check_runs WHERE created_at = 5")
                .execute(&ts.state().db.pool)
                .await
                .unwrap();
        }
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_schedule_restart_survives_stuck_leftover() {
    use std::os::unix::fs::PermissionsExt;

    let mut ts = TestServer::start_with(&backends()[0], |c| c.schedule_tick_secs = 0).await;
    let stuck = ts.config.data_dir.join("check-restore").join("run-1");
    std::fs::create_dir_all(stuck.join("inner")).unwrap();
    std::fs::write(stuck.join("inner").join("x"), b"data").unwrap();
    std::fs::set_permissions(stuck.join("inner"), std::fs::Permissions::from_mode(0o500)).unwrap();

    ts.restart().await;

    std::fs::set_permissions(stuck.join("inner"), std::fs::Permissions::from_mode(0o700)).unwrap();
    ts.stop().await;
}
