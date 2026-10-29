mod common;

use std::time::Duration;

use common::{TestServer, WAIT, backends, wait_until};
use lodestore_server::maintenance::check::run::supervise;
use lodestore_server::maintenance::check::{
    CheckLevel, CheckOrigin, MAX_PARALLEL_CHECKS, db, start_check,
};
use sqlx::Row;

async fn server(backend: &common::Backend) -> TestServer {
    TestServer::start_with(backend, |c| c.schedule_tick_secs = 0).await
}

async fn run_state(ts: &TestServer, id: &str) -> (String, Option<String>) {
    let row = sqlx::query("SELECT state, error FROM check_runs WHERE id = $1")
        .bind(id)
        .fetch_one(&ts.state().db.pool)
        .await
        .unwrap();
    (row.get("state"), row.get("error"))
}

fn boom() -> anyhow::Result<()> {
    panic!("boom")
}

async fn insert_running(ts: &TestServer, repo: &str, id: &str, level: &str) {
    sqlx::query(
        "INSERT INTO check_runs (id, repository_id, level, origin, started_by, state, \
         created_at) VALUES ($1, $2, $3, 'manual', 'test', 'running', 1)",
    )
    .bind(id)
    .bind(repo)
    .bind(level)
    .execute(&ts.state().db.pool)
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_gate_run_waits_for_a_free_slot() {
    for backend in backends() {
        let ts = server(&backend).await;
        let repo = ts.create_local_repository("main").await;
        let slots = ts.state().checks.slots.clone();
        let held = slots
            .acquire_many(u32::try_from(MAX_PARALLEL_CHECKS).unwrap())
            .await
            .unwrap();

        let id = start_check(
            ts.state(),
            &repo,
            CheckLevel::Structure,
            CheckOrigin::Manual,
            "test",
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            run_state(&ts, &id).await.0,
            "queued",
            "[{}]",
            backend.name()
        );

        drop(held);
        let done = wait_until(WAIT, || async { run_state(&ts, &id).await.0 == "done" }).await;
        assert!(done, "the queued check never ran");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_gate_admission_waits_for_cleanup_decision() {
    for backend in backends() {
        let ts = server(&backend).await;
        let repo = ts.create_local_repository("main").await;
        let admit = ts.state().checks.admit.clone();
        let held = admit.lock().await;

        let starting = tokio::spawn({
            let (state, repo) = (ts.state().clone(), repo.clone());
            async move {
                start_check(
                    &state,
                    &repo,
                    CheckLevel::Structure,
                    CheckOrigin::Manual,
                    "test",
                )
                .await
            }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!starting.is_finished(), "[{}]", backend.name());
        assert!(db::active_run(ts.state(), &repo).await.unwrap().is_none());

        drop(held);
        let id = starting.await.unwrap().unwrap();
        let done = wait_until(WAIT, || async { run_state(&ts, &id).await.0 == "done" }).await;
        assert!(done, "the check never ran");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_gate_panic_and_hang_end_the_run() {
    for backend in backends() {
        let ts = server(&backend).await;
        let repo = ts.create_local_repository("main").await;

        insert_running(&ts, &repo, "panics", "structure").await;
        supervise(ts.state(), "panics", Duration::from_secs(60), async {
            boom()
        })
        .await;
        let (state, error) = run_state(&ts, "panics").await;
        assert_eq!(state, "failed", "[{}]", backend.name());
        assert_eq!(error.as_deref(), Some("The check stopped unexpectedly."));

        insert_running(&ts, &repo, "hangs", "packs").await;
        supervise(ts.state(), "hangs", Duration::from_secs(1), async {
            std::future::pending::<()>().await;
            Ok(())
        })
        .await;
        let (state, error) = run_state(&ts, "hangs").await;
        assert_eq!(state, "failed");
        assert_eq!(
            error.as_deref(),
            Some("The check ran longer than 1s and was stopped.")
        );
        ts.stop().await;
    }
}
