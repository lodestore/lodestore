use std::collections::{BTreeMap, HashMap};
use std::fmt::{Display, Write};

use anyhow::Context;
use axum::Router;
use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::Value;
use sqlx::Row;

use crate::alerts::inputs::{Inputs, gather};
use crate::alerts::model::{
    AlertType, HostFacts, HostState, PairFacts, Thresholds, host_state, load_thresholds,
};
use crate::alerts::silences;
use crate::auth::tokens::{MetricsRead, Scoped};
use crate::db::now_ms;
use crate::error::ApiError;
use crate::jobs::JobState;
use crate::maintenance::check::CheckState;
use crate::maintenance::check::db::COVERAGE_DAYS;
use crate::maintenance::gc::GcState;
use crate::state::AppState;

pub const CONTENT_TYPE_TEXT: &str = "text/plain; version=0.0.4; charset=utf-8";

type Labels<'a> = Vec<(&'static str, &'a str)>;

pub fn label_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[derive(Default)]
pub struct Exposition {
    out: String,
    name: String,
}

impl Exposition {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn gauge(&mut self, name: &str, help: &str) {
        self.name = name.to_owned();
        let _ = writeln!(self.out, "# HELP {name} {help}\n# TYPE {name} gauge");
    }

    pub fn sample(&mut self, labels: &[(&str, &str)], value: impl Display) {
        self.out.push_str(&self.name);
        let mut first = true;
        for (key, value) in labels.iter().filter(|(_, value)| !value.is_empty()) {
            self.out.push(if first { '{' } else { ',' });
            first = false;
            let _ = write!(self.out, "{key}=\"{}\"", label_value(value));
        }
        if !first {
            self.out.push('}');
        }
        let _ = writeln!(self.out, " {value}");
    }

    pub fn finish(self) -> String {
        self.out
    }

    fn family<'a>(
        &mut self,
        name: &str,
        help: &str,
        samples: impl IntoIterator<Item = (Labels<'a>, f64)>,
    ) {
        self.gauge(name, help);
        for (labels, value) in samples {
            self.sample(&labels, value);
        }
    }
}

fn secs(ms: i64) -> f64 {
    ms as f64 / 1000.0
}

fn flag(on: bool) -> f64 {
    if on { 1.0 } else { 0.0 }
}

#[derive(Default)]
struct PairDetail {
    duration_secs: Option<f64>,
    bytes_added: Option<u64>,
    bytes_total: Option<u64>,
}

struct PairRow<'a> {
    host: &'a HostFacts,
    pair: &'a PairFacts,
    detail: Option<&'a PairDetail>,
}

impl<'a> PairRow<'a> {
    fn labels(&self) -> Labels<'a> {
        vec![
            ("host_id", self.host.host_id.as_str()),
            ("host", self.host.hostname.as_str()),
            ("policy", self.pair.policy.as_str()),
        ]
    }
}

fn host_labels(host: &HostFacts) -> Labels<'_> {
    vec![
        ("host_id", host.host_id.as_str()),
        ("host", host.hostname.as_str()),
    ]
}

fn state_name(state: HostState) -> &'static str {
    match state {
        HostState::Healthy => "healthy",
        HostState::Overdue => "overdue",
        HostState::Failed => "failed",
        HostState::Offline => "offline",
        HostState::NotProtected => "not_protected",
    }
}

const HOST_STATES: [HostState; 5] = [
    HostState::Healthy,
    HostState::Overdue,
    HostState::Failed,
    HostState::Offline,
    HostState::NotProtected,
];

// ponytail: recomputed on every scrape; a 5 s cache if scrapes get frequent
pub async fn render(state: &AppState, now_ms: i64) -> anyhow::Result<String> {
    let thresholds = load_thresholds(state).await?;
    let inputs = gather(state, now_ms).await?;
    let details = pair_details(state).await?;
    let mut x = Exposition::new();
    x.family(
        "lodestore_build_info",
        "Server version.",
        [(vec![("version", env!("CARGO_PKG_VERSION"))], 1.0)],
    );
    host_series(&mut x, state, &inputs, now_ms, &thresholds);
    pair_series(&mut x, &inputs, &details, now_ms);
    job_series(&mut x, state, now_ms).await?;
    repository_series(&mut x, state, now_ms).await?;
    check_series(&mut x, state).await?;
    x.family(
        "lodestore_server_cert_expiry_timestamp_seconds",
        "When a certificate of the server expires, as a Unix time.",
        inputs
            .server_cert
            .map(|(not_after_ms, _)| (vec![("cert", "grpc")], secs(not_after_ms))),
    );
    alert_series(&mut x, state, now_ms).await?;
    Ok(x.finish())
}

fn host_series(
    x: &mut Exposition,
    state: &AppState,
    inputs: &Inputs,
    now_ms: i64,
    thresholds: &Thresholds,
) {
    let mut counts = [0u32; 5];
    for host in &inputs.hosts {
        let hs = host_state(host, now_ms, state.started_at_ms, thresholds.offline_after);
        counts[HOST_STATES.iter().position(|s| *s == hs).unwrap_or(0)] += 1;
    }
    x.family(
        "lodestore_hosts",
        "Hosts by state.",
        HOST_STATES
            .iter()
            .zip(counts)
            .map(|(s, n)| (vec![("state", state_name(*s))], f64::from(n))),
    );
    let hosts = &inputs.hosts;
    x.family(
        "lodestore_host_agent_up",
        "1 while the agent is connected.",
        hosts.iter().map(|h| (host_labels(h), flag(h.online))),
    );
    x.family(
        "lodestore_host_agent_last_seen_timestamp_seconds",
        "When the agent was last seen, as a Unix time.",
        hosts
            .iter()
            .filter_map(|h| Some((host_labels(h), secs(h.last_seen_ms?)))),
    );
    x.family(
        "lodestore_host_cert_expiry_timestamp_seconds",
        "When the agent certificate expires, as a Unix time.",
        hosts
            .iter()
            .map(|h| (host_labels(h), secs(h.cert_not_after_ms))),
    );
}

fn pair_series(
    x: &mut Exposition,
    inputs: &Inputs,
    details: &HashMap<Pair, PairDetail>,
    now_ms: i64,
) {
    let rows: Vec<PairRow> = inputs
        .hosts
        .iter()
        .flat_map(|host| {
            host.pairs.iter().map(move |pair| PairRow {
                host,
                pair,
                detail: details.get(&(pair.policy.clone(), host.host_id.clone())),
            })
        })
        .collect();
    x.family(
        "lodestore_backup_last_success_timestamp_seconds",
        "When the last successful backup finished, as a Unix time. No series before the first success.",
        rows.iter()
            .filter_map(|r| Some((r.labels(), secs(r.pair.last_success_ms?)))),
    );
    x.family(
        "lodestore_backup_last_status",
        "1 when the last attempt succeeded, 0 when it failed.",
        rows.iter().filter_map(|r| {
            let status = if r.pair.pair_failed() {
                0.0
            } else {
                r.pair.last_success_ms.map(|_| 1.0)?
            };
            Some((r.labels(), status))
        }),
    );
    x.family(
        "lodestore_backup_last_duration_seconds",
        "How long the last successful backup took.",
        rows.iter()
            .filter_map(|r| Some((r.labels(), r.detail?.duration_secs?))),
    );
    x.family(
        "lodestore_backup_last_bytes_added",
        "Bytes the last successful snapshot added to the repository.",
        rows.iter()
            .filter_map(|r| Some((r.labels(), r.detail?.bytes_added? as f64))),
    );
    x.family(
        "lodestore_backup_last_bytes_total",
        "Size of the data in the last successful snapshot.",
        rows.iter()
            .filter_map(|r| Some((r.labels(), r.detail?.bytes_total? as f64))),
    );
    x.family(
        "lodestore_backup_overdue",
        "1 when the last success is older than the limit.",
        rows.iter()
            .map(|r| (r.labels(), flag(r.pair.pair_overdue(now_ms)))),
    );
    x.family(
        "lodestore_backup_overdue_after_seconds",
        "How old the last success may get before the backup counts as overdue.",
        rows.iter()
            .filter_map(|r| Some((r.labels(), r.pair.limit?.as_secs_f64()))),
    );
}

type Pair = (String, String);

async fn pair_details(state: &AppState) -> anyhow::Result<HashMap<Pair, PairDetail>> {
    let mut out: HashMap<Pair, PairDetail> = HashMap::new();
    let rows = sqlx::query(
        "SELECT s.policy_name, s.host_id, s.stats_json FROM snapshots s JOIN ( \
             SELECT policy_name, host_id, MAX(received_at) AS at FROM snapshots \
             WHERE incomplete = 0 AND policy_name IS NOT NULL AND host_id IS NOT NULL \
             GROUP BY policy_name, host_id \
         ) l ON l.policy_name = s.policy_name AND l.host_id = s.host_id AND l.at = s.received_at \
         WHERE s.incomplete = 0",
    )
    .fetch_all(&state.db.pool)
    .await
    .context("reading the last snapshots")?;
    for row in &rows {
        let stats: Value =
            serde_json::from_str(&row.try_get::<String, _>("stats_json")?).unwrap_or(Value::Null);
        let detail = out
            .entry((row.try_get("policy_name")?, row.try_get("host_id")?))
            .or_default();
        detail.bytes_added = stats["bytes_added"].as_u64();
        detail.bytes_total = stats["bytes_total"].as_u64();
    }
    let rows = sqlx::query(
        "SELECT j.policy_name, j.host_id, j.started_at, j.finished_at FROM jobs j JOIN ( \
             SELECT policy_name, host_id, MAX(finished_at) AS at FROM jobs \
             WHERE kind = 'backup' AND state IN ('succeeded', 'warnings') \
                 AND policy_name IS NOT NULL AND started_at IS NOT NULL \
             GROUP BY policy_name, host_id \
         ) l ON l.policy_name = j.policy_name AND l.host_id = j.host_id AND l.at = j.finished_at \
         WHERE j.kind = 'backup' AND j.state IN ('succeeded', 'warnings') \
             AND j.started_at IS NOT NULL",
    )
    .fetch_all(&state.db.pool)
    .await
    .context("reading the last successful backup jobs")?;
    for row in &rows {
        let started: i64 = row.try_get("started_at")?;
        let finished: i64 = row.try_get("finished_at")?;
        out.entry((row.try_get("policy_name")?, row.try_get("host_id")?))
            .or_default()
            .duration_secs = Some(secs((finished - started).max(0)));
    }
    Ok(out)
}

type Counts = BTreeMap<(&'static str, &'static str), i64>;

const OPEN_STATES: [&str; 3] = ["queued", "running", "waiting"];

const JOB_STATES: [(&str, &[&str]); 5] = [
    (
        "backup",
        &[
            "queued",
            "running",
            "succeeded",
            "warnings",
            "failed",
            "cancelled",
            "interrupted",
        ],
    ),
    (
        "restore",
        &[
            "queued",
            "running",
            "succeeded",
            "warnings",
            "failed",
            "cancelled",
            "interrupted",
        ],
    ),
    (
        "download",
        &[
            "queued",
            "running",
            "succeeded",
            "interrupted",
            "not_started",
        ],
    ),
    (
        "gc",
        &["running", "waiting", "succeeded", "failed", "cancelled"],
    ),
    (
        "check",
        &["queued", "running", "succeeded", "failed", "cancelled"],
    ),
];

fn job_state_name(state: JobState) -> &'static str {
    match state {
        JobState::Queued => "queued",
        JobState::Running => "running",
        JobState::Waiting => "waiting",
        JobState::Succeeded => "succeeded",
        JobState::Warnings => "warnings",
        JobState::Failed => "failed",
        JobState::Cancelled => "cancelled",
        JobState::Interrupted => "interrupted",
        JobState::NotStarted => "not_started",
    }
}

fn gc_state_name(state: GcState) -> &'static str {
    job_state_name(match state {
        GcState::Marking | GcState::Repacking | GcState::Verifying | GcState::Sweeping => {
            JobState::Running
        }
        GcState::Waiting => JobState::Waiting,
        GcState::Done => JobState::Succeeded,
        GcState::Failed => JobState::Failed,
        GcState::Aborted => JobState::Cancelled,
    })
}

fn check_state_name(state: CheckState) -> &'static str {
    job_state_name(match state {
        CheckState::Queued => JobState::Queued,
        CheckState::Running => JobState::Running,
        CheckState::Done => JobState::Succeeded,
        CheckState::Failed => JobState::Failed,
        CheckState::Aborted => JobState::Cancelled,
    })
}

fn kind_name(kind: &str) -> Option<&'static str> {
    match kind {
        "backup" => Some("backup"),
        "restore" => Some("restore"),
        _ => None,
    }
}

async fn job_counts(
    state: &AppState,
    now_ms: i64,
    finished_since: Option<i64>,
) -> anyhow::Result<Counts> {
    let (since, all) = finished_since.map_or((0, 1_i64), |since| (since, 0));
    let mut out = Counts::new();
    for (kind, states) in JOB_STATES {
        for name in states {
            if all == 1 || !OPEN_STATES.contains(name) {
                out.insert((kind, name), 0);
            }
        }
    }
    let mut add = |kind: &'static str, name: Option<&'static str>, n: i64| {
        if let Some(name) = name {
            *out.entry((kind, name)).or_default() += n;
        }
    };

    let rows = sqlx::query(
        "SELECT kind, state, COUNT(*) AS n FROM jobs \
         WHERE $2 = 1 OR finished_at >= $1 GROUP BY kind, state",
    )
    .bind(since)
    .bind(all)
    .fetch_all(&state.db.pool)
    .await
    .context("counting jobs")?;
    for row in &rows {
        let kind: String = row.try_get("kind")?;
        let db_state: String = row.try_get("state")?;
        if let Some(kind) = kind_name(&kind) {
            add(
                kind,
                JobState::parse(&db_state).map(job_state_name),
                row.try_get("n")?,
            );
        }
    }

    let rows = sqlx::query(
        "SELECT state, CASE WHEN state = 'pending' AND created_at <= $3 THEN 1 ELSE 0 END AS expired, \
         COUNT(*) AS n FROM downloads WHERE $2 = 1 OR finished_at >= $1 GROUP BY 1, 2",
    )
    .bind(since)
    .bind(all)
    .bind(now_ms - crate::download::api::TICKET_TTL_MS)
    .fetch_all(&state.db.pool)
    .await
    .context("counting downloads")?;
    for row in &rows {
        let db_state: String = row.try_get("state")?;
        let expired: i64 = row.try_get("expired")?;
        let name = match (db_state.as_str(), expired) {
            ("pending", 0) => Some("queued"),
            ("pending", _) => Some("not_started"),
            ("running", _) => Some("running"),
            ("succeeded", _) => Some("succeeded"),
            ("interrupted", _) => Some("interrupted"),
            _ => None,
        };
        add("download", name, row.try_get("n")?);
    }

    let rows = sqlx::query(
        "SELECT state, COUNT(*) AS n FROM gc_cycles \
         WHERE $2 = 1 OR finished_at >= $1 GROUP BY state",
    )
    .bind(since)
    .bind(all)
    .fetch_all(&state.db.pool)
    .await
    .context("counting cleanup cycles")?;
    for row in &rows {
        let db_state: String = row.try_get("state")?;
        add(
            "gc",
            GcState::parse(&db_state).map(gc_state_name),
            row.try_get("n")?,
        );
    }

    let rows = sqlx::query(
        "SELECT state, COUNT(*) AS n FROM check_runs \
         WHERE $2 = 1 OR finished_at >= $1 GROUP BY state",
    )
    .bind(since)
    .bind(all)
    .fetch_all(&state.db.pool)
    .await
    .context("counting check runs")?;
    for row in &rows {
        let db_state: String = row.try_get("state")?;
        add(
            "check",
            CheckState::parse(&db_state).map(check_state_name),
            row.try_get("n")?,
        );
    }
    Ok(out)
}

async fn job_series(x: &mut Exposition, state: &AppState, now_ms: i64) -> anyhow::Result<()> {
    for (name, help, since) in [
        ("lodestore_jobs", "Jobs by kind and state.", None),
        (
            "lodestore_jobs_finished_24h",
            "Jobs that finished in the last 24 hours, by kind and state.",
            Some(now_ms - 24 * 3_600_000),
        ),
    ] {
        let counts = job_counts(state, now_ms, since).await?;
        x.family(
            name,
            help,
            counts
                .into_iter()
                .map(|((kind, state), n)| (vec![("kind", kind), ("state", state)], n as f64)),
        );
    }
    Ok(())
}

struct Usage {
    repository: String,
    physical: i64,
    logical: i64,
    counted_at: i64,
}

async fn repository_series(
    x: &mut Exposition,
    state: &AppState,
    now_ms: i64,
) -> anyhow::Result<()> {
    let rows = sqlx::query(
        "SELECT r.name, u.physical_bytes, u.logical_bytes, u.counted_at FROM repo_usage u \
         JOIN repositories r ON r.id = u.repository_id ORDER BY r.name",
    )
    .fetch_all(&state.db.pool)
    .await
    .context("reading repository usage")?;
    let usage = rows
        .iter()
        .map(|row| {
            Ok(Usage {
                repository: row.try_get("name")?,
                physical: row.try_get("physical_bytes")?,
                logical: row.try_get("logical_bytes")?,
                counted_at: row.try_get("counted_at")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
    let mut series = |name: &str, help: &str, value: fn(&Usage) -> Option<f64>| {
        x.family(
            name,
            help,
            usage
                .iter()
                .filter_map(|u| Some((vec![("repository", u.repository.as_str())], value(u)?))),
        );
    };
    series(
        "lodestore_repository_physical_bytes",
        "Size of the repository in the bucket.",
        |u| Some(u.physical as f64),
    );
    series(
        "lodestore_repository_logical_bytes",
        "Size of the data in live snapshots before deduplication.",
        |u| Some(u.logical as f64),
    );
    series(
        "lodestore_repository_dedup_ratio",
        "Logical size divided by the size in the bucket.",
        |u| (u.physical > 0).then(|| u.logical as f64 / u.physical as f64),
    );
    series(
        "lodestore_repository_counted_timestamp_seconds",
        "When the size was last counted, as a Unix time.",
        |u| Some(secs(u.counted_at)),
    );

    let rows = sqlx::query(
        "SELECT r.name, COUNT(*) AS packs, COUNT(CASE WHEN p.checked_at >= $1 THEN 1 END) AS recent \
         FROM pack_checks p JOIN repositories r ON r.id = p.repository_id \
         GROUP BY r.name ORDER BY r.name",
    )
    .bind(now_ms - COVERAGE_DAYS * 24 * 3_600_000)
    .fetch_all(&state.db.pool)
    .await
    .context("reading pack coverage")?;
    let coverage = rows
        .iter()
        .map(|row| {
            let packs: i64 = row.try_get("packs")?;
            let recent: i64 = row.try_get("recent")?;
            Ok((
                row.try_get::<String, _>("name")?,
                recent as f64 / packs.max(1) as f64,
            ))
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
    x.family(
        "lodestore_pack_coverage_ratio",
        "Share of the packs read by a check in the last 30 days.",
        coverage
            .iter()
            .map(|(name, ratio)| (vec![("repository", name.as_str())], *ratio)),
    );
    Ok(())
}

struct CheckRow {
    repository: String,
    level: String,
    ok: bool,
    findings: i64,
    duration_secs: Option<f64>,
}

async fn check_series(x: &mut Exposition, state: &AppState) -> anyhow::Result<()> {
    let rows = sqlx::query(
        "SELECT r.name, c.level, c.state, c.findings_total, c.started_at, c.finished_at \
         FROM check_runs c JOIN repositories r ON r.id = c.repository_id JOIN ( \
             SELECT repository_id, level, MAX(created_at) AS at FROM check_runs \
             WHERE state IN ('done', 'failed') AND (outcome IS NULL OR outcome <> 'stopped' OR report_json IS NOT NULL) \
             GROUP BY repository_id, level \
         ) l ON l.repository_id = c.repository_id AND l.level = c.level AND l.at = c.created_at \
         WHERE c.state IN ('done', 'failed') AND (c.outcome IS NULL OR c.outcome <> 'stopped' OR c.report_json IS NOT NULL) \
         ORDER BY r.name, c.level, c.id",
    )
    .fetch_all(&state.db.pool)
    .await
    .context("reading the last check runs")?;
    let mut last: Vec<CheckRow> = Vec::new();
    for row in &rows {
        let repository: String = row.try_get("name")?;
        let level: String = row.try_get("level")?;
        if last
            .iter()
            .any(|c| c.repository == repository && c.level == level)
        {
            continue;
        }
        let started: Option<i64> = row.try_get("started_at")?;
        let finished: Option<i64> = row.try_get("finished_at")?;
        last.push(CheckRow {
            repository,
            level,
            ok: row.try_get::<String, _>("state")? == "done",
            findings: row.try_get("findings_total")?,
            duration_secs: started.zip(finished).map(|(s, f)| secs((f - s).max(0))),
        });
    }
    let rows = sqlx::query(
        "SELECT r.name, c.level, MAX(c.finished_at) AS at FROM check_runs c \
         JOIN repositories r ON r.id = c.repository_id \
         WHERE c.state = 'done' AND c.finished_at IS NOT NULL GROUP BY r.name, c.level \
         ORDER BY r.name, c.level",
    )
    .fetch_all(&state.db.pool)
    .await
    .context("reading the last passing check runs")?;
    let passed = rows
        .iter()
        .map(|row| {
            Ok((
                row.try_get::<String, _>("name")?,
                row.try_get::<String, _>("level")?,
                row.try_get::<i64, _>("at")?,
            ))
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;

    x.family(
        "lodestore_check_last_success_timestamp_seconds",
        "When the last passing check finished, as a Unix time.",
        passed.iter().map(|(repository, level, at)| {
            (
                vec![
                    ("repository", repository.as_str()),
                    ("level", level.as_str()),
                ],
                secs(*at),
            )
        }),
    );
    let mut series = |name: &str, help: &str, value: fn(&CheckRow) -> Option<f64>| {
        x.family(
            name,
            help,
            last.iter().filter_map(|c| {
                Some((
                    vec![
                        ("repository", c.repository.as_str()),
                        ("level", c.level.as_str()),
                    ],
                    value(c)?,
                ))
            }),
        );
    };
    series(
        "lodestore_check_last_status",
        "1 when the last finished check passed, 0 when it found problems or stopped at a limit.",
        |c| Some(flag(c.ok)),
    );
    series(
        "lodestore_check_findings",
        "Problems the last finished check reported.",
        |c| Some(c.findings as f64),
    );
    series(
        "lodestore_check_last_duration_seconds",
        "How long the last finished check ran.",
        |c| c.duration_secs,
    );
    Ok(())
}

const ALERT_TYPES: [AlertType; 5] = [
    AlertType::BackupFailed,
    AlertType::NoRecentBackup,
    AlertType::AgentOffline,
    AlertType::CheckFailed,
    AlertType::CertificateExpiring,
];

async fn alert_series(x: &mut Exposition, state: &AppState, now_ms: i64) -> anyhow::Result<()> {
    let silenced = silences(state, now_ms).await?;
    let rows = sqlx::query("SELECT type, host_id FROM alerts WHERE state = 'firing'")
        .fetch_all(&state.db.pool)
        .await
        .context("reading active alerts")?;
    let mut counts: BTreeMap<(&'static str, bool), u32> = BTreeMap::new();
    for kind in ALERT_TYPES {
        counts.insert((kind.as_str(), false), 0);
        counts.insert((kind.as_str(), true), 0);
    }
    for row in &rows {
        let kind: String = row.try_get("type")?;
        let host_id: Option<String> = row.try_get("host_id")?;
        let Some(kind) = AlertType::parse(&kind) else {
            continue;
        };
        let silenced = host_id.is_some_and(|id| silenced.contains_key(&id));
        *counts.entry((kind.as_str(), silenced)).or_default() += 1;
    }
    x.family(
        "lodestore_alerts_active",
        "Active alerts by type; silenced alerts belong to silenced hosts.",
        counts.into_iter().map(|((kind, silenced), n)| {
            let silenced = if silenced { "true" } else { "false" };
            (vec![("type", kind), ("silenced", silenced)], f64::from(n))
        }),
    );
    Ok(())
}

async fn scrape(
    State(state): State<AppState>,
    _auth: Scoped<MetricsRead>,
) -> Result<Response, ApiError> {
    respond(&state).await
}

async fn scrape_open(State(state): State<AppState>) -> Result<Response, ApiError> {
    respond(&state).await
}

async fn respond(state: &AppState) -> Result<Response, ApiError> {
    let text = render(state, now_ms()).await?;
    Ok((
        [
            (CONTENT_TYPE, CONTENT_TYPE_TEXT),
            (CACHE_CONTROL, "no-store"),
        ],
        text,
    )
        .into_response())
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/metrics", get(scrape))
        .with_state(state)
}

pub fn open_router(state: AppState) -> Router {
    Router::new()
        .route("/metrics", get(scrape_open))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_value_escapes() {
        assert_eq!(label_value(r#"we"b\1"#), r#"we\"b\\1"#);
        assert_eq!(label_value("a\nb"), "a\\nb");
        assert_eq!(label_value("plain"), "plain");
    }

    #[test]
    fn exposition_writes_help_type_and_labels() {
        let mut x = Exposition::new();
        x.gauge("lodestore_x", "Help.");
        x.sample(&[("a", "1"), ("empty", ""), ("b", "q\"")], 2.5);
        x.sample(&[], 3);
        assert_eq!(
            x.finish(),
            "# HELP lodestore_x Help.\n# TYPE lodestore_x gauge\n\
             lodestore_x{a=\"1\",b=\"q\\\"\"} 2.5\nlodestore_x 3\n"
        );
    }
}
