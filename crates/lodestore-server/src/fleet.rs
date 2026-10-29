use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;
use sqlx::Row;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::alerts::inputs::gather;
use crate::alerts::model::{HostFacts, HostState, PairFacts, host_state, load_thresholds};
use crate::alerts::silences;
use crate::auth::tokens::{HostsRead, Scoped};
use crate::db::now_ms;
use crate::error::{ApiError, ErrorBody};
use crate::hosts::labels::Label;
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PairProblem {
    Overdue,
    Failed,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct FleetPair {
    pub policy: String,
    pub problem: PairProblem,
    pub age_secs: u64,
    pub limit_secs: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LastSnapshot {
    pub time: i64,
    pub bytes_total: u64,
    pub bytes_added: u64,
}

const RECENT_RUNS: i64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Succeeded,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RecentRun {
    pub finished_at: i64,
    pub outcome: RunOutcome,
    pub policy: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct FleetHost {
    pub id: String,
    pub hostname: String,
    pub labels: Vec<Label>,
    pub platform: String,
    pub state: HostState,
    pub last_success_at: Option<i64>,
    pub worst: Option<FleetPair>,
    pub overdue_after_secs: Option<u64>,
    pub last_snapshot: Option<LastSnapshot>,
    pub agent_online: bool,
    pub agent_last_seen_at: Option<i64>,
    pub silenced: bool,
    pub silenced_until: Option<i64>,
    pub has_policy: bool,
    /// The last finished backup runs of the host, oldest first, at most 10.
    pub recent_runs: Vec<RecentRun>,
}

#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct FleetTotals {
    pub hosts: u64,
    pub healthy: u64,
    pub overdue: u64,
    pub failed: u64,
    pub offline: u64,
    pub not_protected: u64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct FleetStorage {
    pub repositories: u64,
    /// Repositories whose size has been counted.
    pub counted: u64,
    pub physical_bytes: Option<u64>,
    pub logical_bytes: Option<u64>,
    /// Logical size divided by the size in the bucket.
    pub dedup_ratio: Option<f64>,
    pub counted_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct FleetOverview {
    pub generated_at: i64,
    pub totals: FleetTotals,
    pub storage: FleetStorage,
    pub hosts: Vec<FleetHost>,
}

fn rank(state: HostState) -> u8 {
    match state {
        HostState::Failed => 0,
        HostState::Overdue => 1,
        HostState::Offline => 2,
        HostState::NotProtected => 3,
        HostState::Healthy => 4,
    }
}

fn secs(ms: i64) -> u64 {
    u64::try_from(ms / 1000).unwrap_or(0)
}

fn worst_pair(pairs: &[PairFacts], now_ms: i64) -> Option<FleetPair> {
    let problem = |p: &PairFacts| match (p.pair_failed(), p.pair_overdue(now_ms)) {
        (true, _) => Some(PairProblem::Failed),
        (false, true) => Some(PairProblem::Overdue),
        _ => None,
    };
    pairs
        .iter()
        .filter_map(|p| problem(p).map(|kind| (kind, p)))
        .min_by_key(|(kind, p)| (*kind == PairProblem::Overdue, -p.age_ms(now_ms)))
        .map(|(problem, p)| FleetPair {
            policy: p.policy.clone(),
            problem,
            age_secs: secs(p.age_ms(now_ms)),
            limit_secs: p.limit.map(|l| l.as_secs()),
            error: p.last_failure.as_ref().map(|(_, message)| message.clone()),
        })
}

fn fleet_host(
    h: &HostFacts,
    state: HostState,
    now_ms: i64,
    recent_runs: Vec<RecentRun>,
) -> FleetHost {
    FleetHost {
        id: h.host_id.clone(),
        hostname: h.hostname.clone(),
        labels: h.labels.clone(),
        platform: h.platform.clone(),
        state,
        last_success_at: h.pairs.iter().filter_map(|p| p.last_success_ms).max(),
        worst: worst_pair(&h.pairs, now_ms),
        overdue_after_secs: h
            .pairs
            .iter()
            .filter_map(|p| p.limit)
            .map(|l| l.as_secs())
            .min(),
        last_snapshot: h.last_snapshot.as_ref().map(|s| LastSnapshot {
            time: s.time_ms,
            bytes_total: s.bytes_total,
            bytes_added: s.bytes_added,
        }),
        agent_online: h.online,
        agent_last_seen_at: h.last_seen_ms,
        silenced: false,
        silenced_until: None,
        has_policy: !h.pairs.is_empty(),
        recent_runs,
    }
}

fn outcome(state: &str) -> Option<RunOutcome> {
    match state {
        "succeeded" | "warnings" => Some(RunOutcome::Succeeded),
        "failed" | "interrupted" => Some(RunOutcome::Failed),
        "cancelled" | "not_started" => Some(RunOutcome::Skipped),
        _ => None,
    }
}

async fn recent_runs(state: &AppState) -> anyhow::Result<HashMap<String, Vec<RecentRun>>> {
    let rows = sqlx::query(
        "SELECT host_id, policy_name, state, finished_at FROM ( \
           SELECT id, host_id, policy_name, state, finished_at, \
             ROW_NUMBER() OVER (PARTITION BY host_id ORDER BY finished_at DESC, id DESC) AS rn \
           FROM jobs \
           WHERE kind = 'backup' AND finished_at IS NOT NULL \
             AND state IN ('succeeded', 'warnings', 'failed', 'interrupted', 'cancelled', 'not_started') \
         ) ranked WHERE rn <= $1 ORDER BY host_id, finished_at ASC, id ASC",
    )
    .bind(RECENT_RUNS)
    .fetch_all(&state.db.pool)
    .await?;
    let mut runs: HashMap<String, Vec<RecentRun>> = HashMap::new();
    for row in &rows {
        let job_state: String = row.try_get("state")?;
        let Some(outcome) = outcome(&job_state) else {
            continue;
        };
        runs.entry(row.try_get("host_id")?)
            .or_default()
            .push(RecentRun {
                finished_at: row.try_get("finished_at")?,
                outcome,
                policy: row.try_get("policy_name")?,
            });
    }
    Ok(runs)
}

pub async fn fleet_hosts(
    state: &AppState,
    now_ms: i64,
    process_started_ms: i64,
) -> anyhow::Result<Vec<FleetHost>> {
    let thresholds = load_thresholds(state).await?;
    let inputs = gather(state, now_ms).await?;
    let silenced = silences(state, now_ms).await?;
    let mut runs = recent_runs(state).await?;
    let mut hosts: Vec<FleetHost> = inputs
        .hosts
        .iter()
        .map(|h| {
            let hs = host_state(h, now_ms, process_started_ms, thresholds.offline_after);
            let mut host = fleet_host(h, hs, now_ms, runs.remove(&h.host_id).unwrap_or_default());
            if let Some(until) = silenced.get(&h.host_id) {
                host.silenced = true;
                host.silenced_until = *until;
            }
            host
        })
        .collect();
    hosts.sort_by(|a, b| {
        (rank(a.state), &a.hostname, &a.id).cmp(&(rank(b.state), &b.hostname, &b.id))
    });
    Ok(hosts)
}

async fn storage(state: &AppState) -> anyhow::Result<FleetStorage> {
    let repositories: i64 = sqlx::query("SELECT COUNT(*) AS n FROM repositories")
        .fetch_one(&state.db.pool)
        .await?
        .try_get("n")?;
    let rows = sqlx::query("SELECT physical_bytes, logical_bytes, counted_at FROM repo_usage")
        .fetch_all(&state.db.pool)
        .await?;
    let (mut physical, mut logical, mut counted_at) = (0u64, 0u64, None::<i64>);
    for row in &rows {
        physical += u64::try_from(row.try_get::<i64, _>("physical_bytes")?).unwrap_or(0);
        logical += u64::try_from(row.try_get::<i64, _>("logical_bytes")?).unwrap_or(0);
        counted_at = counted_at.max(Some(row.try_get("counted_at")?));
    }
    let counted = rows.len() as u64;
    Ok(FleetStorage {
        repositories: u64::try_from(repositories).unwrap_or(0),
        counted,
        physical_bytes: (counted > 0).then_some(physical),
        logical_bytes: (counted > 0).then_some(logical),
        dedup_ratio: (physical > 0 && logical > 0).then(|| logical as f64 / physical as f64),
        counted_at,
    })
}

pub async fn overview(
    state: &AppState,
    now_ms: i64,
    process_started_ms: i64,
) -> anyhow::Result<FleetOverview> {
    let hosts = fleet_hosts(state, now_ms, process_started_ms).await?;
    let mut totals = FleetTotals {
        hosts: hosts.len() as u64,
        ..FleetTotals::default()
    };
    for host in &hosts {
        *match host.state {
            HostState::Healthy => &mut totals.healthy,
            HostState::Overdue => &mut totals.overdue,
            HostState::Failed => &mut totals.failed,
            HostState::Offline => &mut totals.offline,
            HostState::NotProtected => &mut totals.not_protected,
        } += 1;
    }
    Ok(FleetOverview {
        generated_at: now_ms,
        totals,
        storage: storage(state).await?,
        hosts,
    })
}

#[utoipa::path(
    get,
    tag = "Overview",
    operation_id = "fleet_overview",
    summary = "Fleet overview",
    description = "Every host with one state (failed, overdue, offline, not protected or healthy), \
        its last successful backup and its worst policy, plus totals and the space used.",
    security(("session" = []), ("bearer" = ["hosts:read"])),
    path = "/api/v1/fleet",
    responses((status = 200, description = "Host states, totals and storage", body = FleetOverview))
)]
async fn get_overview(
    State(state): State<AppState>,
    _auth: Scoped<HostsRead>,
) -> Result<Json<FleetOverview>, ApiError> {
    Ok(Json(overview(&state, now_ms(), state.started_at_ms).await?))
}

#[utoipa::path(
    get,
    tag = "Overview",
    operation_id = "fleet_host",
    summary = "Fleet state of one host",
    security(("session" = []), ("bearer" = ["hosts:read"])),
    path = "/api/v1/fleet/hosts/{id}",
    params(("id" = String, Path, description = "Host id")),
    responses(
        (status = 200, description = "The host state and its worst policy", body = FleetHost),
        (status = 404, description = "Unknown or revoked host", body = ErrorBody),
    )
)]
async fn get_host(
    State(state): State<AppState>,
    _auth: Scoped<HostsRead>,
    Path(id): Path<String>,
) -> Result<Json<FleetHost>, ApiError> {
    fleet_hosts(&state, now_ms(), state.started_at_ms)
        .await?
        .into_iter()
        .find(|h| h.id == id)
        .map(Json)
        .ok_or_else(|| ApiError::NotFound("host not found".into()))
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_overview))
        .routes(routes!(get_host))
}
