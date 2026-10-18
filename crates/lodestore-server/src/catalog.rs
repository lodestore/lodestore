use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::Context;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use lodestore_format::index::{hex32, parse_hex32};
use lodestore_format::keys::{MasterKey, ReadKeys};
use lodestore_format::pb;
use lodestore_format::snapshot::{open_snapshot, snapshot_object_key};
use lodestore_format::store::{BlobDelete, BlobError, BlobStore, Store};
use lodestore_format::{Error as FormatError, Result as FormatResult};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::AuthUser;
use crate::auth::tokens::{Scoped, SnapshotsRead};
use crate::db::now_ms;
use crate::error::{ApiError, ErrorBody};
use crate::grpc::dispatch::messages::clip;
use crate::repos::checks::open_store;
use crate::repos::db::{get_repository, list_repositories, server_access};
use crate::state::AppState;

pub mod actions;
pub mod anomaly;
pub mod retention;

use self::actions::{SuspicionDto, suspicion_from_row};

const SYNC_INTERVAL: Duration = Duration::from_secs(300);
const PAGE_SIZE: i64 = 50;
pub(crate) const SNAPSHOT_PREFIX: &str = "snapshots/";
const VANISH_GRACE_MS: i64 = 3_600_000;
const TOMBSTONE_RETRY_MS: i64 = 86_400_000;
const VANISH_GUARD_MIN: usize = 10;
const CONFIG_OBJECT: &str = "config";
const READ_CONCURRENCY: usize = 8;
pub(crate) const SNAPSHOT_COLUMNS: &str = "id, time_ms, hostname, paths_json, tags_json, \
     policy_name, stdin_name, job_id, parent_id, incomplete, errors_total, warnings_total, \
     stats_json, received_at, suspicious, suspicious_by, suspicious_reason, suspicious_at, \
     anomaly_json, CAST(CASE WHEN policy_name IS NOT NULL AND NOT EXISTS \
     (SELECT 1 FROM policies p WHERE p.name = snapshots.policy_name) THEN 1 ELSE 0 END AS BIGINT) \
     AS policy_deleted";

pub struct JobLink<'a> {
    pub job_id: &'a str,
    pub host_id: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncReport {
    pub added: u64,
    pub skipped: u64,
    pub total: u64,
}

pub struct OpenRepository {
    pub store: Store,
    pub keys: ReadKeys,
    pub config: pb::Config,
}

pub async fn open_repository(
    state: &AppState,
    repository_id: &str,
) -> anyhow::Result<OpenRepository> {
    let access = server_access(state, repository_id).await?;
    let master: [u8; 32] = access
        .master
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("master secret must be 32 bytes"))?;
    let store = open_store(&access.location, access.server_creds.as_ref())?;
    let config = lodestore_format::config::decode(&store.get(CONFIG_OBJECT).await?)?;
    Ok(OpenRepository {
        store,
        keys: ReadKeys::derive(MasterKey::from_bytes(master), access.format_repo_id),
        config,
    })
}

async fn read_snapshot(source: &OpenRepository, id: &[u8; 32]) -> FormatResult<pb::Snapshot> {
    let bytes = source.store.get(&snapshot_object_key(id)).await?;
    open_snapshot(
        source.keys.hpke_private_key(),
        source.keys.repo_id(),
        id,
        &bytes,
    )
}

fn is_unreadable(error: &FormatError) -> bool {
    matches!(
        error,
        FormatError::Corrupt { .. } | FormatError::UnsupportedVersion { .. }
    )
}

fn clamp(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn tag_value<'a>(tags: &'a [String], key: &str) -> Option<&'a str> {
    tags.iter()
        .find_map(|t| t.strip_prefix(key)?.strip_prefix('='))
}

async fn bind_host(
    state: &AppState,
    repository_id: &str,
    snapshot: &pb::Snapshot,
    claim: Option<&str>,
) -> anyhow::Result<Option<String>> {
    if let Some(claim) = claim {
        let row = sqlx::query("SELECT id FROM agents WHERE id = $1 AND repository_id = $2")
            .bind(claim)
            .bind(repository_id)
            .fetch_optional(&state.db.pool)
            .await?;
        if let Some(row) = row {
            return Ok(Some(row.try_get("id")?));
        }
    }
    let rows = sqlx::query(
        "SELECT id FROM agents WHERE repository_id = $1 AND hostname = $2 AND status <> 'revoked' \
         LIMIT 2",
    )
    .bind(repository_id)
    .bind(text(&snapshot.hostname))
    .fetch_all(&state.db.pool)
    .await?;
    match rows.as_slice() {
        [only] => Ok(Some(only.try_get("id")?)),
        _ => Ok(None),
    }
}

async fn insert_snapshot(
    state: &AppState,
    repository_id: &str,
    id: &[u8; 32],
    snapshot: &pb::Snapshot,
    link: Option<&JobLink<'_>>,
    received: Option<i64>,
) -> anyhow::Result<bool> {
    let discovered = now_ms();
    let claim = std::str::from_utf8(&snapshot.agent_id)
        .ok()
        .filter(|c| !c.is_empty());
    let host_id = match link {
        Some(link) => Some(link.host_id.to_string()),
        None => bind_host(state, repository_id, snapshot, claim).await?,
    };
    let paths: Vec<String> = snapshot.paths.iter().map(|p| text(p)).collect();
    let time = snapshot.time.unwrap_or_default();
    let stats = SnapshotStatsDto::from(&snapshot.stats.unwrap_or_default());
    let parent = <[u8; 32]>::try_from(snapshot.parent.as_slice())
        .ok()
        .map(|p| hex32(&p));
    let result = sqlx::query(
        "INSERT INTO snapshots (id, repository_id, host_id, hostname, time_ms, paths_json, \
         tags_json, policy_name, stdin_name, job_id, parent_id, incomplete, errors_total, \
         warnings_total, stats_json, agent_id_claim, discovered_at, received_at, received_exact) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, \
         $18, $19) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(hex32(id))
    .bind(repository_id)
    .bind(&host_id)
    .bind(text(&snapshot.hostname))
    .bind(
        time.seconds
            .saturating_mul(1000)
            .saturating_add(i64::from(time.nanos) / 1_000_000),
    )
    .bind(serde_json::to_string(&paths)?)
    .bind(serde_json::to_string(&snapshot.tags)?)
    .bind(tag_value(&snapshot.tags, "policy"))
    .bind(tag_value(&snapshot.tags, "stdin"))
    .bind(link.map(|l| l.job_id))
    .bind(parent)
    .bind(i64::from(snapshot.incomplete))
    .bind(clamp(snapshot.errors_total))
    .bind(clamp(snapshot.warnings_total))
    .bind(serde_json::to_string(&stats)?)
    .bind(claim)
    .bind(discovered)
    .bind(received.unwrap_or(discovered))
    .bind(i64::from(received.is_some() || link.is_some()))
    .execute(&state.db.pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn ingest_snapshot(
    state: &AppState,
    repository_id: &str,
    snapshot_id: &str,
    link: Option<JobLink<'_>>,
) -> anyhow::Result<bool> {
    let id = parse_hex32(snapshot_id).context("snapshot id must be 64 hex characters")?;
    let known = sqlx::query("SELECT 1 FROM snapshots WHERE id = $1 AND repository_id = $2")
        .bind(hex32(&id))
        .bind(repository_id)
        .fetch_optional(&state.db.pool)
        .await?
        .is_some();
    if known {
        if let Some(link) = &link {
            link_snapshot(state, repository_id, &id, link).await?;
        }
        return Ok(false);
    }
    let source = open_repository(state, repository_id).await?;
    let snapshot = read_snapshot(&source, &id).await?;
    if let Some(link) = &link
        && std::str::from_utf8(&snapshot.agent_id)
            .is_ok_and(|claim| !claim.is_empty() && claim != link.host_id)
    {
        return Err(LinkRefused.into());
    }
    let received = source
        .store
        .modified_ms(&snapshot_object_key(&id))
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "reading the snapshot object time failed");
            None
        });
    let inserted = insert_snapshot(
        state,
        repository_id,
        &id,
        &snapshot,
        link.as_ref(),
        received,
    )
    .await?;
    if !inserted && let Some(link) = &link {
        link_snapshot(state, repository_id, &id, link).await?;
    }
    Ok(inserted)
}

#[derive(Debug, thiserror::Error)]
#[error("the snapshot belongs to another host or job")]
pub struct LinkRefused;

async fn link_snapshot(
    state: &AppState,
    repository_id: &str,
    id: &[u8; 32],
    link: &JobLink<'_>,
) -> anyhow::Result<()> {
    let done = sqlx::query(
        "UPDATE snapshots SET job_id = $1, host_id = $2 WHERE id = $3 AND repository_id = $4 \
         AND (job_id IS NULL OR job_id = $1) AND (host_id IS NULL OR host_id = $2)",
    )
    .bind(link.job_id)
    .bind(link.host_id)
    .bind(hex32(id))
    .bind(repository_id)
    .execute(&state.db.pool)
    .await?;
    if done.rows_affected() == 0 {
        return Err(LinkRefused.into());
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CatalogPhase {
    Idle,
    Listing,
    Reading,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SkippedSnapshot {
    pub key: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CatalogStatus {
    pub repository_id: String,
    pub repository_name: String,
    pub phase: CatalogPhase,
    pub read: u64,
    pub total: Option<u64>,
    pub added: u64,
    pub skipped: Vec<SkippedSnapshot>,
    pub error: Option<String>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub last_sync_at: Option<i64>,
    pub snapshots: u64,
}

impl CatalogStatus {
    fn new(repository_id: &str, phase: CatalogPhase) -> Self {
        CatalogStatus {
            repository_id: repository_id.to_string(),
            repository_name: String::new(),
            phase,
            read: 0,
            total: None,
            added: 0,
            skipped: Vec::new(),
            error: None,
            started_at: None,
            finished_at: None,
            last_sync_at: None,
            snapshots: 0,
        }
    }

    fn running(&self) -> bool {
        matches!(self.phase, CatalogPhase::Listing | CatalogPhase::Reading)
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum RebuildError {
    #[error("{0}")]
    Storage(String),
    #[error("{0}")]
    Other(String),
}

impl RebuildError {
    fn of(error: &anyhow::Error) -> Self {
        let storage = error.downcast_ref::<FormatError>().is_some()
            || error.downcast_ref::<BlobError>().is_some();
        if storage {
            RebuildError::Storage(error.to_string())
        } else {
            RebuildError::Other(error.to_string())
        }
    }
}

type Outcome = Result<SyncReport, RebuildError>;

struct Job {
    status: CatalogStatus,
    outcome: watch::Receiver<Option<Outcome>>,
    /// A routine sync: the API keeps showing `shown`, the last rebuild an
    /// operator saw, instead of this run.
    quiet: bool,
    shown: Option<CatalogStatus>,
}

type PairLocks = HashMap<(String, String), Arc<tokio::sync::Mutex<()>>>;

#[derive(Clone, Default)]
pub struct CatalogJobs {
    inner: Arc<Mutex<HashMap<String, Job>>>,
    pairs: Arc<Mutex<PairLocks>>,
}

impl CatalogJobs {
    pub(crate) fn pair_lock(&self, policy: &str, host_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.pairs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry((policy.to_string(), host_id.to_string()))
            .or_default()
            .clone()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Job>> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn update(&self, repository_id: &str, change: impl FnOnce(&mut CatalogStatus)) {
        if let Some(job) = self.lock().get_mut(repository_id) {
            change(&mut job.status);
        }
    }

    fn status(&self, repository_id: &str) -> Option<CatalogStatus> {
        self.lock().get(repository_id).map(|j| j.status.clone())
    }

    /// What the API reports: a routine sync stays out of sight.
    fn shown(&self, repository_id: &str) -> Option<CatalogStatus> {
        self.lock().get(repository_id).and_then(|j| {
            if j.quiet {
                j.shown.clone()
            } else {
                Some(j.status.clone())
            }
        })
    }

    /// Starts a run unless one is going on. A rebuild asked for while a quiet
    /// sync runs joins it and makes it visible.
    fn start(
        &self,
        state: &AppState,
        repository_id: &str,
        quiet: bool,
    ) -> (CatalogStatus, watch::Receiver<Option<Outcome>>) {
        let mut jobs = self.lock();
        let mut shown = None;
        if let Some(job) = jobs.get_mut(repository_id) {
            if job.status.running() {
                job.quiet &= quiet;
                return (job.status.clone(), job.outcome.clone());
            }
            shown = if job.quiet {
                job.shown.take()
            } else {
                Some(job.status.clone())
            };
        }
        let (tx, rx) = watch::channel(None);
        let mut status = CatalogStatus::new(repository_id, CatalogPhase::Listing);
        status.started_at = Some(now_ms());
        jobs.insert(
            repository_id.to_string(),
            Job {
                status: status.clone(),
                outcome: rx.clone(),
                quiet,
                shown,
            },
        );
        drop(jobs);
        spawn_job(state.clone(), repository_id.to_string(), tx);
        (status, rx)
    }

    fn finish(&self, repository_id: &str, outcome: &Outcome) {
        self.update(repository_id, |status| {
            status.finished_at = Some(now_ms());
            match outcome {
                Ok(_) => status.phase = CatalogPhase::Done,
                Err(e) => {
                    status.phase = CatalogPhase::Failed;
                    status.error = Some(e.to_string());
                }
            }
        });
    }
}

fn spawn_job(state: AppState, repository_id: String, done: watch::Sender<Option<Outcome>>) {
    tokio::spawn(async move {
        let worker = tokio::spawn({
            let state = state.clone();
            let repository_id = repository_id.clone();
            async move { rebuild(&state, &repository_id).await }
        });
        let outcome = match worker.await {
            Ok(Ok(report)) => Ok(report),
            Ok(Err(error)) => {
                if let Err(e) =
                    record_state(&state, &repository_id, Some(&error.to_string()), &[]).await
                {
                    tracing::warn!(error = %e, "recording the catalog rebuild error failed");
                }
                Err(RebuildError::of(&error))
            }
            Err(e) => Err(RebuildError::Other(format!(
                "the catalog rebuild stopped unexpectedly: {e}"
            ))),
        };
        state.catalog.finish(&repository_id, &outcome);
        done.send_replace(Some(outcome));
    });
}

pub fn start_rebuild(state: &AppState, repository_id: &str) -> CatalogStatus {
    state.catalog.start(state, repository_id, false).0
}

pub async fn resume_unsynced(state: &AppState) {
    let rows = sqlx::query(
        "SELECT r.id FROM repositories r LEFT JOIN catalog_state c ON c.repository_id = r.id \
         WHERE c.last_sync_at IS NULL",
    )
    .fetch_all(&state.db.pool)
    .await;
    match rows {
        Ok(rows) => {
            for row in rows {
                if let Ok(id) = row.try_get::<String, _>("id") {
                    start_rebuild(state, &id);
                }
            }
        }
        Err(error) => tracing::warn!(%error, "listing unsynced repositories failed"),
    }
}

async fn read_one(
    state: &AppState,
    repository_id: &str,
    source: &OpenRepository,
    key: String,
    id: [u8; 32],
    received: Option<i64>,
) -> anyhow::Result<()> {
    match read_snapshot(source, &id).await {
        Ok(snapshot) => {
            let inserted =
                insert_snapshot(state, repository_id, &id, &snapshot, None, received).await?;
            state.catalog.update(repository_id, |s| {
                s.read += 1;
                s.added += u64::from(inserted);
            });
        }
        Err(e) => {
            if !is_unreadable(&e) {
                tracing::warn!(%key, error = %e, "reading a snapshot failed");
            }
            state.catalog.update(repository_id, |s| {
                s.read += 1;
                s.skipped.push(SkippedSnapshot {
                    key,
                    reason: e.to_string(),
                });
            });
        }
    }
    Ok(())
}

struct KnownRow {
    received_at: i64,
    exact: bool,
    suspicious: bool,
}

async fn known_rows(
    state: &AppState,
    repository_id: &str,
) -> anyhow::Result<HashMap<String, KnownRow>> {
    sqlx::query(
        "SELECT id, received_at, received_exact, suspicious FROM snapshots \
         WHERE repository_id = $1",
    )
    .bind(repository_id)
    .fetch_all(&state.db.pool)
    .await?
    .iter()
    .map(|r| {
        Ok((
            r.try_get("id")?,
            KnownRow {
                received_at: r.try_get("received_at")?,
                exact: r.try_get::<i64, _>("received_exact")? != 0,
                suspicious: r.try_get::<i64, _>("suspicious")? != 0,
            },
        ))
    })
    .collect()
}

async fn tombstone_ids(state: &AppState, repository_id: &str) -> anyhow::Result<HashSet<String>> {
    Ok(
        sqlx::query("SELECT id FROM snapshot_tombstones WHERE repository_id = $1")
            .bind(repository_id)
            .fetch_all(&state.db.pool)
            .await?
            .iter()
            .map(|r| r.try_get("id"))
            .collect::<Result<_, _>>()?,
    )
}

async fn store_exact_times(
    state: &AppState,
    repository_id: &str,
    times: &[(String, i64)],
) -> anyhow::Result<()> {
    for (id, ms) in times {
        sqlx::query(
            "UPDATE snapshots SET received_at = $1, received_exact = 1 \
             WHERE id = $2 AND repository_id = $3 AND received_exact = 0",
        )
        .bind(ms)
        .bind(id)
        .bind(repository_id)
        .execute(&state.db.pool)
        .await?;
    }
    Ok(())
}

async fn drop_stale_tombstones(
    state: &AppState,
    repository_id: &str,
    tombstones: &HashSet<String>,
    listed: &HashSet<String>,
) -> anyhow::Result<()> {
    for id in tombstones.difference(listed) {
        sqlx::query("DELETE FROM snapshot_tombstones WHERE id = $1 AND repository_id = $2")
            .bind(id)
            .bind(repository_id)
            .execute(&state.db.pool)
            .await?;
    }
    Ok(())
}

async fn retry_tombstones(
    state: &AppState,
    repository_id: &str,
    source: &OpenRepository,
    listed: &HashSet<String>,
) -> anyhow::Result<()> {
    let cutoff = now_ms().saturating_sub(TOMBSTONE_RETRY_MS);
    let due = sqlx::query(
        "SELECT id FROM snapshot_tombstones WHERE repository_id = $1 AND forgotten_at < $2",
    )
    .bind(repository_id)
    .bind(cutoff)
    .fetch_all(&state.db.pool)
    .await?;
    for row in due {
        let hex: String = row.try_get("id")?;
        let Some(id) = parse_hex32(&hex).filter(|_| listed.contains(&hex)) else {
            continue;
        };
        match source.store.delete(&snapshot_object_key(&id)).await {
            Ok(()) | Err(BlobError::NotFound) => {
                sqlx::query("DELETE FROM snapshot_tombstones WHERE id = $1 AND repository_id = $2")
                    .bind(&hex)
                    .bind(repository_id)
                    .execute(&state.db.pool)
                    .await?;
            }
            Err(BlobError::Denied { message }) => {
                sqlx::query(
                    "UPDATE snapshot_tombstones SET forgotten_at = $1, reason = $2 \
                     WHERE id = $3 AND repository_id = $4",
                )
                .bind(now_ms())
                .bind(clip(&message))
                .bind(&hex)
                .bind(repository_id)
                .execute(&state.db.pool)
                .await?;
            }
            Err(error) => {
                tracing::warn!(snapshot = %hex, %error, "retrying a denied snapshot delete failed");
            }
        }
    }
    Ok(())
}

async fn remove_vanished(
    state: &AppState,
    repository_id: &str,
    known: &HashMap<String, KnownRow>,
    listed: &HashSet<String>,
) -> anyhow::Result<Option<String>> {
    let cutoff = now_ms().saturating_sub(VANISH_GRACE_MS);
    let gone: Vec<&String> = known
        .iter()
        .filter(|(id, row)| !listed.contains(*id) && row.received_at < cutoff && !row.suspicious)
        .map(|(id, _)| id)
        .collect();
    if listed.is_empty() && !known.is_empty() {
        return Ok(Some(
            "The bucket listing is empty. Nothing was removed from the catalog.".into(),
        ));
    }
    if gone.len() * 2 > known.len() && known.len() > VANISH_GUARD_MIN {
        return Ok(Some(format!(
            "The bucket listing is missing {} of {} snapshots. Nothing was removed from the catalog.",
            gone.len(),
            known.len()
        )));
    }
    for id in gone {
        sqlx::query("DELETE FROM snapshots WHERE id = $1 AND repository_id = $2")
            .bind(id)
            .bind(repository_id)
            .execute(&state.db.pool)
            .await?;
    }
    Ok(None)
}

async fn rebuild(state: &AppState, repository_id: &str) -> anyhow::Result<SyncReport> {
    let source = Arc::new(open_repository(state, repository_id).await?);
    let listed = source.store.list(SNAPSHOT_PREFIX).await?;
    let known = known_rows(state, repository_id).await?;
    let tombstones = tombstone_ids(state, repository_id).await?;
    let mut listed_ids = HashSet::new();
    let mut unreadable = Vec::new();
    let mut candidates = Vec::new();
    let mut exact_times = Vec::new();
    for entry in listed {
        match entry
            .key
            .strip_prefix(SNAPSHOT_PREFIX)
            .and_then(parse_hex32)
        {
            None => unreadable.push(SkippedSnapshot {
                key: entry.key,
                reason: "not a snapshot name".into(),
            }),
            Some(id) => {
                let hex = hex32(&id);
                listed_ids.insert(hex.clone());
                match known.get(&hex) {
                    _ if tombstones.contains(&hex) => {}
                    Some(row) => {
                        if !row.exact
                            && let Some(ms) = entry.modified_ms
                        {
                            exact_times.push((hex, ms));
                        }
                    }
                    None => candidates.push((entry.key, id, entry.modified_ms)),
                }
            }
        }
    }
    store_exact_times(state, repository_id, &exact_times).await?;
    drop_stale_tombstones(state, repository_id, &tombstones, &listed_ids).await?;
    retry_tombstones(state, repository_id, &source, &listed_ids).await?;
    let vanish_note = remove_vanished(state, repository_id, &known, &listed_ids).await?;
    state.catalog.update(repository_id, |s| {
        s.phase = CatalogPhase::Reading;
        s.total = Some(candidates.len() as u64);
        s.skipped = unreadable;
    });

    let permits = Arc::new(Semaphore::new(READ_CONCURRENCY));
    let mut tasks = JoinSet::new();
    for (key, id, received) in candidates {
        let permit = permits.clone().acquire_owned().await?;
        while let Some(done) = tasks.try_join_next() {
            done??;
        }
        let (state, source, repository_id) =
            (state.clone(), source.clone(), repository_id.to_string());
        tasks.spawn(async move {
            let _permit = permit;
            read_one(&state, &repository_id, &source, key, id, received).await
        });
    }
    while let Some(done) = tasks.join_next().await {
        done??;
    }

    state.catalog.update(repository_id, |s| {
        s.skipped.sort_by(|a, b| a.key.cmp(&b.key));
    });
    let status = state
        .catalog
        .status(repository_id)
        .context("the rebuild state vanished")?;
    record_state(state, repository_id, None, &status.skipped).await?;
    if let Some(note) = &vanish_note {
        sqlx::query("UPDATE catalog_state SET last_error = $1 WHERE repository_id = $2")
            .bind(note)
            .bind(repository_id)
            .execute(&state.db.pool)
            .await?;
    }
    let total: i64 = sqlx::query("SELECT COUNT(*) AS n FROM snapshots WHERE repository_id = $1")
        .bind(repository_id)
        .fetch_one(&state.db.pool)
        .await?
        .try_get("n")?;
    Ok(SyncReport {
        added: status.added,
        skipped: status.skipped.len() as u64,
        total: total.max(0) as u64,
    })
}

async fn record_state(
    state: &AppState,
    repository_id: &str,
    error: Option<&str>,
    skipped: &[SkippedSnapshot],
) -> anyhow::Result<()> {
    let at = error.is_none().then(now_ms);
    let skipped_json = error
        .is_none()
        .then(|| serde_json::to_string(skipped))
        .transpose()?;
    sqlx::query(
        "INSERT INTO catalog_state (repository_id, last_sync_at, last_error, skipped_json) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (repository_id) DO UPDATE SET \
         last_sync_at = COALESCE(excluded.last_sync_at, catalog_state.last_sync_at), \
         last_error = excluded.last_error, \
         skipped_json = COALESCE(excluded.skipped_json, catalog_state.skipped_json)",
    )
    .bind(repository_id)
    .bind(at)
    .bind(error)
    .bind(skipped_json)
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

pub async fn sync_repository(state: &AppState, repository_id: &str) -> anyhow::Result<SyncReport> {
    let (_, mut done) = state.catalog.start(state, repository_id, true);
    let outcome = done
        .wait_for(Option::is_some)
        .await
        .context("the catalog rebuild was dropped")?
        .clone()
        .context("the catalog rebuild has no outcome")?;
    Ok(outcome?)
}

pub async fn sync_loop(state: AppState, stop: CancellationToken) -> anyhow::Result<()> {
    let mut tick = tokio::time::interval(SYNC_INTERVAL);
    loop {
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            _ = tick.tick() => {}
        }
        let repositories = match list_repositories(&state).await {
            Ok(all) => all,
            Err(error) => {
                tracing::warn!(%error, "listing repositories for the catalog sync failed");
                continue;
            }
        };
        for repo in repositories {
            if let Err(error) = sync_repository(&state, &repo.id).await {
                tracing::warn!(repository = %repo.name, %error, "catalog sync failed");
            }
        }
    }
}
#[derive(Serialize, ToSchema)]
pub struct CatalogSyncResult {
    pub added: u64,
    pub skipped: u64,
    pub total: u64,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct SnapshotStatsDto {
    pub files_new: u64,
    pub files_changed: u64,
    pub files_unchanged: u64,
    pub dirs: u64,
    pub bytes_total: u64,
    pub bytes_read: u64,
    pub bytes_added: u64,
}

impl From<&pb::SnapshotStats> for SnapshotStatsDto {
    fn from(s: &pb::SnapshotStats) -> Self {
        SnapshotStatsDto {
            files_new: s.files_new,
            files_changed: s.files_changed,
            files_unchanged: s.files_unchanged,
            dirs: s.dirs,
            bytes_total: s.bytes_total,
            bytes_read: s.bytes_read,
            bytes_added: s.bytes_added,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct SnapshotItem {
    pub id: String,
    pub time: i64,
    pub hostname: String,
    pub paths: Vec<String>,
    pub tags: Vec<String>,
    pub policy_name: Option<String>,
    pub stdin_name: Option<String>,
    pub job_id: Option<String>,
    pub parent_id: Option<String>,
    pub incomplete: bool,
    pub errors_total: u64,
    pub warnings_total: u64,
    pub stats: SnapshotStatsDto,
    pub received_at: i64,
    pub suspicious: Option<SuspicionDto>,
    pub policy_deleted: bool,
}

#[derive(Serialize, ToSchema)]
pub struct SnapshotPage {
    pub items: Vec<SnapshotItem>,
    pub total: u64,
    pub page: u64,
    pub page_size: u64,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PageQuery {
    /// One-based page number.
    pub page: Option<u64>,
}

fn sync_error(error: anyhow::Error) -> ApiError {
    if matches!(
        error.downcast_ref::<RebuildError>(),
        Some(RebuildError::Storage(_))
    ) {
        ApiError::Backend(format!("the repository storage failed: {error}"))
    } else {
        ApiError::Internal(error)
    }
}

#[utoipa::path(
    post,
    tag = "Snapshots",
    summary = "Read new snapshots from storage",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/repositories/{id}/catalog/sync",
    params(("id" = String, Path, description = "Repository id")),
    responses(
        (status = 200, description = "Snapshots found in the bucket", body = CatalogSyncResult),
        (status = 404, description = "Unknown repository", body = ErrorBody),
        (status = 502, description = "The repository storage is unavailable", body = ErrorBody),
    )
)]
async fn sync_catalog(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<CatalogSyncResult>, ApiError> {
    get_repository(&state, &id)
        .await?
        .ok_or_else(|| ApiError::NotFound("repository not found".into()))?;
    let report = sync_repository(&state, &id).await.map_err(sync_error)?;
    Ok(Json(CatalogSyncResult {
        added: report.added,
        skipped: report.skipped,
        total: report.total,
    }))
}

pub(crate) fn item_from_row(row: &sqlx::any::AnyRow) -> Result<SnapshotItem, ApiError> {
    let parse = |e: serde_json::Error| ApiError::Internal(e.into());
    Ok(SnapshotItem {
        id: row.try_get("id")?,
        time: row.try_get("time_ms")?,
        hostname: row.try_get("hostname")?,
        paths: serde_json::from_str(&row.try_get::<String, _>("paths_json")?).map_err(parse)?,
        tags: serde_json::from_str(&row.try_get::<String, _>("tags_json")?).map_err(parse)?,
        policy_name: row.try_get("policy_name")?,
        stdin_name: row.try_get("stdin_name")?,
        job_id: row.try_get("job_id")?,
        parent_id: row.try_get("parent_id")?,
        incomplete: row.try_get::<i64, _>("incomplete")? != 0,
        errors_total: row.try_get::<i64, _>("errors_total")?.max(0) as u64,
        warnings_total: row.try_get::<i64, _>("warnings_total")?.max(0) as u64,
        stats: serde_json::from_str(&row.try_get::<String, _>("stats_json")?).map_err(parse)?,
        received_at: row.try_get("received_at")?,
        suspicious: suspicion_from_row(row)?,
        policy_deleted: row.try_get::<i64, _>("policy_deleted")? != 0,
    })
}

#[utoipa::path(
    get,
    tag = "Snapshots",
    summary = "List snapshots of a host",
    security(("session" = []), ("bearer" = ["snapshots:read"])),
    path = "/api/v1/hosts/{id}/snapshots",
    params(("id" = String, Path, description = "Host id"), PageQuery),
    responses(
        (status = 200, description = "Snapshots of the host, newest first", body = SnapshotPage),
        (status = 404, description = "Unknown host", body = ErrorBody),
    )
)]
async fn list_host_snapshots(
    State(state): State<AppState>,
    _auth: Scoped<SnapshotsRead>,
    Path(id): Path<String>,
    Query(query): Query<PageQuery>,
) -> Result<Json<SnapshotPage>, ApiError> {
    sqlx::query("SELECT 1 FROM agents WHERE id = $1")
        .bind(&id)
        .fetch_optional(&state.db.pool)
        .await?
        .ok_or_else(|| ApiError::NotFound("host not found".into()))?;
    let page = query.page.unwrap_or(1).max(1);
    let total: i64 = sqlx::query("SELECT COUNT(*) AS n FROM snapshots WHERE host_id = $1")
        .bind(&id)
        .fetch_one(&state.db.pool)
        .await?
        .try_get("n")?;
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {SNAPSHOT_COLUMNS} FROM snapshots \
         WHERE host_id = $1 ORDER BY time_ms DESC, id LIMIT $2 OFFSET $3"
    )))
    .bind(&id)
    .bind(PAGE_SIZE)
    .bind(clamp(page - 1).saturating_mul(PAGE_SIZE))
    .fetch_all(&state.db.pool)
    .await?;
    Ok(Json(SnapshotPage {
        items: rows
            .iter()
            .map(item_from_row)
            .collect::<Result<Vec<_>, _>>()?,
        total: total.max(0) as u64,
        page,
        page_size: PAGE_SIZE as u64,
    }))
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StoredSkip {
    Legacy(String),
    Full(SkippedSnapshot),
}

fn stored_skipped(json: Option<&str>) -> Vec<SkippedSnapshot> {
    let parsed: Vec<StoredSkip> = json
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default();
    parsed
        .into_iter()
        .map(|s| match s {
            StoredSkip::Legacy(key) => SkippedSnapshot {
                key,
                reason: String::new(),
            },
            StoredSkip::Full(full) => full,
        })
        .collect()
}

async fn statuses(state: &AppState) -> anyhow::Result<Vec<CatalogStatus>> {
    let repositories = list_repositories(state).await?;
    let mut stored = HashMap::new();
    for row in sqlx::query(
        "SELECT repository_id, last_sync_at, last_error, skipped_json FROM catalog_state",
    )
    .fetch_all(&state.db.pool)
    .await?
    {
        let id: String = row.try_get("repository_id")?;
        stored.insert(
            id,
            (
                row.try_get::<Option<i64>, _>("last_sync_at")?,
                row.try_get::<Option<String>, _>("last_error")?,
                row.try_get::<Option<String>, _>("skipped_json")?,
            ),
        );
    }
    let mut counts = HashMap::new();
    for row in
        sqlx::query("SELECT repository_id, COUNT(*) AS n FROM snapshots GROUP BY repository_id")
            .fetch_all(&state.db.pool)
            .await?
    {
        let id: String = row.try_get("repository_id")?;
        counts.insert(id, row.try_get::<i64, _>("n")?.max(0) as u64);
    }
    Ok(repositories
        .into_iter()
        .map(|repo| {
            let stored = stored.remove(&repo.id);
            let mut status = state.catalog.shown(&repo.id).unwrap_or_else(|| {
                let mut idle = CatalogStatus::new(&repo.id, CatalogPhase::Idle);
                if let Some((_, error, skipped)) = &stored {
                    idle.error.clone_from(error);
                    idle.skipped = stored_skipped(skipped.as_deref());
                }
                idle
            });
            status.repository_name = repo.name;
            status.last_sync_at = stored.and_then(|(at, ..)| at);
            status.snapshots = counts.get(&repo.id).copied().unwrap_or(0);
            status
        })
        .collect())
}

#[utoipa::path(
    get,
    tag = "Snapshots",
    summary = "Catalog rebuild status of every repository",
    path = "/api/v1/catalog",
    responses((status = 200, description = "Phase and progress per repository", body = Vec<CatalogStatus>))
)]
async fn list_catalog_status(
    State(state): State<AppState>,
    _user: AuthUser,
) -> Result<Json<Vec<CatalogStatus>>, ApiError> {
    Ok(Json(statuses(&state).await?))
}

#[utoipa::path(
    post,
    tag = "Snapshots",
    summary = "Rebuild the catalog of a repository from storage",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/repositories/{id}/catalog/rebuild",
    params(("id" = String, Path, description = "Repository id")),
    responses(
        (status = 202, description = "The rebuild runs in the background", body = CatalogStatus),
        (status = 404, description = "Unknown repository", body = ErrorBody),
    )
)]
async fn rebuild_catalog(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<CatalogStatus>), ApiError> {
    get_repository(&state, &id)
        .await?
        .ok_or_else(|| ApiError::NotFound("repository not found".into()))?;
    start_rebuild(&state, &id);
    let status = statuses(&state)
        .await?
        .into_iter()
        .find(|s| s.repository_id == id)
        .ok_or_else(|| ApiError::NotFound("repository not found".into()))?;
    Ok((StatusCode::ACCEPTED, Json(status)))
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(sync_catalog))
        .routes(routes!(list_catalog_status))
        .routes(routes!(rebuild_catalog))
        .routes(routes!(list_host_snapshots))
        .merge(actions::routes())
}
