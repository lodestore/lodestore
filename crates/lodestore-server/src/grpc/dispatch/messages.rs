use std::collections::HashMap;

use lodestore_format::index::{hex32, parse_hex32};
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{
    JobCancel, JobCreated, JobEventLevel, JobFinished, JobFinishedAck, JobForget, JobOutcome,
    JobPhaseKind, JobReport, JobReportState, JobSync, ServerMessage,
};
use prost::Message;
use sqlx::Row;

use super::live::LiveProgress;
use crate::catalog::{self, JobLink};
use crate::db::now_ms;
use crate::maintenance;
use crate::state::AppState;

pub(crate) const LOG_LIMIT: i64 = 5000;
pub(crate) const LOG_LIMIT_MESSAGE: &str = "Log limit reached. Later events are not kept.";
const MAX_TEXT: usize = 4096;
const MAX_JOB_ID: usize = 64;
const MAX_REPORTS: usize = 256;
const INGEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

pub(crate) fn clip(text: &str) -> String {
    let mut end = text.len().min(MAX_TEXT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

fn non_empty(text: &str) -> Option<String> {
    (!text.is_empty()).then(|| clip(text))
}

fn valid_job_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_JOB_ID
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn clamp(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn send(state: &AppState, agent_id: &str, body: ServerBody) -> bool {
    state
        .registry
        .send(agent_id, ServerMessage { body: Some(body) })
}

fn outcome_state(outcome: i32) -> &'static str {
    match JobOutcome::try_from(outcome) {
        Ok(JobOutcome::Succeeded) => "succeeded",
        Ok(JobOutcome::Warnings) => "warnings",
        Ok(JobOutcome::Cancelled) => "cancelled",
        Ok(JobOutcome::Interrupted) => "interrupted",
        _ => "failed",
    }
}

fn level_name(level: i32) -> &'static str {
    match JobEventLevel::try_from(level) {
        Ok(JobEventLevel::Warning) => "warning",
        Ok(JobEventLevel::Error) => "error",
        _ => "info",
    }
}

fn phase_name(phase: i32) -> Option<&'static str> {
    match JobPhaseKind::try_from(phase) {
        Ok(JobPhaseKind::PreHook) => Some("pre_hook"),
        Ok(JobPhaseKind::Backup) => Some("backup"),
        Ok(JobPhaseKind::PostHook) => Some("post_hook"),
        Ok(JobPhaseKind::Snapshot) => Some("snapshot"),
        Ok(JobPhaseKind::RestorePlan) => Some("restore_plan"),
        Ok(JobPhaseKind::RestoreFiles) => Some("restore_files"),
        Ok(JobPhaseKind::RestoreMetadata) => Some("restore_metadata"),
        _ => None,
    }
}

struct OwnJob {
    state: String,
    log_seq: i64,
    log_lines: i64,
    cancel_requested: bool,
}

async fn own_job(state: &AppState, agent_id: &str, job_id: &str) -> anyhow::Result<Option<OwnJob>> {
    let row = sqlx::query(
        "SELECT state, log_seq, log_lines, cancel_requested FROM jobs WHERE id = $1 AND host_id = $2",
    )
    .bind(job_id)
    .bind(agent_id)
    .fetch_optional(&state.db.pool)
    .await?;
    row.map(|row| {
        Ok(OwnJob {
            state: row.try_get("state")?,
            log_seq: row.try_get("log_seq")?,
            log_lines: row.try_get("log_lines")?,
            cancel_requested: row.try_get::<i64, _>("cancel_requested")? != 0,
        })
    })
    .transpose()
}

async fn job_exists(state: &AppState, job_id: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query("SELECT 1 FROM jobs WHERE id = $1")
        .bind(job_id)
        .fetch_optional(&state.db.pool)
        .await?
        .is_some())
}

pub async fn session_started(state: &AppState, agent_id: &str, reports: &[JobReport]) {
    if let Err(error) = reconcile(state, agent_id, reports).await {
        tracing::warn!(%error, agent_id, "reconciling agent jobs failed");
    }
    state.registry.live.set_ready(agent_id, true);
    if let Err(error) = super::assign_queued(state, Some(agent_id)).await {
        tracing::warn!(%error, agent_id, "assigning jobs to a connected agent failed");
    }
}

async fn reconcile(state: &AppState, agent_id: &str, reports: &[JobReport]) -> anyhow::Result<()> {
    let mut reported: HashMap<&str, &JobReport> = reports
        .iter()
        .take(MAX_REPORTS)
        .map(|r| (r.job_id.as_str(), r))
        .collect();
    let active = sqlx::query(
        "SELECT id, state FROM jobs WHERE host_id = $1 AND state IN ('assigned', 'running')",
    )
    .bind(agent_id)
    .fetch_all(&state.db.pool)
    .await?;
    for row in active {
        let job_id: String = row.try_get("id")?;
        let db_state: String = row.try_get("state")?;
        match reported.remove(job_id.as_str()) {
            None if db_state == "assigned" => {
                super::requeue_assigned(state, &job_id, agent_id).await?;
            }
            None => interrupt(state, agent_id, &job_id).await?,
            Some(report) => adopt(state, agent_id, report).await?,
        }
    }
    for report in reported.into_values() {
        if let Some(own) = own_job(state, agent_id, &report.job_id).await? {
            if let Some(finished) = finished_report(report) {
                finish(state, agent_id, &report.job_id, finished).await;
            } else if own.state == "queued" {
                adopt(state, agent_id, report).await?;
            }
        } else if job_exists(state, &report.job_id).await? {
            continue;
        } else if let Some(created) = &report.created {
            insert_cli_job(state, agent_id, &report.job_id, created).await?;
            adopt(state, agent_id, report).await?;
        } else {
            send(
                state,
                agent_id,
                ServerBody::JobForget(JobForget {
                    job_id: report.job_id.clone(),
                }),
            );
        }
    }
    Ok(())
}

fn finished_report(report: &JobReport) -> Option<&JobFinished> {
    (report.state == JobReportState::Finished as i32)
        .then_some(report.finished.as_ref())
        .flatten()
}

async fn adopt(state: &AppState, agent_id: &str, report: &JobReport) -> anyhow::Result<()> {
    if let Some(finished) = finished_report(report) {
        finish(state, agent_id, &report.job_id, finished).await;
        return Ok(());
    }
    if report.state == JobReportState::Finished as i32 {
        return Ok(());
    }
    sqlx::query(
        "UPDATE jobs SET state = 'running', started_at = COALESCE(started_at, $1) \
         WHERE id = $2 AND host_id = $3 AND state IN ('queued', 'assigned')",
    )
    .bind(now_ms())
    .bind(&report.job_id)
    .bind(agent_id)
    .execute(&state.db.pool)
    .await?;
    let Some(job) = own_job(state, agent_id, &report.job_id).await? else {
        return Ok(());
    };
    send(
        state,
        agent_id,
        ServerBody::JobSync(JobSync {
            job_id: report.job_id.clone(),
            resend_events_from: u64::try_from(job.log_seq).unwrap_or(0) + 1,
        }),
    );
    if job.cancel_requested {
        state.registry.live.cancel_is_new(&report.job_id);
        send(
            state,
            agent_id,
            ServerBody::JobCancel(JobCancel {
                job_id: report.job_id.clone(),
            }),
        );
    }
    Ok(())
}

pub async fn interrupt(state: &AppState, agent_id: &str, job_id: &str) -> anyhow::Result<()> {
    let progress = state
        .registry
        .live
        .get(job_id)
        .map(|p| serde_json::to_string(&p))
        .transpose()?;
    sqlx::query(
        "UPDATE jobs SET state = 'interrupted', finished_at = $1, error_code = 'interrupted', \
         error_message = 'The agent lost track of the job.', phase = NULL, \
         progress_json = COALESCE($2, progress_json) \
         WHERE id = $3 AND host_id = $4 AND state = 'running'",
    )
    .bind(now_ms())
    .bind(progress)
    .bind(job_id)
    .bind(agent_id)
    .execute(&state.db.pool)
    .await?;
    state.registry.live.remove(job_id);
    state.registry.clear_issued(job_id);
    Ok(())
}

pub async fn on_message(state: &AppState, agent_id: &str, body: AgentBody) {
    if let Err(error) = handle(state, agent_id, body).await {
        tracing::warn!(%error, agent_id, "handling a job message failed");
    }
}

async fn handle(state: &AppState, agent_id: &str, body: AgentBody) -> anyhow::Result<()> {
    let pool = &state.db.pool;
    match body {
        AgentBody::JobAccepted(m) => {
            let at = if m.started_at_ms > 0 {
                m.started_at_ms
            } else {
                now_ms()
            };
            sqlx::query(
                "UPDATE jobs SET state = 'running', started_at = $1 \
                 WHERE id = $2 AND host_id = $3 AND state = 'assigned'",
            )
            .bind(at)
            .bind(&m.job_id)
            .bind(agent_id)
            .execute(pool)
            .await?;
        }
        AgentBody::JobRejected(m) => {
            tracing::warn!(agent_id, job_id = %m.job_id, code = %m.code, "the agent rejected a job");
            if m.code == "busy" {
                super::requeue_assigned(state, &m.job_id, agent_id).await?;
            } else {
                sqlx::query(
                    "UPDATE jobs SET state = 'failed', finished_at = $1, error_code = $2, \
                     error_message = $3 WHERE id = $4 AND host_id = $5 AND state = 'assigned'",
                )
                .bind(now_ms())
                .bind(non_empty(&m.code))
                .bind(non_empty(&m.message))
                .bind(&m.job_id)
                .bind(agent_id)
                .execute(pool)
                .await?;
            }
        }
        AgentBody::JobPhase(m) => {
            if let Some(phase) = phase_name(m.phase) {
                sqlx::query(
                    "UPDATE jobs SET phase = $1 \
                     WHERE id = $2 AND host_id = $3 AND state IN ('assigned', 'running')",
                )
                .bind(phase)
                .bind(&m.job_id)
                .bind(agent_id)
                .execute(pool)
                .await?;
            }
        }
        AgentBody::JobProgress(m) => {
            let active = sqlx::query(
                "SELECT 1 FROM jobs WHERE id = $1 AND host_id = $2 AND state IN ('assigned', 'running')",
            )
            .bind(&m.job_id)
            .bind(agent_id)
            .fetch_optional(pool)
            .await?;
            if active.is_some() {
                state.registry.live.set(
                    &m.job_id,
                    LiveProgress {
                        files_done: m.files_done,
                        files_total: m.files_total,
                        bytes_done: m.bytes_done,
                        bytes_total: m.bytes_total,
                        bytes_uploaded: m.bytes_uploaded,
                        bytes_downloaded: m.bytes_downloaded,
                        scan_done: m.scan_done,
                        speed_bps: m.speed_bps,
                        upload_speed_bps: m.upload_speed_bps,
                        eta_s: m.eta_s,
                        updated_at: now_ms(),
                    },
                );
            }
        }
        AgentBody::JobEvents(m) => store_events(state, agent_id, &m.job_id, &m.events).await?,
        AgentBody::JobFinished(m) => {
            let job_id = m.job_id.clone();
            finish(state, agent_id, &job_id, &m).await;
        }
        AgentBody::JobCreated(m) => {
            let job_id = m.job_id.clone();
            insert_cli_job(state, agent_id, &job_id, &m).await?;
        }
        AgentBody::UpdateStatus(m) => {
            crate::agent_update::rollout::on_status(state, agent_id, m).await?;
        }
        _ => {}
    }
    Ok(())
}

async fn store_events(
    state: &AppState,
    agent_id: &str,
    job_id: &str,
    events: &[lodestore_proto::agent::v1::JobEvent],
) -> anyhow::Result<()> {
    let Some(job) = own_job(state, agent_id, job_id).await? else {
        return Ok(());
    };
    if events.is_empty() {
        return Ok(());
    }
    let mut lines = job.log_lines;
    let mut log_seq = job.log_seq;
    let mut tx = state.db.pool.begin().await?;
    for event in events {
        log_seq = log_seq.max(clamp(event.seq));
        if lines >= LOG_LIMIT {
            continue;
        }
        let capped = lines == LOG_LIMIT - 1;
        let (level, kind, message, path, script) = if capped {
            (
                "warning",
                "log_limit".to_string(),
                LOG_LIMIT_MESSAGE.to_string(),
                None,
                None,
            )
        } else {
            (
                level_name(event.level),
                clip(&event.kind),
                clip(&event.message),
                non_empty(&event.path),
                non_empty(&event.script),
            )
        };
        let at = if event.at_ms > 0 {
            event.at_ms
        } else {
            now_ms()
        };
        let inserted = sqlx::query(
            "INSERT INTO job_events (job_id, seq, at, level, kind, path, script, message) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT DO NOTHING",
        )
        .bind(job_id)
        .bind(clamp(event.seq))
        .bind(at)
        .bind(level)
        .bind(kind)
        .bind(path)
        .bind(script)
        .bind(message)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        lines += i64::try_from(inserted).unwrap_or(0);
    }
    sqlx::query("UPDATE jobs SET log_seq = $1, log_lines = $2 WHERE id = $3 AND host_id = $4")
        .bind(log_seq)
        .bind(lines)
        .bind(job_id)
        .bind(agent_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn finish(state: &AppState, agent_id: &str, job_id: &str, finished: &JobFinished) {
    match apply_finished(state, agent_id, job_id, finished).await {
        Err(error) => {
            tracing::warn!(%error, agent_id, job_id, "recording a finished job failed");
            return;
        }
        Ok(true) => {
            tokio::spawn(maintenance::attest::verify::verify_then_retain(
                state.clone(),
                job_id.to_string(),
            ));
        }
        Ok(false) => {}
    }
    send(
        state,
        agent_id,
        ServerBody::JobFinishedAck(JobFinishedAck {
            job_id: job_id.to_string(),
        }),
    );
}

async fn ingest_before_finish(
    state: &AppState,
    agent_id: &str,
    job_id: &str,
    snapshot_id: &str,
) -> anyhow::Result<Option<String>> {
    let Some(id) = parse_hex32(snapshot_id) else {
        tracing::warn!(
            agent_id,
            job_id,
            "a finished job reported a malformed snapshot id"
        );
        return Ok(None);
    };
    let snapshot_id = hex32(&id);
    let active = sqlx::query(
        "SELECT a.repository_id FROM jobs j JOIN agents a ON a.id = j.host_id \
         WHERE j.id = $1 AND j.host_id = $2 AND j.state IN ('queued', 'assigned', 'running')",
    )
    .bind(job_id)
    .bind(agent_id)
    .fetch_optional(&state.db.pool)
    .await?;
    let Some(row) = active else {
        return Ok(Some(snapshot_id));
    };
    let repository: String = row.try_get("repository_id")?;
    let link = JobLink {
        job_id,
        host_id: agent_id,
    };
    let ingested = tokio::time::timeout(
        INGEST_TIMEOUT,
        catalog::ingest_snapshot(state, &repository, &snapshot_id, Some(link)),
    )
    .await
    .unwrap_or_else(|_| Err(anyhow::anyhow!("reading the snapshot timed out")));
    match ingested {
        Err(error) if error.is::<catalog::LinkRefused>() => {
            tracing::warn!(
                job_id,
                snapshot_id,
                "a finished job reported a snapshot it does not own"
            );
            Ok(None)
        }
        Err(error) => {
            tracing::warn!(%error, job_id, snapshot_id, "could not read the snapshot of a finished job");
            Ok(Some(snapshot_id))
        }
        Ok(_) => Ok(Some(snapshot_id)),
    }
}

async fn apply_finished(
    state: &AppState,
    agent_id: &str,
    job_id: &str,
    f: &JobFinished,
) -> anyhow::Result<bool> {
    let is_restore = sqlx::query("SELECT kind FROM jobs WHERE id = $1 AND host_id = $2")
        .bind(job_id)
        .bind(agent_id)
        .fetch_optional(&state.db.pool)
        .await?
        .map(|row| row.try_get::<String, _>("kind"))
        .transpose()?
        .is_some_and(|kind| kind == "restore");
    let stats = if is_restore {
        f.restore.as_ref().map(|r| {
            serde_json::json!({
                "created": r.created,
                "overwritten": r.overwritten,
                "skipped": r.skipped,
                "files": r.files,
                "bytes": r.bytes,
            })
            .to_string()
        })
    } else {
        f.stats.as_ref().map(|s| {
            serde_json::json!({
                "files_new": s.files_new,
                "files_changed": s.files_changed,
                "files_unchanged": s.files_unchanged,
                "dirs": s.dirs,
                "bytes_total": s.bytes_total,
                "bytes_read": s.bytes_read,
                "bytes_added": s.bytes_added,
            })
            .to_string()
        })
    };
    let detail = (!f.error_detail.is_empty())
        .then(|| serde_json::to_string(&f.error_detail))
        .transpose()?;
    let progress = state
        .registry
        .live
        .get(job_id)
        .map(|p| serde_json::to_string(&p))
        .transpose()?;
    let reported = non_empty(&f.snapshot_id).filter(|_| !is_restore);
    let snapshot_id = match reported {
        Some(reported) => ingest_before_finish(state, agent_id, job_id, &reported).await?,
        None => None,
    };
    let outcome = outcome_state(f.outcome);
    let verify_state =
        (snapshot_id.is_some() && matches!(outcome, "succeeded" | "warnings")).then_some("pending");
    let done = sqlx::query(
        "UPDATE jobs SET state = $1, snapshot_id = $2, parent_id = $3, stats_json = $4, \
         incomplete = $5, errors_total = $6, warnings_total = $7, error_code = $8, \
         error_message = $9, error_detail_json = $10, finished_at = $11, phase = NULL, \
         progress_json = COALESCE($12, progress_json), verify_state = $15 \
         WHERE id = $13 AND host_id = $14 AND state IN ('queued', 'assigned', 'running')",
    )
    .bind(outcome)
    .bind(&snapshot_id)
    .bind(non_empty(&f.parent_id))
    .bind(stats)
    .bind(i64::from(f.incomplete))
    .bind(clamp(f.errors_total))
    .bind(clamp(f.warnings_total))
    .bind(non_empty(&f.error_code))
    .bind(non_empty(&f.error_message))
    .bind(detail)
    .bind(if f.finished_at_ms > 0 {
        f.finished_at_ms
    } else {
        now_ms()
    })
    .bind(progress)
    .bind(job_id)
    .bind(agent_id)
    .bind(verify_state)
    .execute(&state.db.pool)
    .await?;
    if done.rows_affected() != 1 {
        let own_final = sqlx::query("SELECT 1 FROM jobs WHERE id = $1 AND host_id = $2")
            .bind(job_id)
            .bind(agent_id)
            .fetch_optional(&state.db.pool)
            .await?
            .is_some();
        if own_final {
            state.registry.clear_issued(job_id);
        }
        return Ok(own_final);
    }
    state.registry.live.remove(job_id);
    state.registry.clear_issued(job_id);
    Ok(true)
}

async fn insert_cli_job(
    state: &AppState,
    agent_id: &str,
    job_id: &str,
    created: &JobCreated,
) -> anyhow::Result<()> {
    if !valid_job_id(job_id) {
        tracing::warn!(agent_id, "an agent reported a job with a malformed id");
        return Ok(());
    }
    let spec = created.spec.clone().unwrap_or_default();
    let policy_id = if spec.policy_name.is_empty() {
        None
    } else {
        sqlx::query(
            "SELECT p.id FROM policies p JOIN agents a ON a.repository_id = p.repository_id \
             WHERE p.name = $1 AND a.id = $2",
        )
        .bind(&spec.policy_name)
        .bind(agent_id)
        .fetch_optional(&state.db.pool)
        .await?
        .map(|row| row.try_get::<String, _>("id"))
        .transpose()?
    };
    let at = if created.created_at_ms > 0 {
        created.created_at_ms
    } else {
        now_ms()
    };
    for policy in [policy_id, None] {
        let inserted = sqlx::query(
            "INSERT INTO jobs (id, host_id, policy_id, policy_name, stdin_name, source, state, \
             spec, queued_at, assigned_at, started_at, caller_uid, caller_gid) \
             VALUES ($1, $2, $3, $4, $5, 'cli', 'running', $6, $7, $7, $7, $8, $9) \
             ON CONFLICT DO NOTHING",
        )
        .bind(job_id)
        .bind(agent_id)
        .bind(&policy)
        .bind(non_empty(&spec.policy_name))
        .bind(non_empty(&spec.stdin_name))
        .bind(spec.encode_to_vec())
        .bind(at)
        .bind(i64::from(created.caller_uid))
        .bind(i64::from(created.caller_gid))
        .execute(&state.db.pool)
        .await?
        .rows_affected();
        if inserted == 1 || job_exists(state, job_id).await? || policy.is_none() {
            break;
        }
    }
    Ok(())
}
