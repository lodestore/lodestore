mod common;

use common::{TestServer, backends};
use sqlx::AnyPool;

async fn seed(pool: &AnyPool, repo: &str) {
    sqlx::query(
        "INSERT INTO agents (id, hostname, repository_id, labels_json, os, arch, version, status, \
         enrolled_at, cert_serial, cert_issued_at, cert_not_after) \
         VALUES ('h1', 'h1', $1, '{}', 'linux', 'x86_64', 'test', 'active', 0, 'aa', 0, 0)",
    )
    .bind(repo)
    .execute(pool)
    .await
    .unwrap();
    for id in ["p1", "p2"] {
        sqlx::query(
            "INSERT INTO policies (id, name, repository_id, yaml, spec_json, revision, created_at, updated_at) \
             VALUES ($1, $1, $2, '', '{}', 1, 0, 0)",
        )
        .bind(id)
        .bind(repo)
        .execute(pool)
        .await
        .unwrap();
    }
}

async fn enqueue(
    pool: &AnyPool,
    id: &str,
    policy: Option<&str>,
    source: &str,
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "INSERT INTO jobs (id, host_id, policy_id, source, state, spec, queued_at) \
         VALUES ($1, 'h1', $2, $3, 'queued', $4, 0) ON CONFLICT DO NOTHING",
    )
    .bind(id)
    .bind(policy)
    .bind(source)
    .bind(Vec::<u8>::new())
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
}

async fn set_state(pool: &AnyPool, id: &str, state: &str) -> Result<u64, sqlx::Error> {
    sqlx::query("UPDATE jobs SET state = $1 WHERE id = $2")
        .bind(state)
        .bind(id)
        .execute(pool)
        .await
        .map(|r| r.rows_affected())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jobs_schema_coalesces_active_policy_jobs() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let repo = ts.create_local_repository("main").await;
        let pool = &ts.state().db.pool;
        seed(pool, &repo).await;

        assert_eq!(enqueue(pool, "j1", Some("p1"), "manual").await.unwrap(), 1);
        assert_eq!(enqueue(pool, "j2", Some("p1"), "manual").await.unwrap(), 0);
        assert_eq!(enqueue(pool, "j3", Some("p2"), "manual").await.unwrap(), 1);

        set_state(pool, "j1", "running").await.unwrap();
        assert_eq!(enqueue(pool, "j4", Some("p1"), "manual").await.unwrap(), 0);

        set_state(pool, "j1", "succeeded").await.unwrap();
        assert_eq!(enqueue(pool, "j5", Some("p1"), "manual").await.unwrap(), 1);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jobs_schema_cli_jobs_never_coalesce() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let repo = ts.create_local_repository("main").await;
        let pool = &ts.state().db.pool;
        seed(pool, &repo).await;

        assert_eq!(enqueue(pool, "c1", None, "cli").await.unwrap(), 1);
        assert_eq!(enqueue(pool, "c2", None, "cli").await.unwrap(), 1);
        set_state(pool, "c1", "running").await.unwrap();
        set_state(pool, "c2", "running").await.unwrap();
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jobs_schema_one_active_job_per_host() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let repo = ts.create_local_repository("main").await;
        let pool = &ts.state().db.pool;
        seed(pool, &repo).await;

        enqueue(pool, "m1", Some("p1"), "manual").await.unwrap();
        enqueue(pool, "m2", Some("p2"), "manual").await.unwrap();
        set_state(pool, "m1", "assigned").await.unwrap();
        set_state(pool, "m2", "assigned")
            .await
            .expect_err("a second active manual job on the host");
        set_state(pool, "m1", "succeeded").await.unwrap();
        set_state(pool, "m2", "assigned").await.unwrap();
        ts.stop().await;
    }
}
