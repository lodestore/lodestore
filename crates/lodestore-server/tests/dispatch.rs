mod common;

use std::time::Duration;

use common::fake_agent::{Fake, Host, finished, insert_job, is_ack, is_assign, report, spec};
use common::{Api, TestServer, backends, wait_until};
use lodestore_agent::{EnrollArgs, NetOptions};
use lodestore_format::attest::read_attestations;
use lodestore_format::index::{hex32, parse_hex32};
use lodestore_format::provenance::ProvenanceSet;
use lodestore_format::repo::Repository;
use lodestore_format::store::LocalFsStore;
use lodestore_format::writer::{BackupOptions, Writer};
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{
    ConflictPolicy, Heartbeat, HookFacts, HookScript, IoniceClass, JobAccepted, JobCreated,
    JobEvent, JobEventLevel, JobEvents, JobOutcome, JobPhase, JobPhaseKind, JobProgress,
    JobRejected, JobReportState, JobSpec,
};
use lodestore_server::catalog::ingest_snapshot;
use lodestore_server::db::now_ms;
use lodestore_server::grpc::dispatch::janitor_once;
use lodestore_server::jobs::job_spec;
use lodestore_server::jobs::restore::{RestoreRequest, enqueue_restore};
use lodestore_server::maintenance::gc::{GcOrigin, GcSettings, db as gc_db, start_cycle};
use prost::Message;
use sqlx::{AnyPool, Row};
use tokio::sync::mpsc;

const PASSWORD: &[u8] = b"test-password";
const HOUR_MS: i64 = 3600 * 1000;
const DAY_MS: i64 = 24 * HOUR_MS;

async fn enroll(ts: &TestServer, api: &Api, repo: &str, hostname: &str) -> Host {
    let token = api.join_token(repo, serde_json::json!([])).await;
    let paths = ts.agent_paths(hostname);
    let enrolled = lodestore_agent::enroll(EnrollArgs {
        token,
        paths: paths.clone(),
        hostname: Some(hostname.to_string()),
        net: NetOptions::default(),
    })
    .await
    .expect("enroll");
    Host {
        id: enrolled.agent_id,
        paths,
    }
}

async fn text(pool: &AnyPool, sql: &'static str, id: &str) -> Option<String> {
    sqlx::query(sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap()
        .and_then(|row| row.try_get::<Option<String>, _>(0).unwrap())
}

async fn number(pool: &AnyPool, sql: &'static str, id: &str) -> i64 {
    sqlx::query(sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
        .try_get::<i64, _>(0)
        .unwrap()
}

async fn state_of(pool: &AnyPool, id: &str) -> String {
    text(pool, "SELECT state FROM jobs WHERE id = $1", id)
        .await
        .unwrap_or_else(|| "<missing>".to_string())
}

async fn wait_state(pool: &AnyPool, id: &str, want: &str) {
    let reached = wait_until(Duration::from_secs(10), || async {
        state_of(pool, id).await == want
    })
    .await;
    assert!(
        reached,
        "job {id} is '{}', expected '{want}'",
        state_of(pool, id).await
    );
}

async fn write_snapshot(ts: &TestServer, hostname: &str) -> String {
    write_snapshot_for(ts, hostname, None).await
}

async fn write_snapshot_for(ts: &TestServer, hostname: &str, agent_id: Option<&str>) -> String {
    let store = LocalFsStore::new(ts.local_repo_root("main"));
    let repo = Repository::open(&store, PASSWORD).await.unwrap();
    let keys = repo.read_keys().write_keys(repo.config_hash());
    let mut writer = Writer::new(&store, keys, repo.config()).await.unwrap();
    let data = ts.dir.path().join(format!("data-{hostname}"));
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("file.txt"), format!("content of {hostname}")).unwrap();
    let outcome = writer
        .backup(
            &[data],
            BackupOptions {
                hostname: Some(hostname.as_bytes().to_vec()),
                agent_id: agent_id.map(|a| a.as_bytes().to_vec()),
                ..BackupOptions::default()
            },
            &|_| {},
        )
        .await
        .expect("backup");
    hex32(&outcome.snapshot_id)
}

fn event(seq: u64) -> JobEvent {
    JobEvent {
        seq,
        at_ms: now_ms(),
        level: JobEventLevel::Info as i32,
        kind: "started".to_string(),
        message: format!("event {seq}"),
        ..Default::default()
    }
}

fn events(job_id: &str, range: std::ops::RangeInclusive<u64>) -> AgentBody {
    AgentBody::JobEvents(JobEvents {
        job_id: job_id.to_string(),
        events: range.map(event).collect(),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_assigns_and_finishes_job() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let pool = &ts.state().db.pool;
        insert_job(pool, "job-1", &host.id, "manual", "queued", now_ms()).await;

        let mut agent = Fake::connect(&ts, &host, vec![]).await;
        let assigned = agent.recv(is_assign).await;
        assert_eq!(assigned, "job-1", "[{}]", backend.name());
        assert_eq!(state_of(pool, "job-1").await, "assigned");

        agent
            .send(AgentBody::JobAccepted(JobAccepted {
                job_id: "job-1".to_string(),
                started_at_ms: now_ms(),
            }))
            .await;
        wait_state(pool, "job-1", "running").await;

        let snapshot = write_snapshot(&ts, "host-a").await;
        agent
            .send(AgentBody::JobFinished(finished(
                "job-1",
                JobOutcome::Succeeded,
                &snapshot,
            )))
            .await;
        assert_eq!(agent.recv(is_ack).await, "job-1");
        assert_eq!(state_of(pool, "job-1").await, "succeeded");
        assert_eq!(
            text(pool, "SELECT snapshot_id FROM jobs WHERE id = $1", "job-1")
                .await
                .as_deref(),
            Some(snapshot.as_str())
        );
        assert_eq!(
            text(
                pool,
                "SELECT job_id FROM snapshots WHERE id = $1",
                &snapshot
            )
            .await
            .as_deref(),
            Some("job-1")
        );
        assert_eq!(
            text(
                pool,
                "SELECT host_id FROM snapshots WHERE id = $1",
                &snapshot
            )
            .await
            .as_deref(),
            Some(host.id.as_str())
        );
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_unavailable_repository_fails_backup_job() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let pool = &ts.state().db.pool;
        sqlx::query("UPDATE repositories SET open_error = 'differs from the configuration recorded' WHERE id = $1")
            .bind(&repo)
            .execute(pool)
            .await
            .unwrap();
        let jobs = [
            ("job-1", "manual"),
            ("job-2", "schedule"),
            ("job-3", "catchup"),
        ];
        for (id, origin) in jobs {
            insert_job(pool, id, &host.id, "manual", "queued", now_ms()).await;
            sqlx::query("UPDATE jobs SET origin = $1 WHERE id = $2")
                .bind(origin)
                .bind(id)
                .execute(pool)
                .await
                .unwrap();
        }

        let mut agent = Fake::connect(&ts, &host, vec![]).await;
        for (id, origin) in jobs {
            wait_state(pool, id, "failed").await;
            assert_eq!(
                text(pool, "SELECT error_code FROM jobs WHERE id = $1", id)
                    .await
                    .as_deref(),
                Some("repository_unavailable"),
                "[{}] {origin} job",
                backend.name()
            );
        }
        let sent = agent.drain(Duration::from_millis(500)).await;
        assert!(
            sent.into_iter().all(|body| is_assign(body).is_none()),
            "[{}] the agent got a job for an unavailable repository",
            backend.name()
        );
        drop(agent);
        ts.stop().await;
    }
}

async fn signed(ts: &TestServer, repo: &str, snapshot: &str) -> bool {
    let keys = lodestore_server::catalog::open_repository(ts.state(), repo)
        .await
        .unwrap()
        .keys;
    let store = LocalFsStore::new(ts.local_repo_root("main"));
    let attested = read_attestations(&store, &keys).await.unwrap();
    ProvenanceSet::from_attested(&attested, &keys).contains(&parse_hex32(snapshot).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_finish_with_unclaimed_snapshot_is_not_signed() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let a = enroll(&ts, &api, &repo, "host-a").await;
        let b = enroll(&ts, &api, &repo, "host-b").await;
        let pool = &ts.state().db.pool;
        let unclaimed = write_snapshot_for(&ts, "host-b", None).await;

        let mut agent = Fake::connect(&ts, &b, vec![]).await;
        insert_job(pool, "job-b1", &b.id, "cli", "running", now_ms()).await;
        agent
            .send(AgentBody::JobFinished(finished(
                "job-b1",
                JobOutcome::Succeeded,
                &unclaimed,
            )))
            .await;
        assert_eq!(agent.recv(is_ack).await, "job-b1");
        wait_state(pool, "job-b1", "failed").await;
        let sql = "SELECT error_code FROM jobs WHERE id = $1";
        assert_eq!(
            text(pool, sql, "job-b1").await.as_deref(),
            Some("verification_failed"),
            "[{}]",
            backend.name()
        );
        let sql = "SELECT unverified_reason FROM snapshots WHERE id = $1";
        let reason = text(pool, sql, &unclaimed).await.unwrap_or_default();
        assert!(reason.contains("does not name the host"), "{reason}");
        assert!(!signed(&ts, &repo, &unclaimed).await);

        let theirs = write_snapshot_for(&ts, "host-a", Some(&a.id)).await;
        insert_job(pool, "job-b2", &b.id, "cli", "running", now_ms()).await;
        agent
            .send(AgentBody::JobFinished(finished(
                "job-b2",
                JobOutcome::Succeeded,
                &theirs,
            )))
            .await;
        assert_eq!(agent.recv(is_ack).await, "job-b2");
        let sql = "SELECT snapshot_id FROM jobs WHERE id = $1";
        assert_eq!(text(pool, sql, "job-b2").await, None);
        assert!(!signed(&ts, &repo, &theirs).await);
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_finish_never_adopts_a_foreign_snapshot() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let a = enroll(&ts, &api, &repo, "host-a").await;
        let b = enroll(&ts, &api, &repo, "host-b").await;
        let pool = &ts.state().db.pool;
        let theirs = write_snapshot(&ts, "host-a").await;
        lodestore_server::catalog::ingest_snapshot(
            ts.state(),
            &repo,
            &theirs,
            Some(lodestore_server::catalog::JobLink {
                job_id: "job-a",
                host_id: &a.id,
            }),
        )
        .await
        .unwrap();

        let mut agent = Fake::connect(&ts, &b, vec![]).await;
        insert_job(pool, "job-b1", &b.id, "cli", "running", now_ms()).await;
        agent
            .send(AgentBody::JobFinished(finished(
                "job-b1",
                JobOutcome::Succeeded,
                &theirs,
            )))
            .await;
        assert_eq!(agent.recv(is_ack).await, "job-b1");
        assert_eq!(state_of(pool, "job-b1").await, "succeeded");
        let sql = "SELECT snapshot_id FROM jobs WHERE id = $1";
        assert_eq!(
            text(pool, sql, "job-b1").await,
            None,
            "[{}]",
            backend.name()
        );
        let sql = "SELECT host_id FROM snapshots WHERE id = $1";
        assert_eq!(
            text(pool, sql, &theirs).await.as_deref(),
            Some(a.id.as_str())
        );
        let sql = "SELECT job_id FROM snapshots WHERE id = $1";
        assert_eq!(text(pool, sql, &theirs).await.as_deref(), Some("job-a"));

        insert_job(pool, "job-b2", &b.id, "cli", "running", now_ms()).await;
        agent
            .send(AgentBody::JobFinished(finished(
                "job-b2",
                JobOutcome::Succeeded,
                "not-a-snapshot-id",
            )))
            .await;
        assert_eq!(agent.recv(is_ack).await, "job-b2");
        let sql = "SELECT snapshot_id FROM jobs WHERE id = $1";
        assert_eq!(text(pool, sql, "job-b2").await, None);

        let own = write_snapshot(&ts, "host-b").await;
        insert_job(pool, "job-b3", &b.id, "cli", "running", now_ms()).await;
        agent
            .send(AgentBody::JobFinished(finished(
                "job-b3",
                JobOutcome::Succeeded,
                &own.to_uppercase(),
            )))
            .await;
        assert_eq!(agent.recv(is_ack).await, "job-b3");
        assert_eq!(
            text(pool, sql, "job-b3").await.as_deref(),
            Some(own.as_str())
        );
        let sql = "SELECT job_id FROM snapshots WHERE id = $1";
        assert_eq!(text(pool, sql, &own).await.as_deref(), Some("job-b3"));
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_reconcile_table() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let pool = &ts.state().db.pool;
        let tag = backend.name();

        let host = enroll(&ts, &api, &repo, "h-assigned").await;
        insert_job(pool, "a1", &host.id, "manual", "assigned", now_ms()).await;
        let mut agent = Fake::connect(&ts, &host, vec![]).await;
        assert_eq!(
            agent.recv(is_assign).await,
            "a1",
            "[{tag}] requeued and resent"
        );
        drop(agent);

        let host = enroll(&ts, &api, &repo, "h-running").await;
        insert_job(pool, "r1", &host.id, "manual", "running", now_ms()).await;
        let agent = Fake::connect(&ts, &host, vec![]).await;
        wait_state(pool, "r1", "interrupted").await;
        assert_eq!(
            text(pool, "SELECT error_code FROM jobs WHERE id = $1", "r1")
                .await
                .as_deref(),
            Some("interrupted")
        );
        drop(agent);

        let host = enroll(&ts, &api, &repo, "h-alive").await;
        insert_job(pool, "k1", &host.id, "manual", "running", now_ms()).await;
        sqlx::query("UPDATE jobs SET log_seq = 7 WHERE id = 'k1'")
            .execute(pool)
            .await
            .unwrap();
        let mut agent =
            Fake::connect(&ts, &host, vec![report("k1", JobReportState::Running)]).await;
        let resend = agent
            .recv(|b| match b {
                ServerBody::JobSync(s) => Some((s.job_id, s.resend_events_from)),
                _ => None,
            })
            .await;
        assert_eq!(resend, ("k1".to_string(), 8));
        assert_eq!(state_of(pool, "k1").await, "running");
        drop(agent);

        let host = enroll(&ts, &api, &repo, "h-cancel").await;
        insert_job(pool, "c1", &host.id, "manual", "running", now_ms()).await;
        sqlx::query("UPDATE jobs SET cancel_requested = 1 WHERE id = 'c1'")
            .execute(pool)
            .await
            .unwrap();
        let mut agent =
            Fake::connect(&ts, &host, vec![report("c1", JobReportState::Running)]).await;
        let cancelled = agent
            .recv(|b| match b {
                ServerBody::JobCancel(c) => Some(c.job_id),
                _ => None,
            })
            .await;
        assert_eq!(cancelled, "c1");
        drop(agent);

        let host = enroll(&ts, &api, &repo, "h-finished").await;
        insert_job(pool, "f1", &host.id, "manual", "running", now_ms()).await;
        let mut done = report("f1", JobReportState::Finished);
        done.finished = Some(finished("f1", JobOutcome::Warnings, ""));
        let mut agent = Fake::connect(&ts, &host, vec![done]).await;
        assert_eq!(agent.recv(is_ack).await, "f1");
        assert_eq!(state_of(pool, "f1").await, "warnings");
        drop(agent);

        let host = enroll(&ts, &api, &repo, "h-unknown").await;
        let mut cli = report("cli-1", JobReportState::Running);
        cli.created = Some(JobCreated {
            job_id: "cli-1".to_string(),
            spec: Some(JobSpec {
                stdin_name: "app.dump".to_string(),
                ..Default::default()
            }),
            caller_uid: 1000,
            caller_gid: 100,
            created_at_ms: now_ms(),
        });
        let manual = report("gone-1", JobReportState::Running);
        let mut agent = Fake::connect(&ts, &host, vec![cli, manual]).await;
        let forgotten = agent
            .recv(|b| match b {
                ServerBody::JobForget(f) => Some(f.job_id),
                _ => None,
            })
            .await;
        assert_eq!(forgotten, "gone-1");
        assert_eq!(state_of(pool, "cli-1").await, "running");
        assert_eq!(
            text(pool, "SELECT source FROM jobs WHERE id = $1", "cli-1")
                .await
                .as_deref(),
            Some("cli")
        );
        assert_eq!(
            text(pool, "SELECT stdin_name FROM jobs WHERE id = $1", "cli-1")
                .await
                .as_deref(),
            Some("app.dump")
        );
        assert_eq!(
            number(pool, "SELECT caller_uid FROM jobs WHERE id = $1", "cli-1").await,
            1000
        );
        assert_eq!(state_of(pool, "gone-1").await, "<missing>");
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_finish_is_idempotent_and_acked() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let pool = &ts.state().db.pool;
        insert_job(pool, "job-1", &host.id, "manual", "running", now_ms()).await;

        let agent = Fake::connect(&ts, &host, vec![report("job-1", JobReportState::Running)]).await;
        let mut agent = agent;
        let mut first = finished("job-1", JobOutcome::Succeeded, "");
        first.finished_at_ms = 1_700_000_000_000;
        agent.send(AgentBody::JobFinished(first.clone())).await;
        assert_eq!(agent.recv(is_ack).await, "job-1");
        assert_eq!(state_of(pool, "job-1").await, "succeeded");

        let mut again = finished("job-1", JobOutcome::Failed, "");
        again.error_code = "backup_failed".to_string();
        again.finished_at_ms = 1_800_000_000_000;
        agent.send(AgentBody::JobFinished(again)).await;
        assert_eq!(agent.recv(is_ack).await, "job-1");
        assert_eq!(state_of(pool, "job-1").await, "succeeded");
        assert_eq!(
            number(pool, "SELECT finished_at FROM jobs WHERE id = $1", "job-1").await,
            1_700_000_000_000,
            "[{}]",
            backend.name()
        );
        drop(agent);

        let mut done = report("job-1", JobReportState::Finished);
        done.finished = Some(first);
        let mut agent = Fake::connect(&ts, &host, vec![done]).await;
        assert_eq!(agent.recv(is_ack).await, "job-1");
        assert_eq!(state_of(pool, "job-1").await, "succeeded");
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_events_dedup_and_cap() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let pool = &ts.state().db.pool;
        insert_job(pool, "job-1", &host.id, "manual", "running", now_ms()).await;
        let agent = Fake::connect(&ts, &host, vec![report("job-1", JobReportState::Running)]).await;

        let count = || async {
            number(
                pool,
                "SELECT COUNT(*) FROM job_events WHERE job_id = $1",
                "job-1",
            )
            .await
        };
        agent.send(events("job-1", 1..=3)).await;
        agent.send(events("job-1", 2..=4)).await;
        let settled = wait_until(Duration::from_secs(10), || async {
            number(pool, "SELECT log_seq FROM jobs WHERE id = $1", "job-1").await == 4
        })
        .await;
        assert!(settled, "[{}]", backend.name());
        assert_eq!(count().await, 4);
        assert_eq!(
            number(pool, "SELECT log_lines FROM jobs WHERE id = $1", "job-1").await,
            4
        );

        for start in (5..=6000).step_by(1000) {
            agent
                .send(events("job-1", start..=(start + 999).min(6000)))
                .await;
        }
        let settled = wait_until(Duration::from_secs(30), || async {
            number(pool, "SELECT log_seq FROM jobs WHERE id = $1", "job-1").await == 6000
        })
        .await;
        assert!(settled);
        assert_eq!(count().await, 5000);
        let last = text(
            pool,
            "SELECT kind FROM job_events WHERE job_id = $1 ORDER BY seq DESC LIMIT 1",
            "job-1",
        )
        .await;
        assert_eq!(last.as_deref(), Some("log_limit"));

        agent.send(events("job-1", 1..=100)).await;
        agent.send(events("job-1", 6001..=6002)).await;
        let settled = wait_until(Duration::from_secs(10), || async {
            number(pool, "SELECT log_seq FROM jobs WHERE id = $1", "job-1").await == 6002
        })
        .await;
        assert!(settled);
        assert_eq!(count().await, 5000);
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_progress_live_and_flushed() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let pool = &ts.state().db.pool;
        insert_job(pool, "job-1", &host.id, "manual", "running", now_ms()).await;
        let agent = Fake::connect(&ts, &host, vec![report("job-1", JobReportState::Running)]).await;
        let live = ts.state().registry.live.clone();

        agent
            .send(AgentBody::JobProgress(JobProgress {
                job_id: "job-1".to_string(),
                files_done: 11,
                bytes_done: 2048,
                scan_done: true,
                eta_s: Some(30),
                upload_speed_bps: 1_048_576,
                ..Default::default()
            }))
            .await;
        let seen = wait_until(Duration::from_secs(5), || async {
            live.get("job-1").is_some_and(|p| p.files_done == 11)
        })
        .await;
        assert!(seen, "[{}]", backend.name());
        assert_eq!(live.get("job-1").unwrap().eta_s, Some(30));
        assert_eq!(live.get("job-1").unwrap().upload_speed_bps, 1_048_576);

        let flushed = wait_until(Duration::from_secs(12), || async {
            text(
                pool,
                "SELECT progress_json FROM jobs WHERE id = $1",
                "job-1",
            )
            .await
            .is_some_and(|json| json.contains("\"files_done\":11"))
        })
        .await;
        assert!(flushed, "progress was not flushed to the database");

        agent
            .send(AgentBody::JobPhase(JobPhase {
                job_id: "job-1".to_string(),
                phase: JobPhaseKind::Backup as i32,
                script: String::new(),
            }))
            .await;
        agent
            .send(AgentBody::JobProgress(JobProgress {
                job_id: "job-1".to_string(),
                files_done: 12,
                ..Default::default()
            }))
            .await;
        let mut agent = agent;
        agent
            .send(AgentBody::JobFinished(finished(
                "job-1",
                JobOutcome::Succeeded,
                "",
            )))
            .await;
        assert_eq!(agent.recv(is_ack).await, "job-1");
        let stored = text(
            pool,
            "SELECT progress_json FROM jobs WHERE id = $1",
            "job-1",
        )
        .await
        .unwrap();
        assert!(stored.contains("\"files_done\":12"), "{stored}");
        assert!(live.get("job-1").is_none());
        assert_eq!(
            text(pool, "SELECT phase FROM jobs WHERE id = $1", "job-1").await,
            None
        );
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_cancel_pushes_job_cancel() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let pool = &ts.state().db.pool;
        insert_job(pool, "job-1", &host.id, "manual", "running", now_ms()).await;
        let mut agent =
            Fake::connect(&ts, &host, vec![report("job-1", JobReportState::Running)]).await;
        agent.drain(Duration::from_millis(300)).await;

        sqlx::query("UPDATE jobs SET cancel_requested = 1 WHERE id = 'job-1'")
            .execute(pool)
            .await
            .unwrap();
        let cancels = |messages: &[ServerBody]| {
            messages
                .iter()
                .filter(|b| matches!(b, ServerBody::JobCancel(_)))
                .count()
        };
        let first = agent
            .recv(|b| match b {
                ServerBody::JobCancel(c) => Some(c.job_id),
                _ => None,
            })
            .await;
        assert_eq!(first, "job-1", "[{}]", backend.name());
        assert_eq!(cancels(&agent.drain(Duration::from_millis(2500)).await), 0);
        drop(agent);

        let mut agent =
            Fake::connect(&ts, &host, vec![report("job-1", JobReportState::Running)]).await;
        let again = agent.drain(Duration::from_millis(1500)).await;
        assert_eq!(cancels(&again), 1);
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_cli_job_created() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let pool = &ts.state().db.pool;
        sqlx::query(
            "INSERT INTO policies (id, name, repository_id, yaml, spec_json, revision, created_at, updated_at) \
             VALUES ('pol-1', 'etc', $1, '', '{}', 1, 0, 0)",
        )
        .bind(&repo)
        .execute(pool)
        .await
        .unwrap();
        let agent = Fake::connect(&ts, &host, vec![]).await;

        let created = |id: &str| {
            AgentBody::JobCreated(JobCreated {
                job_id: id.to_string(),
                spec: Some(JobSpec {
                    policy_name: "etc".to_string(),
                    stdin_name: "app.dump".to_string(),
                    ..Default::default()
                }),
                caller_uid: 1000,
                caller_gid: 100,
                created_at_ms: now_ms(),
            })
        };
        agent.send(created("cli-1")).await;
        wait_state(pool, "cli-1", "running").await;
        assert_eq!(
            text(pool, "SELECT policy_id FROM jobs WHERE id = $1", "cli-1")
                .await
                .as_deref(),
            Some("pol-1"),
            "[{}]",
            backend.name()
        );
        assert_eq!(
            text(pool, "SELECT source FROM jobs WHERE id = $1", "cli-1")
                .await
                .as_deref(),
            Some("cli")
        );

        let too_long = "x".repeat(65);
        agent.send(created(&too_long)).await;
        agent.send(created("bad id")).await;
        agent.send(created("cli-2")).await;
        wait_state(pool, "cli-2", "running").await;
        assert_eq!(state_of(pool, &too_long).await, "<missing>");
        assert_eq!(state_of(pool, "bad id").await, "<missing>");
        assert_eq!(
            text(pool, "SELECT policy_id FROM jobs WHERE id = $1", "cli-2").await,
            None
        );
        assert_eq!(
            text(pool, "SELECT policy_name FROM jobs WHERE id = $1", "cli-2")
                .await
                .as_deref(),
            Some("etc")
        );
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_rejects_foreign_job_messages() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let mine = enroll(&ts, &api, &repo, "host-a").await;
        let other = enroll(&ts, &api, &repo, "host-b").await;
        let pool = &ts.state().db.pool;
        insert_job(pool, "theirs", &other.id, "manual", "running", now_ms()).await;
        insert_job(pool, "ours", &mine.id, "manual", "assigned", now_ms()).await;
        let mut agent =
            Fake::connect(&ts, &mine, vec![report("ours", JobReportState::Accepted)]).await;
        agent.drain(Duration::from_millis(300)).await;

        agent
            .send(AgentBody::JobAccepted(JobAccepted {
                job_id: "theirs".to_string(),
                started_at_ms: 1,
            }))
            .await;
        agent.send(events("theirs", 1..=3)).await;
        agent
            .send(AgentBody::JobProgress(JobProgress {
                job_id: "theirs".to_string(),
                files_done: 5,
                ..Default::default()
            }))
            .await;
        agent
            .send(AgentBody::JobPhase(JobPhase {
                job_id: "theirs".to_string(),
                phase: JobPhaseKind::Snapshot as i32,
                script: String::new(),
            }))
            .await;
        agent
            .send(AgentBody::JobFinished(finished(
                "theirs",
                JobOutcome::Failed,
                "",
            )))
            .await;
        agent.send(events("ours", 1..=1)).await;
        let own_events = wait_until(Duration::from_secs(10), || async {
            number(pool, "SELECT log_seq FROM jobs WHERE id = $1", "ours").await == 1
        })
        .await;
        assert!(own_events, "[{}]", backend.name());

        assert_eq!(state_of(pool, "theirs").await, "running");
        assert_eq!(
            number(
                pool,
                "SELECT COUNT(*) FROM job_events WHERE job_id = $1",
                "theirs"
            )
            .await,
            0
        );
        assert!(ts.state().registry.live.get("theirs").is_none());
        assert_eq!(
            text(pool, "SELECT phase FROM jobs WHERE id = $1", "theirs").await,
            None
        );
        assert_eq!(
            number(
                pool,
                "SELECT COUNT(*) FROM jobs WHERE id = $1 AND finished_at IS NULL",
                "theirs"
            )
            .await,
            1
        );
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_janitor_rules() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let pool = &ts.state().db.pool;
        let now = now_ms();

        insert_job(
            pool,
            "stale-queued",
            &host.id,
            "manual",
            "queued",
            now - 25 * HOUR_MS,
        )
        .await;
        insert_job(
            pool,
            "fresh-queued",
            &host.id,
            "cli",
            "queued",
            now - HOUR_MS,
        )
        .await;
        insert_job(
            pool,
            "stuck-assigned",
            &host.id,
            "manual",
            "assigned",
            now - 3 * 60 * 1000,
        )
        .await;
        for (id, age_days, state) in [
            ("old-failed", 31, "failed"),
            ("older-done", 91, "succeeded"),
            ("old-interrupted", 40, "interrupted"),
        ] {
            insert_job(pool, id, &host.id, "cli", state, now - age_days * DAY_MS).await;
            sqlx::query("UPDATE jobs SET finished_at = $1 WHERE id = $2")
                .bind(now - age_days * DAY_MS)
                .bind(id)
                .execute(pool)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO job_events (job_id, seq, at, level, kind, message) \
                 VALUES ($1, 1, 0, 'info', 'started', 'x')",
            )
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        }

        janitor_once(ts.state(), now).await.unwrap();
        let tag = backend.name();

        assert_eq!(
            state_of(pool, "stale-queued").await,
            "not_started",
            "[{tag}]"
        );
        assert_eq!(
            text(
                pool,
                "SELECT error_code FROM jobs WHERE id = $1",
                "stale-queued"
            )
            .await
            .as_deref(),
            Some("not_started")
        );
        assert_eq!(state_of(pool, "fresh-queued").await, "queued");
        assert_eq!(state_of(pool, "stuck-assigned").await, "queued");

        assert_eq!(state_of(pool, "older-done").await, "<missing>");
        assert_eq!(
            number(
                pool,
                "SELECT COUNT(*) FROM job_events WHERE job_id = $1",
                "older-done"
            )
            .await,
            0
        );
        for id in ["old-failed", "old-interrupted"] {
            assert_eq!(
                number(
                    pool,
                    "SELECT COUNT(*) FROM job_events WHERE job_id = $1",
                    id
                )
                .await,
                0
            );
            assert_eq!(
                number(pool, "SELECT logs_purged FROM jobs WHERE id = $1", id).await,
                1
            );
        }
        assert_eq!(state_of(pool, "old-failed").await, "failed");
        assert_eq!(state_of(pool, "old-interrupted").await, "interrupted");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_one_active_per_host() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let pool = &ts.state().db.pool;
        let now = now_ms();
        insert_job(pool, "first", &host.id, "manual", "queued", now - 2000).await;
        insert_job(pool, "second", &host.id, "manual", "queued", now - 1000).await;

        let mut agent = Fake::connect(&ts, &host, vec![]).await;
        assert_eq!(agent.recv(is_assign).await, "first", "[{}]", backend.name());
        let others = agent.drain(Duration::from_millis(2500)).await;
        assert!(others.into_iter().all(|b| is_assign(b).is_none()));
        assert_eq!(state_of(pool, "second").await, "queued");

        agent
            .send(AgentBody::JobAccepted(JobAccepted {
                job_id: "first".to_string(),
                started_at_ms: now_ms(),
            }))
            .await;
        agent
            .send(AgentBody::JobFinished(finished(
                "first",
                JobOutcome::Succeeded,
                "",
            )))
            .await;
        assert_eq!(agent.recv(is_ack).await, "first");
        assert_eq!(agent.recv(is_assign).await, "second");
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_waits_for_agent_update() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let pool = &ts.state().db.pool;
        let mut agent = Fake::connect_with(&ts, &host, &["update_v1"]).await;

        sqlx::query(
            "INSERT INTO agent_rollouts (id, target_version, state, canary, wave_size, \
             pause_after, created_by, created_at) \
             VALUES ('r1', '99.0.0', 'paused', 0, 10, 5, 'test', $1)",
        )
        .bind(now_ms())
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO agent_updates (id, rollout_id, host_id, arch, from_version, \
             target_version, wave, state, queued_at, assigned_at) \
             VALUES ('u1', 'r1', $1, 'x86_64', '0.0.1', '99.0.0', 0, 'assigned', $2, $2)",
        )
        .bind(&host.id)
        .bind(now_ms())
        .execute(pool)
        .await
        .unwrap();
        insert_job(pool, "job-1", &host.id, "manual", "queued", now_ms()).await;

        let others = agent.drain(Duration::from_millis(2500)).await;
        assert!(others.into_iter().all(|b| is_assign(b).is_none()));
        assert_eq!(
            state_of(pool, "job-1").await,
            "queued",
            "[{}]",
            backend.name()
        );

        sqlx::query("UPDATE agent_updates SET state = 'succeeded' WHERE id = 'u1'")
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(agent.recv(is_assign).await, "job-1");
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_hook_facts_persisted() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let pool = &ts.state().db.pool;
        let agent = Fake::connect(&ts, &host, vec![]).await;

        agent
            .send(AgentBody::Heartbeat(Heartbeat {
                hook_facts: Some(HookFacts {
                    allow_inline: true,
                    scripts: vec![
                        HookScript {
                            name: "dump-db".to_string(),
                            ok: true,
                        },
                        HookScript {
                            name: "bad".to_string(),
                            ok: false,
                        },
                    ],
                }),
                ..Default::default()
            }))
            .await;
        let stored = wait_until(Duration::from_secs(10), || async {
            text(
                pool,
                "SELECT facts_json FROM agents WHERE id = $1",
                &host.id,
            )
            .await
            .is_some_and(|f| f.contains("hooks"))
        })
        .await;
        assert!(stored, "[{}]", backend.name());
        let facts: serde_json::Value = serde_json::from_str(
            &text(
                pool,
                "SELECT facts_json FROM agents WHERE id = $1",
                &host.id,
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            facts["hooks"],
            serde_json::json!({
                "allow_inline": true,
                "scripts": [{"name": "dump-db", "ok": true}, {"name": "bad", "ok": false}],
            })
        );
        drop(agent);

        let many: Vec<HookScript> = (0..300)
            .map(|n| HookScript {
                name: format!("s{n}"),
                ok: true,
            })
            .collect();
        let (tx, rx) = mpsc::channel(8);
        let agent = Fake::open(&ts, &host, tx, rx, vec![]).await;
        sqlx::query("UPDATE agents SET facts_json = NULL WHERE id = $1")
            .bind(&host.id)
            .execute(pool)
            .await
            .unwrap();
        agent
            .send(AgentBody::Heartbeat(Heartbeat {
                hook_facts: Some(HookFacts {
                    allow_inline: false,
                    scripts: many,
                }),
                ..Default::default()
            }))
            .await;
        let stored = wait_until(Duration::from_secs(10), || async {
            text(
                pool,
                "SELECT facts_json FROM agents WHERE id = $1",
                &host.id,
            )
            .await
            .is_some_and(|f| f.contains("hooks"))
        })
        .await;
        assert!(stored);
        let facts: serde_json::Value = serde_json::from_str(
            &text(
                pool,
                "SELECT facts_json FROM agents WHERE id = $1",
                &host.id,
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!(facts["hooks"]["scripts"].as_array().unwrap().len(), 256);
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_no_auto_retry() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let pool = &ts.state().db.pool;
        let now = now_ms();
        insert_job(
            pool,
            "was-failed",
            &host.id,
            "manual",
            "failed",
            now - HOUR_MS,
        )
        .await;
        insert_job(
            pool,
            "was-cancelled",
            &host.id,
            "manual",
            "cancelled",
            now - HOUR_MS,
        )
        .await;
        insert_job(
            pool,
            "was-running",
            &host.id,
            "manual",
            "running",
            now - HOUR_MS,
        )
        .await;
        for id in ["was-failed", "was-cancelled"] {
            sqlx::query("UPDATE jobs SET finished_at = $1 WHERE id = $2")
                .bind(now - HOUR_MS)
                .bind(id)
                .execute(pool)
                .await
                .unwrap();
        }

        let mut agent = Fake::connect(&ts, &host, vec![]).await;
        wait_state(pool, "was-running", "interrupted").await;
        janitor_once(ts.state(), now_ms()).await.unwrap();
        let received = agent.drain(Duration::from_millis(2500)).await;
        assert!(
            received.into_iter().all(|b| is_assign(b).is_none()),
            "[{}]",
            backend.name()
        );
        drop(agent);

        let mut agent = Fake::connect(&ts, &host, vec![]).await;
        let received = agent.drain(Duration::from_millis(1500)).await;
        assert!(received.into_iter().all(|b| is_assign(b).is_none()));
        for (id, want) in [
            ("was-failed", "failed"),
            ("was-cancelled", "cancelled"),
            ("was-running", "interrupted"),
        ] {
            assert_eq!(state_of(pool, id).await, want);
        }
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_reject_requeues_only_when_busy() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let pool = &ts.state().db.pool;
        insert_job(pool, "job-1", &host.id, "manual", "queued", now_ms()).await;

        let mut agent = Fake::connect(&ts, &host, vec![]).await;
        assert_eq!(agent.recv(is_assign).await, "job-1");
        agent
            .send(AgentBody::JobRejected(JobRejected {
                job_id: "job-1".to_string(),
                code: "busy".to_string(),
                message: "another backup is running".to_string(),
            }))
            .await;
        assert_eq!(agent.recv(is_assign).await, "job-1", "[{}]", backend.name());

        agent
            .send(AgentBody::JobRejected(JobRejected {
                job_id: "job-1".to_string(),
                code: "store_failed".to_string(),
                message: "disk full".to_string(),
            }))
            .await;
        wait_state(pool, "job-1", "failed").await;
        assert_eq!(
            text(pool, "SELECT error_code FROM jobs WHERE id = $1", "job-1")
                .await
                .as_deref(),
            Some("store_failed")
        );
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_requeue_honours_a_pending_cancel() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let pool = &ts.state().db.pool;
        let now = now_ms();
        insert_job(pool, "job-1", &host.id, "manual", "queued", now).await;

        let mut agent = Fake::connect(&ts, &host, vec![]).await;
        assert_eq!(agent.recv(is_assign).await, "job-1");
        sqlx::query("UPDATE jobs SET cancel_requested = 1 WHERE id = 'job-1'")
            .execute(pool)
            .await
            .unwrap();
        agent
            .send(AgentBody::JobRejected(JobRejected {
                job_id: "job-1".to_string(),
                code: "busy".to_string(),
                message: "another backup is running".to_string(),
            }))
            .await;
        wait_state(pool, "job-1", "cancelled").await;
        assert!(
            number(pool, "SELECT finished_at FROM jobs WHERE id = $1", "job-1").await > 0,
            "[{}]",
            backend.name()
        );
        let assigns = agent
            .drain(Duration::from_millis(1500))
            .await
            .into_iter()
            .filter_map(is_assign)
            .count();
        assert_eq!(assigns, 0);
        drop(agent);
        assert!(
            wait_until(Duration::from_secs(10), || async {
                !ts.state().registry.is_online(&host.id)
            })
            .await
        );

        insert_job(
            pool,
            "job-2",
            &host.id,
            "manual",
            "assigned",
            now - 3 * 60 * 1000,
        )
        .await;
        sqlx::query("UPDATE jobs SET cancel_requested = 1 WHERE id = 'job-2'")
            .execute(pool)
            .await
            .unwrap();
        janitor_once(ts.state(), now).await.unwrap();
        assert_eq!(state_of(pool, "job-2").await, "cancelled");
        ts.stop().await;
    }
}

fn policy_spec(limits: &str) -> JobSpec {
    let yaml = format!(
        "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: etc\n\
         spec:\n  repository: main\n  paths:\n    - /etc\n{limits}"
    );
    job_spec(&lodestore_policy::parse_yaml(&yaml).expect("policy"))
}

async fn set_spec(pool: &AnyPool, id: &str, spec: &JobSpec) {
    sqlx::query("UPDATE jobs SET spec = $1 WHERE id = $2")
        .bind(spec.encode_to_vec())
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

async fn mark_seen(pool: &AnyPool, host: &str, at: i64) {
    sqlx::query("UPDATE agents SET last_seen_at = $1 WHERE id = $2")
        .bind(at)
        .bind(host)
        .execute(pool)
        .await
        .unwrap();
}

async fn set_times(pool: &AnyPool, id: &str, started: Option<i64>, assigned: i64) {
    sqlx::query("UPDATE jobs SET started_at = $1, assigned_at = $2 WHERE id = $3")
        .bind(started)
        .bind(assigned)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_spec_carries_max_runtime() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let pool = &ts.state().db.pool;
        let cases = [
            ("h-default", policy_spec(""), 43_200),
            (
                "h-two-hours",
                policy_spec("  limits:\n    maxRuntime: 2h\n"),
                7_200,
            ),
            ("h-specless", spec(), 43_200),
        ];
        for (hostname, spec, want) in cases {
            let host = enroll(&ts, &api, &repo, hostname).await;
            let id = format!("job-{hostname}");
            insert_job(pool, &id, &host.id, "manual", "queued", now_ms()).await;
            set_spec(pool, &id, &spec).await;
            let mut agent = Fake::connect(&ts, &host, vec![]).await;
            let assigned = agent
                .recv(|b| match b {
                    ServerBody::JobAssign(a) => a.spec,
                    _ => None,
                })
                .await;
            assert_eq!(
                assigned.max_runtime_s,
                want,
                "[{}] {hostname}",
                backend.name()
            );
            assert!(!assigned.gc_locks_required);
            assert!(
                assigned.limits.is_none(),
                "maxRuntime alone must not create limits"
            );
        }
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_hello_persists_capabilities() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let pool = &ts.state().db.pool;
        let stored = |pool: &AnyPool| {
            let id = host.id.clone();
            let pool = pool.clone();
            async move {
                let raw = text(
                    &pool,
                    "SELECT capabilities_json FROM agents WHERE id = $1",
                    &id,
                )
                .await;
                raw.map(|r| serde_json::from_str::<Vec<String>>(&r).unwrap())
            }
        };
        assert_eq!(stored(pool).await, None, "[{}]", backend.name());

        let agent = Fake::connect_with(&ts, &host, &["restore_v1", "gc_v1"]).await;
        assert_eq!(
            stored(pool).await,
            Some(vec!["restore_v1".to_string(), "gc_v1".to_string()]),
            "[{}]",
            backend.name()
        );
        drop(agent);

        let agent = Fake::connect_with(&ts, &host, &["restore_v1"]).await;
        assert_eq!(
            stored(pool).await,
            Some(vec!["restore_v1".to_string()]),
            "[{}]",
            backend.name()
        );
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_janitor_time_limit_fails() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let pool = &ts.state().db.pool;
        let now = now_ms();
        let overdue = 2 * HOUR_MS + HOUR_MS + 1000;
        let limited = policy_spec("  limits:\n    maxRuntime: 2h\n");
        for (id, state) in [("running-late", "running"), ("unstamped-late", "running")] {
            insert_job(pool, id, &host.id, "cli", state, now).await;
            set_spec(pool, id, &limited).await;
        }
        set_times(pool, "running-late", Some(now - overdue), now - 5 * HOUR_MS).await;
        set_times(pool, "unstamped-late", None, now - overdue).await;

        mark_seen(pool, &host.id, now).await;
        janitor_once(ts.state(), now).await.unwrap();
        for id in ["running-late", "unstamped-late"] {
            let tag = format!("[{}] {id}", backend.name());
            assert_eq!(state_of(pool, id).await, "failed", "{tag}");
            assert_eq!(
                text(pool, "SELECT error_code FROM jobs WHERE id = $1", id)
                    .await
                    .as_deref(),
                Some("time_limit"),
                "{tag}"
            );
            let finished = sqlx::query("SELECT finished_at FROM jobs WHERE id = $1")
                .bind(id)
                .fetch_one(pool)
                .await
                .unwrap()
                .try_get::<i64, _>("finished_at")
                .unwrap();
            assert_eq!(finished, now, "{tag}");
        }
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_janitor_time_limit_margin() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let pool = &ts.state().db.pool;
        let now = now_ms();
        insert_job(pool, "inside", &host.id, "cli", "running", now).await;
        set_spec(
            pool,
            "inside",
            &policy_spec("  limits:\n    maxRuntime: 2h\n"),
        )
        .await;
        set_times(pool, "inside", Some(now - (3 * HOUR_MS - 1000)), now).await;
        mark_seen(pool, &host.id, now).await;

        janitor_once(ts.state(), now).await.unwrap();
        assert_eq!(
            state_of(pool, "inside").await,
            "running",
            "[{}]",
            backend.name()
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_janitor_time_limit_default() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let pool = &ts.state().db.pool;
        let now = now_ms();
        let limit = 13 * HOUR_MS;
        insert_job(pool, "zero", &host.id, "cli", "running", now).await;
        insert_job(pool, "garbage", &host.id, "cli", "running", now).await;
        insert_job(pool, "inside", &host.id, "cli", "running", now).await;
        sqlx::query("UPDATE jobs SET spec = $1 WHERE id = 'garbage'")
            .bind(vec![0xffu8, 0xff, 0xff])
            .execute(pool)
            .await
            .unwrap();
        set_times(pool, "zero", Some(now - limit - 1000), now).await;
        set_times(pool, "garbage", Some(now - limit - 1000), now).await;
        set_times(pool, "inside", Some(now - limit + 1000), now).await;
        mark_seen(pool, &host.id, now).await;

        janitor_once(ts.state(), now).await.unwrap();
        let tag = backend.name();
        assert_eq!(state_of(pool, "zero").await, "failed", "[{tag}]");
        assert_eq!(state_of(pool, "garbage").await, "failed", "[{tag}]");
        assert_eq!(state_of(pool, "inside").await, "running", "[{tag}]");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_janitor_skips_restore() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let pool = &ts.state().db.pool;
        let now = now_ms();
        insert_job(pool, "restore-old", &host.id, "manual", "running", now).await;
        sqlx::query("UPDATE jobs SET kind = 'restore' WHERE id = 'restore-old'")
            .execute(pool)
            .await
            .unwrap();
        set_times(
            pool,
            "restore-old",
            Some(now - 3 * DAY_MS),
            now - 3 * DAY_MS,
        )
        .await;
        mark_seen(pool, &host.id, now).await;

        janitor_once(ts.state(), now).await.unwrap();
        assert_eq!(
            state_of(pool, "restore-old").await,
            "running",
            "[{}]",
            backend.name()
        );
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_janitor_recovers_stuck_active_jobs() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let online = enroll(&ts, &api, &repo, "host-online").await;
        let away = enroll(&ts, &api, &repo, "host-away").await;
        let recent = enroll(&ts, &api, &repo, "host-recent").await;
        let pool = &ts.state().db.pool;
        let now = now_ms();

        let mut agent = Fake::connect(&ts, &online, vec![]).await;
        insert_job(
            pool,
            "dropped-assign",
            &online.id,
            "manual",
            "assigned",
            now - 3 * 60 * 1000,
        )
        .await;
        insert_job(
            pool,
            "lost-run",
            &away.id,
            "cli",
            "running",
            now - 2 * DAY_MS,
        )
        .await;
        insert_job(
            pool,
            "fresh-run",
            &recent.id,
            "cli",
            "running",
            now - 2 * DAY_MS,
        )
        .await;
        sqlx::query("UPDATE jobs SET started_at = $1 WHERE id = 'fresh-run'")
            .bind(now - HOUR_MS)
            .execute(pool)
            .await
            .unwrap();
        for (host, seen) in [(&away.id, now - 2 * DAY_MS), (&recent.id, now - HOUR_MS)] {
            sqlx::query("UPDATE agents SET last_seen_at = $1 WHERE id = $2")
                .bind(seen)
                .bind(host)
                .execute(pool)
                .await
                .unwrap();
        }

        janitor_once(ts.state(), now).await.unwrap();
        assert_eq!(
            agent.recv(is_assign).await,
            "dropped-assign",
            "[{}]",
            backend.name()
        );
        assert_eq!(state_of(pool, "lost-run").await, "interrupted");
        assert_eq!(state_of(pool, "fresh-run").await, "running");
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_revoke_ends_active_jobs() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let other = enroll(&ts, &api, &repo, "host-b").await;
        let pool = &ts.state().db.pool;
        let now = now_ms();
        for (id, host, state) in [
            ("q", &host.id, "queued"),
            ("a", &host.id, "assigned"),
            ("r", &host.id, "running"),
            ("other", &other.id, "running"),
        ] {
            insert_job(pool, id, host, "cli", state, now).await;
        }

        let response = api
            .post(
                &format!("/api/v1/hosts/{}/revoke", host.id),
                &serde_json::json!({}),
            )
            .await;
        assert_eq!(response.status(), 204, "[{}]", backend.name());
        assert_eq!(state_of(pool, "q").await, "cancelled");
        assert_eq!(state_of(pool, "a").await, "cancelled");
        assert_eq!(state_of(pool, "r").await, "interrupted");
        assert_eq!(state_of(pool, "other").await, "running");
        assert!(number(pool, "SELECT finished_at FROM jobs WHERE id = $1", "r").await > 0);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_reconcile_adopts_a_requeued_running_job() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let pool = &ts.state().db.pool;
        insert_job(pool, "job-1", &host.id, "manual", "queued", now_ms()).await;

        let mut agent =
            Fake::connect(&ts, &host, vec![report("job-1", JobReportState::Running)]).await;
        wait_state(pool, "job-1", "running").await;
        let sent = agent.drain(Duration::from_millis(1500)).await;
        assert!(
            sent.into_iter().filter_map(is_assign).next().is_none(),
            "[{}]",
            backend.name()
        );
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_reject_text_is_bounded() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let pool = &ts.state().db.pool;
        insert_job(pool, "job-1", &host.id, "manual", "queued", now_ms()).await;

        let mut agent = Fake::connect(&ts, &host, vec![]).await;
        assert_eq!(agent.recv(is_assign).await, "job-1");
        agent
            .send(AgentBody::JobRejected(JobRejected {
                job_id: "job-1".to_string(),
                code: "c".repeat(10_000),
                message: "m".repeat(10_000),
            }))
            .await;
        wait_state(pool, "job-1", "failed").await;
        let code = text(pool, "SELECT error_code FROM jobs WHERE id = $1", "job-1").await;
        let message = text(
            pool,
            "SELECT error_message FROM jobs WHERE id = $1",
            "job-1",
        )
        .await;
        assert_eq!(code.unwrap().len(), 4096, "[{}]", backend.name());
        assert_eq!(message.unwrap().len(), 4096);
        drop(agent);
        ts.stop().await;
    }
}

fn assigned_spec(body: ServerBody) -> Option<JobSpec> {
    match body {
        ServerBody::JobAssign(a) => a.spec,
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_limits_in_spec() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let yaml = format!(
            "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: capped\n\
             spec:\n  repository: main\n  selector:\n    hosts:\n      - {}\n  paths:\n    - /etc\n  \
             limits:\n    uploadRate: 2MiB/s\n    nice: 10\n    ioniceClass: best-effort\n    ioniceLevel: 5\n",
            host.id
        );
        let created = api
            .post("/api/v1/policies", &serde_json::json!({ "yaml": yaml }))
            .await;
        assert_eq!(created.status(), 201, "[{}]", backend.name());
        let run = api
            .post("/api/v1/policies/capped/run", &serde_json::json!({}))
            .await;
        assert_eq!(run.status(), 200, "[{}]", backend.name());

        let mut agent = Fake::connect(&ts, &host, vec![]).await;
        let spec = agent.recv(assigned_spec).await;
        let limits = spec.limits.expect("limits in the spec");
        assert_eq!(limits.upload_rate_bps, 2 << 20);
        assert_eq!(limits.nice, 10);
        assert_eq!(limits.ionice_class, IoniceClass::BestEffort as i32);
        assert_eq!(limits.ionice_level, 5);
        drop(agent);
        ts.stop().await;
    }
}

const LIMITS_WARNINGS: &str = "SELECT COUNT(*) FROM job_events e JOIN jobs j ON j.id = e.job_id \
     WHERE j.host_id = $1 AND e.kind = 'limits_ignored' AND e.seq = -1 AND e.level = 'warning'";
const LIMITS_LOG_LINES: &str = "SELECT log_lines FROM jobs WHERE host_id = $1";

async fn run_policy(api: &Api, host: &Host, limits: &str) {
    let yaml = format!(
        "apiVersion: lodestore.io/v1alpha1\nkind: BackupPolicy\nmetadata:\n  name: capped\n\
         spec:\n  repository: main\n  selector:\n    hosts:\n      - {}\n  paths:\n    - /etc\n{limits}",
        host.id
    );
    let created = api
        .post("/api/v1/policies", &serde_json::json!({ "yaml": yaml }))
        .await;
    assert_eq!(created.status(), 201);
    let run = api
        .post("/api/v1/policies/capped/run", &serde_json::json!({}))
        .await;
    assert_eq!(run.status(), 200);
}

const WITH_LIMITS: &str = "  limits:\n    nice: 10\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_limits_old_agent_warned() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        run_policy(&api, &host, WITH_LIMITS).await;

        let mut agent = Fake::connect(&ts, &host, vec![]).await;
        assert!(agent.recv(assigned_spec).await.limits.is_some());
        let pool = &ts.state().db.pool;
        assert_eq!(
            number(pool, LIMITS_WARNINGS, &host.id).await,
            1,
            "[{}]",
            backend.name()
        );
        assert_eq!(
            number(pool, LIMITS_LOG_LINES, &host.id).await,
            1,
            "[{}]",
            backend.name()
        );
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_limits_new_agent_silent() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        run_policy(&api, &host, WITH_LIMITS).await;

        let mut agent = Fake::connect_with(&ts, &host, &["limits_v1"]).await;
        assert!(agent.recv(assigned_spec).await.limits.is_some());
        let pool = &ts.state().db.pool;
        assert_eq!(
            number(pool, LIMITS_WARNINGS, &host.id).await,
            0,
            "[{}]",
            backend.name()
        );
        assert_eq!(
            number(pool, LIMITS_LOG_LINES, &host.id).await,
            0,
            "[{}]",
            backend.name()
        );
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_no_limits_old_agent_silent() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        run_policy(&api, &host, "").await;

        let mut agent = Fake::connect(&ts, &host, vec![]).await;
        assert!(agent.recv(assigned_spec).await.limits.is_none());
        let pool = &ts.state().db.pool;
        assert_eq!(
            number(pool, LIMITS_WARNINGS, &host.id).await,
            0,
            "[{}]",
            backend.name()
        );
        drop(agent);
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_foreign_finish_does_not_trigger_retention() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host_a = enroll(&ts, &api, &repo, "host-a").await;
        let host_b = enroll(&ts, &api, &repo, "host-b").await;
        let pool = &ts.state().db.pool;
        insert_job(pool, "job-b", &host_b.id, "manual", "succeeded", now_ms()).await;
        sqlx::query("UPDATE jobs SET snapshot_id = $1 WHERE id = 'job-b'")
            .bind("a".repeat(64))
            .execute(pool)
            .await
            .unwrap();

        let mut agent = Fake::connect(&ts, &host_a, vec![]).await;
        agent
            .send(AgentBody::JobFinished(finished(
                "job-b",
                JobOutcome::Succeeded,
                "",
            )))
            .await;
        assert_eq!(agent.recv(is_ack).await, "job-b");
        tokio::time::sleep(Duration::from_millis(500)).await;

        assert_eq!(
            text(
                pool,
                "SELECT retention_json FROM jobs WHERE id = $1",
                "job-b"
            )
            .await,
            None,
            "[{}]",
            backend.name()
        );
        drop(agent);
        ts.stop().await;
    }
}

const GC: &str = "gc_v1";

async fn gc_on(ts: &TestServer, repo: &str, enabled: bool) {
    let settings = GcSettings {
        enabled,
        ..GcSettings::default()
    };
    gc_db::save_settings(ts.state(), repo, &settings)
        .await
        .unwrap();
}

async fn gc_cycle_running(ts: &TestServer, repo: &str) {
    start_cycle(ts.state(), repo, false, GcOrigin::Manual, "admin")
        .await
        .unwrap();
}

async fn queue_backup(ts: &TestServer, job: &str, host: &Host) {
    insert_job(
        &ts.state().db.pool,
        job,
        &host.id,
        "manual",
        "queued",
        now_ms(),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_gc_blocks_old_agent() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let old = enroll(&ts, &api, &repo, "host-old").await;
        let new = enroll(&ts, &api, &repo, "host-new").await;
        gc_on(&ts, &repo, true).await;
        gc_cycle_running(&ts, &repo).await;
        queue_backup(&ts, "job-old", &old).await;
        queue_backup(&ts, "job-new", &new).await;
        let pool = &ts.state().db.pool;

        let _old = Fake::connect(&ts, &old, vec![]).await;
        wait_state(pool, "job-old", "failed").await;
        assert_eq!(
            text(pool, "SELECT error_code FROM jobs WHERE id = $1", "job-old")
                .await
                .as_deref(),
            Some("gc_agent_outdated"),
            "[{}]",
            backend.name()
        );
        assert_eq!(
            text(
                pool,
                "SELECT error_message FROM jobs WHERE id = $1",
                "job-old"
            )
            .await
            .as_deref(),
            Some("Update the agent on this host: a cleanup cycle is running in its repository.")
        );

        let mut new_agent = Fake::connect_with(&ts, &new, &[GC]).await;
        let spec = new_agent.recv(assigned_spec).await;
        assert!(spec.gc_locks_required, "[{}]", backend.name());
        assert_eq!(state_of(pool, "job-new").await, "assigned");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_gc_locks_required_flag() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let aware = enroll(&ts, &api, &repo, "host-aware").await;
        let old = enroll(&ts, &api, &repo, "host-old").await;
        let pool = &ts.state().db.pool;

        queue_backup(&ts, "job-off", &aware).await;
        let mut agent = Fake::connect_with(&ts, &aware, &[GC]).await;
        assert!(
            !agent.recv(assigned_spec).await.gc_locks_required,
            "[{}] cleanup is off",
            backend.name()
        );
        sqlx::query("UPDATE jobs SET state = 'succeeded', finished_at = $1 WHERE id = 'job-off'")
            .bind(now_ms())
            .execute(pool)
            .await
            .unwrap();

        gc_on(&ts, &repo, true).await;
        queue_backup(&ts, "job-on", &aware).await;
        assert!(
            agent.recv(assigned_spec).await.gc_locks_required,
            "[{}] cleanup is on",
            backend.name()
        );

        queue_backup(&ts, "job-old", &old).await;
        let mut old_agent = Fake::connect(&ts, &old, vec![]).await;
        let spec = old_agent.recv(assigned_spec).await;
        assert!(
            !spec.gc_locks_required,
            "[{}] an agent without gc_v1 is not told",
            backend.name()
        );
        assert_eq!(state_of(pool, "job-old").await, "assigned");
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_gc_restore_not_gated() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        gc_on(&ts, &repo, true).await;
        gc_cycle_running(&ts, &repo).await;
        let snapshot = write_snapshot(&ts, "host-a").await;
        ingest_snapshot(ts.state(), &repo, &snapshot, None)
            .await
            .unwrap();
        let job = enqueue_restore(
            ts.state(),
            RestoreRequest {
                snapshot_id: snapshot,
                items: vec![
                    ts.dir
                        .path()
                        .join("data-host-a/file.txt")
                        .canonicalize()
                        .unwrap()
                        .into_os_string()
                        .into_encoded_bytes(),
                ],
                target_host_id: host.id.clone(),
                destination: "/srv/out".to_string(),
                in_place: false,
                conflict: ConflictPolicy::OverwriteChanged,
                restore_owners: false,
                accept_unverified: true,
            },
            "admin",
        )
        .await
        .expect("queue a restore");

        let mut agent = Fake::connect_with(&ts, &host, &["restore_v1"]).await;
        assert_eq!(agent.recv(is_assign).await, job, "[{}]", backend.name());
        ts.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_token_restore_rechecks_roots_capability() {
    for backend in backends() {
        let ts = TestServer::start(&backend).await;
        let api = ts.api().await;
        let repo = ts.create_local_repository("main").await;
        let host = enroll(&ts, &api, &repo, "host-a").await;
        let snapshot = write_snapshot(&ts, "host-a").await;
        ingest_snapshot(ts.state(), &repo, &snapshot, None)
            .await
            .unwrap();
        let pool = &ts.state().db.pool;
        let request = |snapshot: &String| RestoreRequest {
            snapshot_id: snapshot.clone(),
            items: vec![
                ts.dir
                    .path()
                    .join("data-host-a/file.txt")
                    .canonicalize()
                    .unwrap()
                    .into_os_string()
                    .into_encoded_bytes(),
            ],
            target_host_id: host.id.clone(),
            destination: "/srv/out".to_string(),
            in_place: false,
            conflict: ConflictPolicy::OverwriteChanged,
            restore_owners: false,
            accept_unverified: true,
        };
        let job = enqueue_restore(ts.state(), request(&snapshot), "token:ci")
            .await
            .expect("queue a restore");

        let mut agent = Fake::connect_with(&ts, &host, &["restore_v1"]).await;
        wait_state(pool, &job, "failed").await;
        assert_eq!(
            text(pool, "SELECT error_code FROM jobs WHERE id = $1", &job)
                .await
                .as_deref(),
            Some("restore_agent_outdated"),
            "[{}]",
            backend.name()
        );
        assert!(
            agent
                .drain(Duration::from_millis(300))
                .await
                .into_iter()
                .all(|m| is_assign(m).is_none()),
            "[{}]",
            backend.name()
        );

        let session_job = enqueue_restore(ts.state(), request(&snapshot), "admin")
            .await
            .expect("queue a restore");
        assert_eq!(
            agent.recv(is_assign).await,
            session_job,
            "[{}]",
            backend.name()
        );
        ts.stop().await;
    }
}
