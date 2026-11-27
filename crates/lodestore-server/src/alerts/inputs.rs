use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Context;
use lodestore_policy::BackupPolicy;
use lodestore_policy::jiff::Timestamp;
use lodestore_policy::source;
use serde_json::Value;
use sqlx::Row;

use super::evaluate::{GONE_AFTER_MS, GONE_WINDOW_MS};
use super::model::{AlertFacts, AlertType, HostFacts, PairFacts, SnapshotFacts, overdue_limit};
use crate::agent_update::reached;
use crate::maintenance::check::CheckLevel;
use crate::policies::matching::AgentRow;
use crate::sources;
use crate::sources::select::{Selection, select, selects_guest};
use crate::sources::{AgentSources, Guest, GuestType, ProxmoxNode, ScopeKind, Stored, is_fresh};
use crate::state::AppState;

#[derive(Debug, Default)]
pub struct Inputs {
    pub hosts: Vec<HostFacts>,
    pub server_cert: Option<(i64, bool)>,
    pub check_failures: Vec<CheckFailure>,
    pub update_failures: Vec<UpdateFailure>,
    pub quarantines: Vec<Quarantine>,
    pub unavailable: Vec<UnavailableRepository>,
    pub security: Vec<SecurityAlert>,
    pub unreadable_policies: Vec<String>,
    pub guarantees: Vec<RepositoryGuarantee>,
    /// Whether the daily guarantee recheck runs at all.
    pub daily_recheck: bool,
    /// Guests that were backed up once and that no policy selects now, by host and instance.
    pub unprotected: Vec<UnprotectedGuest>,
    /// Stored unprotected guests that a fresh complete list shows selected again or gone.
    pub unprotected_gone: Vec<(String, String)>,
    pub plugins: Vec<PluginFinding>,
    /// Names of the Proxmox clusters and nodes whose plugin state was read this pass.
    pub plugin_scopes: HashSet<String>,
}

/// A guest that was protected and is not any more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnprotectedGuest {
    pub host_id: String,
    pub source_ref: String,
    pub label: Option<String>,
    pub last_snapshot_ms: Option<i64>,
    /// The policy of the last snapshot.
    pub policy: Option<String>,
    /// When the alert loop first saw the guest unprotected; set by `record_unprotected`.
    pub since_ms: Option<i64>,
}

/// A Proxmox node whose Lodestore plugin is missing or does not match the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginFinding {
    /// The host that carries the alert.
    pub host_id: String,
    /// The cluster, or the node itself when it is not in a cluster.
    pub scope: String,
    pub in_cluster: bool,
    pub node: String,
    pub outdated: bool,
}

/// What the guarantee alert needs to know about a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryGuarantee {
    pub repository_id: String,
    pub repository: String,
    pub level: String,
    pub accepted: String,
    pub unchecked_runs: u64,
    pub failed_runs: u64,
    /// When a level was last written; the creation time before that was recorded.
    pub measured_at_ms: i64,
    pub tick_at_ms: Option<i64>,
    /// The pinned and the offered SFTP host key fingerprints while the server presents another key.
    pub host_key_mismatch: Option<(String, String)>,
    /// The last stored checks show the agents' credentials failing to write.
    pub agent_cannot_write: bool,
}

/// The last verdict of a check level in a repository, when it was a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckFailure {
    pub repository_id: String,
    pub repository: String,
    pub level: CheckLevel,
    pub run_id: String,
    pub findings: u64,
    pub error: Option<String>,
    pub finished_at: i64,
}

/// A host whose latest finished self-update was rolled back and which still runs another version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateFailure {
    pub host_id: String,
    pub hostname: String,
    pub reason: String,
}

/// Snapshots of a repository that the cleanup cannot read and that nobody has confirmed yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quarantine {
    pub repository_id: String,
    pub repository: String,
    pub count: u64,
    pub first_key: String,
    pub first_reason: String,
}

/// A repository the server cannot open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnavailableRepository {
    pub repository_id: String,
    pub repository: String,
    pub error: String,
}

/// A recent security event of an agent session; `at_ms` is when it happened (or, after a restart, when its alert opened).
#[derive(Debug, Clone, PartialEq)]
pub struct SecurityAlert {
    pub host_id: String,
    pub kind: AlertType,
    pub at_ms: i64,
    pub facts: AlertFacts,
}

struct Agent {
    row: AgentRow,
    platform: String,
    enrolled_at: i64,
    last_seen_ms: Option<i64>,
    cert_not_after_ms: i64,
}

struct Policy {
    name: String,
    repository_id: String,
    policy: BackupPolicy,
    created_at: i64,
}

type Pair = (String, String);
type Instance = (String, i64, Option<String>);
type Failures = HashMap<String, (i64, String)>;
/// When an instance stopped being expected, and when it came back inside its gone window.
type Gone = (i64, Option<i64>);

/// The instances a source selects on a host; `complete` is false when the list may hold more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub keys: Vec<String>,
    pub complete: bool,
}

/// Files expect the host itself. Other sources expect what discovery selects plus the instances whose last job failed, and every known instance while the list is unknown or incomplete, so missing data never silences the dead-man. With nothing expected, one stub (`''`) stands for the policy so it still alerts.
pub fn expected_instances(
    policy: &BackupPolicy,
    discovered: Option<&Discovered>,
    known: &[String],
    failed: &[String],
) -> Vec<String> {
    if source::normalized(&policy.spec.source).is_none() {
        return vec![String::new()];
    }
    let mut out: Vec<String> = failed.to_vec();
    match discovered {
        Some(d) => out.extend(d.keys.iter().cloned()),
        None => out.extend(known.iter().cloned()),
    }
    if discovered.is_some_and(|d| !d.complete) {
        out.extend(known.iter().cloned());
    }
    out.sort();
    out.dedup();
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// A failure counts only while it is newer than the last success of its instance; the stub's failure must also be newer than any success of the policy, or an old refusal would burn for ever.
fn live_failures(
    policy: &BackupPolicy,
    failures: Option<&Failures>,
    known: &[Instance],
) -> Failures {
    let stub = source::normalized(&policy.spec.source).is_some();
    let any_success = known.iter().map(|(_, at, _)| *at).max();
    failures
        .into_iter()
        .flatten()
        .filter(|(source_ref, (at, _))| {
            let own = known.iter().find(|(r, ..)| r == *source_ref);
            let ok = if stub && source_ref.is_empty() {
                any_success
            } else {
                own.map(|(_, at, _)| *at)
            };
            ok.is_none_or(|ok| *at > ok)
        })
        .map(|(r, f)| (r.clone(), f.clone()))
        .collect()
}

/// The expected instances first, then the known ones the policy no longer expects, by `source_ref`.
pub fn instance_facts(
    policy: &str,
    limit: Option<std::time::Duration>,
    baseline_ms: i64,
    expected: &[String],
    known: &[Instance],
    failures: &Failures,
    gone: &HashMap<String, Gone>,
) -> Vec<PairFacts> {
    let facts = |source_ref: &str, expected_now: bool| {
        let found = known.iter().find(|(r, ..)| r == source_ref);
        let last_success_ms = found.map(|(_, at, _)| *at);
        let moment = gone.get(source_ref);
        PairFacts {
            policy: policy.to_owned(),
            limit,
            last_success_ms,
            baseline_ms,
            last_failure: failures
                .get(source_ref)
                .filter(|(at, _)| expected_now && last_success_ms.is_none_or(|ok| *at > ok))
                .cloned(),
            source_ref: source_ref.to_owned(),
            source_label: found.and_then(|(_, _, label)| label.clone()),
            expected: expected_now,
            gone_at_ms: moment.map(|(at, _)| *at),
            returned_at_ms: moment.and_then(|(_, back)| *back),
        }
    };
    let mut out: Vec<PairFacts> = expected.iter().map(|r| facts(r, true)).collect();
    let mut dropped: Vec<&str> = known
        .iter()
        .map(|(r, ..)| r.as_str())
        .filter(|r| !expected.iter().any(|e| e == r))
        .collect();
    dropped.sort_unstable();
    out.extend(dropped.into_iter().map(|r| facts(r, false)));
    out
}

pub async fn gather(state: &AppState, now_ms: i64) -> anyhow::Result<Inputs> {
    let sources = sources::load_all(state).await?;
    let mut inputs = gather_with(state, now_ms, |policy, host| {
        let source = source::normalized(&policy.spec.source)?;
        let stored = sources.get(host).and_then(|a| a.stored.as_ref());
        match select(source, stored, now_ms) {
            Selection::Unknown => None,
            Selection::NoSource | Selection::Nothing => Some(Discovered {
                keys: Vec::new(),
                complete: true,
            }),
            Selection::Some { keys, complete } => Some(Discovered { keys, complete }),
        }
    })
    .await?;
    watch_guests(state, &sources, &mut inputs, now_ms).await?;
    watch_plugins(&sources, &mut inputs, now_ms);
    Ok(inputs)
}

/// `discovered(policy, host)` is what the policy's source selects on the host, `None` while that is unknown.
pub async fn gather_with(
    state: &AppState,
    now_ms: i64,
    discovered: impl Fn(&BackupPolicy, &str) -> Option<Discovered>,
) -> anyhow::Result<Inputs> {
    let agents = load_agents(state).await?;
    let (policies, unreadable_policies) = load_policies(state).await?;
    let successes = last_successes(state).await?;
    let failures = last_failures(state).await?;
    let gone = gone_moments(state).await?;
    let mut snapshots = last_snapshots(state).await?;
    let no_gone = HashMap::new();
    let now = Timestamp::from_millisecond(now_ms).context("the clock is out of range")?;
    let limits: Vec<_> = policies
        .iter()
        .map(|p| overdue_limit(&p.policy.spec, now))
        .collect();
    let hosts = agents
        .into_iter()
        .map(|agent| {
            let id = agent.row.id.clone();
            let pairs = policies
                .iter()
                .zip(&limits)
                .filter(|(p, _)| agent.row.applies(&p.repository_id, &p.policy.spec.selector))
                .flat_map(|(p, limit)| {
                    let key = (p.name.clone(), id.clone());
                    let known = successes.get(&key).map_or(&[][..], Vec::as_slice);
                    let known_refs: Vec<String> = known.iter().map(|(r, ..)| r.clone()).collect();
                    let live = live_failures(&p.policy, failures.get(&key), known);
                    let failed: Vec<String> = live.keys().cloned().collect();
                    let found = discovered(&p.policy, &id);
                    let expected =
                        expected_instances(&p.policy, found.as_ref(), &known_refs, &failed);
                    instance_facts(
                        &p.name,
                        *limit,
                        agent.enrolled_at.max(p.created_at),
                        &expected,
                        known,
                        &live,
                        gone.get(&key).unwrap_or(&no_gone),
                    )
                })
                .collect();
            HostFacts {
                online: state.registry.is_online(&id),
                last_snapshot: snapshots.remove(&id),
                host_id: id,
                hostname: agent.row.hostname,
                labels: agent.row.labels,
                platform: agent.platform,
                pairs,
                last_seen_ms: agent.last_seen_ms,
                cert_not_after_ms: agent.cert_not_after_ms,
            }
        })
        .collect();
    let server_cert = match crate::grpc::server_cert_not_after(state) {
        Ok(cert) => Some(cert),
        Err(error) => {
            tracing::warn!(%error, "cannot read the server gRPC certificate");
            None
        }
    };
    let check_failures = check_failures(state).await?;
    let update_failures = update_failures(state).await?;
    let quarantines = quarantines(state).await?;
    let unavailable = unavailable_repositories(state).await?;
    let security = security_alerts(state).await?;
    let guarantees = guarantees(state).await?;
    Ok(Inputs {
        hosts,
        server_cert,
        check_failures,
        update_failures,
        quarantines,
        unavailable,
        security,
        unreadable_policies,
        guarantees,
        daily_recheck: state.config.schedule_tick_secs > 0,
        ..Inputs::default()
    })
}

fn parse_mismatch(json: &str) -> (String, String) {
    let field = |v: &serde_json::Value, k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or("unknown")
            .to_string()
    };
    let v = serde_json::from_str::<serde_json::Value>(json).unwrap_or_default();
    (field(&v, "pinned"), field(&v, "offered"))
}

fn agent_cannot_write(checks_json: Option<&str>) -> bool {
    checks_json
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .and_then(|v| v.as_array().cloned())
        .is_some_and(|checks| {
            checks.iter().any(|c| {
                c.get("id").and_then(|v| v.as_str()) == Some("agent_delete")
                    && c.get("status").and_then(|v| v.as_str()) == Some("fail")
            })
        })
}

async fn guarantees(state: &AppState) -> anyhow::Result<Vec<RepositoryGuarantee>> {
    let rows = sqlx::query(
        "SELECT id, name, guarantee_level, guarantee_accepted, guarantee_unchecked_runs, \
         guarantee_failed_runs, guarantee_checked_at, guarantee_tick_at, created_at, \
         host_key_mismatch_json, checks_json FROM repositories ORDER BY name, id",
    )
    .fetch_all(&state.db.pool)
    .await
    .context("reading repository guarantees")?;
    rows.iter()
        .map(|row| {
            let checked: Option<i64> = row.try_get("guarantee_checked_at")?;
            Ok(RepositoryGuarantee {
                repository_id: row.try_get("id")?,
                repository: row.try_get("name")?,
                level: row.try_get("guarantee_level")?,
                accepted: row.try_get("guarantee_accepted")?,
                unchecked_runs: u64::try_from(row.try_get::<i64, _>("guarantee_unchecked_runs")?)
                    .unwrap_or(0),
                failed_runs: u64::try_from(row.try_get::<i64, _>("guarantee_failed_runs")?)
                    .unwrap_or(0),
                measured_at_ms: match checked {
                    Some(at) => at,
                    None => row.try_get("created_at")?,
                },
                tick_at_ms: row.try_get("guarantee_tick_at")?,
                host_key_mismatch: row
                    .try_get::<Option<String>, _>("host_key_mismatch_json")?
                    .map(|json| parse_mismatch(&json)),
                agent_cannot_write: agent_cannot_write(
                    row.try_get::<Option<String>, _>("checks_json")?.as_deref(),
                ),
            })
        })
        .collect()
}

/// Events live in memory; the still-open alert rows keep a security alert across a restart until their own 24 hours are over.
async fn security_alerts(state: &AppState) -> anyhow::Result<Vec<SecurityAlert>> {
    let mut out = Vec::new();
    for (host_id, events) in state.registry.security_events() {
        if let Some(c) = events.conflict {
            out.push(SecurityAlert {
                host_id: host_id.clone(),
                kind: AlertType::AgentSessionConflict,
                at_ms: c.at_ms,
                facts: AlertFacts {
                    ip: c.ip.map(|ip| ip.to_string()),
                    previous_ip: c.previous_ip.map(|ip| ip.to_string()),
                    ..AlertFacts::default()
                },
            });
        }
        if let Some(r) = events.rejected {
            out.push(SecurityAlert {
                host_id,
                kind: AlertType::AgentCertificateRejected,
                at_ms: r.at_ms,
                facts: AlertFacts {
                    serial: Some(r.serial),
                    ip: r.ip.map(|ip| ip.to_string()),
                    ..AlertFacts::default()
                },
            });
        }
    }
    let rows = sqlx::query(
        "SELECT type, host_id, facts_json, started_at FROM alerts \
         WHERE state = 'firing' AND host_id IS NOT NULL AND type IN ($1, $2)",
    )
    .bind(AlertType::AgentSessionConflict.as_str())
    .bind(AlertType::AgentCertificateRejected.as_str())
    .fetch_all(&state.db.pool)
    .await
    .context("reading the open security alerts")?;
    for row in &rows {
        let kind: String = row.try_get("type")?;
        let host_id: String = row.try_get("host_id")?;
        let Some(kind) = AlertType::parse(&kind) else {
            continue;
        };
        if out.iter().any(|s| s.host_id == host_id && s.kind == kind) {
            continue;
        }
        let facts: String = row.try_get("facts_json")?;
        out.push(SecurityAlert {
            host_id,
            kind,
            at_ms: row.try_get("started_at")?,
            facts: serde_json::from_str(&facts).unwrap_or_default(),
        });
    }
    Ok(out)
}

async fn quarantines(state: &AppState) -> anyhow::Result<Vec<Quarantine>> {
    let rows = sqlx::query(
        "SELECT q.repository_id, r.name AS repository, q.object_key, q.reason \
         FROM gc_quarantine q JOIN repositories r ON r.id = q.repository_id \
         WHERE q.confirmed_at IS NULL \
         ORDER BY q.repository_id, q.detected_at, q.snapshot_id",
    )
    .fetch_all(&state.db.pool)
    .await
    .context("reading the snapshot quarantine")?;
    let mut out: Vec<Quarantine> = Vec::new();
    for row in &rows {
        let repository_id: String = row.try_get("repository_id")?;
        match out.last_mut() {
            Some(last) if last.repository_id == repository_id => last.count += 1,
            _ => out.push(Quarantine {
                repository_id,
                repository: row.try_get("repository")?,
                count: 1,
                first_key: row.try_get("object_key")?,
                first_reason: row.try_get("reason")?,
            }),
        }
    }
    Ok(out)
}

async fn unavailable_repositories(state: &AppState) -> anyhow::Result<Vec<UnavailableRepository>> {
    let rows = sqlx::query(
        "SELECT id, name, open_error FROM repositories WHERE open_error IS NOT NULL ORDER BY name, id",
    )
    .fetch_all(&state.db.pool)
    .await
    .context("reading unavailable repositories")?;
    rows.iter()
        .map(|row| {
            Ok(UnavailableRepository {
                repository_id: row.try_get("id")?,
                repository: row.try_get("name")?,
                error: row.try_get("open_error")?,
            })
        })
        .collect()
}

pub(crate) async fn update_failures(state: &AppState) -> anyhow::Result<Vec<UpdateFailure>> {
    let rows = sqlx::query(
        "SELECT u.host_id, u.reason, u.target_version, a.hostname, a.version \
         FROM agent_updates u JOIN agents a ON a.id = u.host_id JOIN ( \
             SELECT host_id, MAX(finished_at) AS at FROM agent_updates \
             WHERE state IN ('succeeded', 'rolled_back') AND finished_at IS NOT NULL \
             GROUP BY host_id \
         ) l ON l.host_id = u.host_id AND l.at = u.finished_at \
         WHERE u.state = 'rolled_back' AND a.status <> 'revoked' \
         ORDER BY a.hostname, u.host_id",
    )
    .fetch_all(&state.db.pool)
    .await
    .context("reading the last agent updates")?;
    let mut out: Vec<UpdateFailure> = Vec::new();
    for row in &rows {
        let host_id: String = row.try_get("host_id")?;
        let version: String = row.try_get("version")?;
        let target: String = row.try_get("target_version")?;
        if reached(&version, &target) || out.iter().any(|seen| seen.host_id == host_id) {
            continue;
        }
        let reason: Option<String> = row.try_get("reason")?;
        out.push(UpdateFailure {
            host_id,
            hostname: row.try_get("hostname")?,
            reason: reason.unwrap_or_else(|| "The update was rolled back.".into()),
        });
    }
    Ok(out)
}

async fn check_failures(state: &AppState) -> anyhow::Result<Vec<CheckFailure>> {
    let rows = sqlx::query(
        "SELECT c.id, c.repository_id, c.level, c.state, c.findings_total, c.error, \
         c.finished_at, r.name AS repository \
         FROM check_runs c JOIN repositories r ON r.id = c.repository_id JOIN ( \
             SELECT repository_id, level, MAX(created_at) AS at FROM check_runs \
             WHERE state IN ('done', 'failed') AND (outcome IS NULL OR outcome <> 'stopped' OR report_json IS NOT NULL) \
             GROUP BY repository_id, level \
         ) l ON l.repository_id = c.repository_id AND l.level = c.level AND l.at = c.created_at \
         WHERE c.state = 'failed' AND (c.outcome IS NULL OR c.outcome <> 'stopped' OR c.report_json IS NOT NULL) \
         ORDER BY r.name, c.level, c.id",
    )
    .fetch_all(&state.db.pool)
    .await
    .context("reading the last check runs")?;
    let mut out: Vec<CheckFailure> = Vec::new();
    for row in &rows {
        let level: String = row.try_get("level")?;
        let Some(level) = CheckLevel::parse(&level) else {
            continue;
        };
        let repository_id: String = row.try_get("repository_id")?;
        if out
            .iter()
            .any(|seen| seen.repository_id == repository_id && seen.level == level)
        {
            continue;
        }
        out.push(CheckFailure {
            repository_id,
            repository: row.try_get("repository")?,
            level,
            run_id: row.try_get("id")?,
            findings: u64::try_from(row.try_get::<i64, _>("findings_total")?).unwrap_or(0),
            error: row.try_get("error")?,
            finished_at: row.try_get("finished_at")?,
        });
    }
    Ok(out)
}

async fn load_agents(state: &AppState) -> anyhow::Result<Vec<Agent>> {
    let rows = sqlx::query(
        "SELECT a.id, a.hostname, a.repository_id, a.labels_json, a.os, a.arch, a.enrolled_at, \
         a.last_seen_at, a.cert_not_after, r.name AS repository_name \
         FROM agents a JOIN repositories r ON r.id = a.repository_id \
         WHERE a.status <> 'revoked' ORDER BY a.hostname, a.id",
    )
    .fetch_all(&state.db.pool)
    .await
    .context("reading agents")?;
    rows.iter()
        .map(|row| {
            let labels_json: String = row.try_get("labels_json")?;
            let os: String = row.try_get("os")?;
            let arch: String = row.try_get("arch")?;
            Ok(Agent {
                row: AgentRow {
                    id: row.try_get("id")?,
                    hostname: row.try_get("hostname")?,
                    repository_id: row.try_get("repository_id")?,
                    repository_name: row.try_get("repository_name")?,
                    labels: serde_json::from_str(&labels_json).context("agent labels")?,
                    facts: Value::Null,
                },
                platform: format!("{os} / {arch}"),
                enrolled_at: row.try_get("enrolled_at")?,
                last_seen_ms: row.try_get("last_seen_at")?,
                cert_not_after_ms: row.try_get("cert_not_after")?,
            })
        })
        .collect()
}

async fn load_policies(state: &AppState) -> anyhow::Result<(Vec<Policy>, Vec<String>)> {
    let rows = sqlx::query("SELECT name, repository_id, spec_json, created_at FROM policies")
        .fetch_all(&state.db.pool)
        .await
        .context("reading policies")?;
    let mut out = Vec::new();
    let mut unreadable = Vec::new();
    for row in &rows {
        let name: String = row.try_get("name")?;
        match serde_json::from_str(&row.try_get::<String, _>("spec_json")?) {
            Ok(policy) => out.push(Policy {
                name,
                repository_id: row.try_get("repository_id")?,
                policy,
                created_at: row.try_get("created_at")?,
            }),
            Err(error) => {
                tracing::warn!(policy = %name, %error, "stored policy cannot be parsed");
                unreadable.push(name);
            }
        }
    }
    Ok((out, unreadable))
}

async fn last_successes(state: &AppState) -> anyhow::Result<HashMap<Pair, Vec<Instance>>> {
    let rows = sqlx::query(
        "SELECT policy_name, host_id, source_ref, MAX(received_at) AS at, \
         MAX(source_label) AS label FROM snapshots \
         WHERE incomplete = 0 AND verified_at IS NOT NULL AND stdin_name IS NULL \
         AND policy_name IS NOT NULL AND host_id IS NOT NULL \
         GROUP BY policy_name, host_id, source_ref",
    )
    .fetch_all(&state.db.pool)
    .await
    .context("reading the last successful backups")?;
    let mut out: HashMap<Pair, Vec<Instance>> = HashMap::new();
    for row in &rows {
        // shortcut: label of any snapshot of the instance, replace with the discovered label in phase 21
        out.entry((row.try_get("policy_name")?, row.try_get("host_id")?))
            .or_default()
            .push((
                row.try_get("source_ref")?,
                row.try_get("at")?,
                row.try_get("label")?,
            ));
    }
    Ok(out)
}

async fn last_failures(
    state: &AppState,
) -> anyhow::Result<HashMap<Pair, HashMap<String, (i64, String)>>> {
    let rows = sqlx::query(
        "SELECT j.policy_name, j.host_id, j.source_ref, j.state, j.finished_at, j.error_message \
         FROM jobs j JOIN ( \
             SELECT policy_name, host_id, source_ref, MAX(finished_at) AS at FROM jobs \
             WHERE kind = 'backup' AND policy_name IS NOT NULL AND finished_at IS NOT NULL \
             GROUP BY policy_name, host_id, source_ref \
         ) l ON l.policy_name = j.policy_name AND l.host_id = j.host_id \
             AND l.source_ref = j.source_ref AND l.at = j.finished_at \
         WHERE j.kind = 'backup'",
    )
    .fetch_all(&state.db.pool)
    .await
    .context("reading the last backup jobs")?;
    let mut out: HashMap<Pair, HashMap<String, (i64, String)>> = HashMap::new();
    for row in &rows {
        let job_state: String = row.try_get("state")?;
        let message: Option<String> = row.try_get("error_message")?;
        let message = match job_state.as_str() {
            "failed" => message.unwrap_or_else(|| "The backup failed.".into()),
            "interrupted" => message.unwrap_or_else(|| "The backup was interrupted.".into()),
            _ => continue,
        };
        out.entry((row.try_get("policy_name")?, row.try_get("host_id")?))
            .or_default()
            .entry(row.try_get("source_ref")?)
            .or_insert((row.try_get("finished_at")?, message));
    }
    Ok(out)
}

async fn gone_moments(state: &AppState) -> anyhow::Result<HashMap<Pair, HashMap<String, Gone>>> {
    let rows = sqlx::query(
        "SELECT policy_name, host_id, source_ref, gone_at, returned_at FROM source_instances_gone",
    )
    .fetch_all(&state.db.pool)
    .await
    .context("reading when source instances stopped being expected")?;
    let mut out: HashMap<Pair, HashMap<String, Gone>> = HashMap::new();
    for row in &rows {
        out.entry((row.try_get("policy_name")?, row.try_get("host_id")?))
            .or_default()
            .insert(
                row.try_get("source_ref")?,
                (row.try_get("gone_at")?, row.try_get("returned_at")?),
            );
    }
    Ok(out)
}

/// Remembers when the policy stopped expecting an instance, once, so the gone event starts an hour later and a restart does not move it. A return before the event, or after its window, forgets the row; a return inside the window keeps it, marked, so a flapping instance raises one event a day.
// shortcut: rows of deleted policies and hosts stay; prune them if the table ever grows
pub async fn record_gone(
    state: &AppState,
    hosts: &mut [HostFacts],
    now_ms: i64,
) -> anyhow::Result<()> {
    const CONTEXT: &str = "recording a source instance that is no longer expected";
    for host in hosts {
        for pair in &mut host.pairs {
            match (pair.expected, pair.gone_at_ms) {
                (false, None) => {
                    sqlx::query(
                        "INSERT INTO source_instances_gone (policy_name, host_id, source_ref, gone_at) \
                         VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
                    )
                    .bind(&pair.policy)
                    .bind(&host.host_id)
                    .bind(&pair.source_ref)
                    .bind(now_ms)
                    .execute(&state.db.pool)
                    .await
                    .context(CONTEXT)?;
                    pair.gone_at_ms = Some(now_ms);
                }
                (true, Some(gone_at)) => {
                    let opens = gone_at + GONE_AFTER_MS;
                    if now_ms < opens || now_ms >= opens + GONE_WINDOW_MS {
                        sqlx::query(
                            "DELETE FROM source_instances_gone \
                             WHERE policy_name = $1 AND host_id = $2 AND source_ref = $3",
                        )
                        .bind(&pair.policy)
                        .bind(&host.host_id)
                        .bind(&pair.source_ref)
                        .execute(&state.db.pool)
                        .await
                        .context(CONTEXT)?;
                        pair.gone_at_ms = None;
                        pair.returned_at_ms = None;
                    } else if pair.returned_at_ms.is_none() {
                        sqlx::query(
                            "UPDATE source_instances_gone SET returned_at = $1 \
                             WHERE policy_name = $2 AND host_id = $3 AND source_ref = $4 \
                             AND returned_at IS NULL",
                        )
                        .bind(now_ms)
                        .bind(&pair.policy)
                        .bind(&host.host_id)
                        .bind(&pair.source_ref)
                        .execute(&state.db.pool)
                        .await
                        .context(CONTEXT)?;
                        pair.returned_at_ms = Some(now_ms);
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

// shortcut: the newest snapshot of any source of the host; split it per source when other sources back up (phase 22).
async fn last_snapshots(state: &AppState) -> anyhow::Result<HashMap<String, SnapshotFacts>> {
    let rows = sqlx::query(
        "SELECT s.host_id, s.received_at, s.stats_json FROM snapshots s JOIN ( \
             SELECT host_id, MAX(received_at) AS at FROM snapshots \
             WHERE incomplete = 0 AND verified_at IS NOT NULL AND host_id IS NOT NULL \
             GROUP BY host_id \
         ) l ON l.host_id = s.host_id AND l.at = s.received_at \
         WHERE s.incomplete = 0 AND s.verified_at IS NOT NULL",
    )
    .fetch_all(&state.db.pool)
    .await
    .context("reading the last snapshots")?;
    let mut out = HashMap::new();
    for row in &rows {
        let stats: Value =
            serde_json::from_str(&row.try_get::<String, _>("stats_json")?).unwrap_or(Value::Null);
        out.entry(row.try_get("host_id")?).or_insert(SnapshotFacts {
            time_ms: row.try_get("received_at")?,
            bytes_total: stats["bytes_total"].as_u64().unwrap_or(0),
            bytes_added: stats["bytes_added"].as_u64().unwrap_or(0),
        });
    }
    Ok(out)
}

/// The list of a host that is fresh and not cut short; anything else says nothing about absence.
fn complete_list<'a>(
    sources: &'a HashMap<String, AgentSources>,
    host_id: &str,
    now_ms: i64,
) -> Option<&'a Stored> {
    sources
        .get(host_id)?
        .stored
        .as_ref()
        .filter(|s| is_fresh(s.collected_at, now_ms) && !s.discovery.truncated)
}

struct Past {
    at: Option<i64>,
    policy: Option<String>,
    label: Option<String>,
}

/// The instances of a host that have a snapshot or a backup job, with their newest snapshot.
async fn guest_past(state: &AppState, host_id: &str) -> anyhow::Result<HashMap<String, Past>> {
    let mut out = HashMap::new();
    let rows = sqlx::query(
        "SELECT s.source_ref, s.policy_name, s.source_label, s.received_at FROM snapshots s JOIN ( \
             SELECT source_ref, MAX(received_at) AS at FROM snapshots \
             WHERE host_id = $1 AND source_ref <> '' GROUP BY source_ref \
         ) l ON l.source_ref = s.source_ref AND l.at = s.received_at WHERE s.host_id = $1",
    )
    .bind(host_id)
    .fetch_all(&state.db.pool)
    .await
    .context("reading the snapshots of the instances of a host")?;
    for row in &rows {
        out.entry(row.try_get::<String, _>("source_ref")?)
            .or_insert(Past {
                at: Some(row.try_get("received_at")?),
                policy: row.try_get("policy_name")?,
                label: row.try_get("source_label")?,
            });
    }
    let rows = sqlx::query(
        "SELECT DISTINCT source_ref FROM jobs \
         WHERE host_id = $1 AND kind = 'backup' AND source_ref <> ''",
    )
    .bind(host_id)
    .fetch_all(&state.db.pool)
    .await
    .context("reading the backup jobs of the instances of a host")?;
    for row in &rows {
        out.entry(row.try_get::<String, _>("source_ref")?)
            .or_insert(Past {
                at: None,
                policy: None,
                label: None,
            });
    }
    Ok(out)
}

fn guest_label(guest: &Guest) -> String {
    let prefix = match guest.kind {
        GuestType::Vm => "VM",
        GuestType::Ct => "CT",
    };
    match guest.name.as_deref().filter(|name| !name.is_empty()) {
        Some(name) => format!("{prefix} {} · {name}", guest.vmid),
        None => format!("{prefix} {}", guest.vmid),
    }
}

/// A guest alert follows the stored row of the guest. A fresh, complete list adds the rows of guests that were backed up once and that no policy of the host selects, and removes the rows of guests that are selected again or gone; any other list, and a policy that cannot be read, leaves the rows as they are.
async fn watch_guests(
    state: &AppState,
    sources: &HashMap<String, AgentSources>,
    inputs: &mut Inputs,
    now_ms: i64,
) -> anyhow::Result<()> {
    let rows = sqlx::query("SELECT host_id, source_ref, since FROM guests_unprotected")
        .fetch_all(&state.db.pool)
        .await
        .context("reading the unprotected guests")?;
    let mut tracked: HashMap<String, HashMap<String, i64>> = HashMap::new();
    for row in &rows {
        tracked
            .entry(row.try_get("host_id")?)
            .or_default()
            .insert(row.try_get("source_ref")?, row.try_get("since")?);
    }
    let (policies, unreadable) = load_policies(state).await?;
    for agent in load_agents(state).await? {
        let host = agent.row.id.as_str();
        let stored = tracked.remove(host).unwrap_or_default();
        let list = complete_list(sources, host, now_ms).filter(|_| unreadable.is_empty());
        let unselected: Vec<&Guest> = list
            .and_then(|s| s.discovery.proxmox.as_ref())
            .map_or(&[][..], |p| p.guests.as_slice())
            .iter()
            .filter(|g| {
                !policies
                    .iter()
                    .filter(|p| agent.row.applies(&p.repository_id, &p.policy.spec.selector))
                    .filter_map(|p| source::normalized(&p.policy.spec.source))
                    .any(|source| selects_guest(source, g))
            })
            .collect();
        if unselected.is_empty() && stored.is_empty() {
            continue;
        }
        let past = guest_past(state, host).await?;
        let mut push = |source_ref: &str, label: Option<String>| {
            let known = past.get(source_ref);
            inputs.unprotected.push(UnprotectedGuest {
                host_id: host.to_owned(),
                source_ref: source_ref.to_owned(),
                label: label.or_else(|| known.and_then(|k| k.label.clone())),
                last_snapshot_ms: known.and_then(|k| k.at),
                policy: known.and_then(|k| k.policy.clone()),
                since_ms: stored.get(source_ref).copied(),
            });
        };
        if list.is_some() {
            let now_unprotected: Vec<&Guest> = unselected
                .into_iter()
                .filter(|g| past.contains_key(&g.key))
                .collect();
            for guest in &now_unprotected {
                push(&guest.key, Some(guest_label(guest)));
            }
            inputs.unprotected_gone.extend(
                stored
                    .keys()
                    .filter(|r| !now_unprotected.iter().any(|g| &g.key == *r))
                    .map(|r| (host.to_owned(), r.clone())),
            );
        } else {
            for source_ref in stored.keys() {
                push(source_ref, None);
            }
        }
    }
    Ok(())
}

/// Plugin state of the Proxmox nodes. The facts of a cluster come from its hosts with a fresh, complete list; the host with the smallest node name carries the alert of a node that has no agent of its own.
fn watch_plugins(sources: &HashMap<String, AgentSources>, inputs: &mut Inputs, now_ms: i64) {
    let mut scopes: BTreeMap<&str, Vec<(String, &str, &ProxmoxNode)>> = BTreeMap::new();
    for host in &inputs.hosts {
        let Some(node) = complete_list(sources, &host.host_id, now_ms)
            .and_then(|s| s.discovery.proxmox.as_ref())
        else {
            continue;
        };
        let own = match (node.node.as_str(), node.scope) {
            ("", ScopeKind::Node) => node.name.clone(),
            (own, _) => own.to_owned(),
        };
        scopes
            .entry(node.name.as_str())
            .or_default()
            .push((own, host.host_id.as_str(), node));
    }
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    for (scope, mut hosts) in scopes {
        seen.insert(scope.to_owned());
        hosts.sort_by(|a, b| (a.0.is_empty(), &a.0, a.1).cmp(&(b.0.is_empty(), &b.0, b.1)));
        let carrier = hosts[0].1;
        let view = hosts[0].2;
        let mut outdated: BTreeMap<&str, bool> = BTreeMap::new();
        for node in view.nodes.iter().filter(|n| n.plugin_marker.is_empty()) {
            outdated.insert(&node.name, false);
        }
        for (own, _, node) in hosts.iter().filter(|(own, ..)| !own.is_empty()) {
            if !node.plugin_installed {
                outdated.insert(own, false);
            }
        }
        for (own, _, node) in hosts.iter().filter(|(own, ..)| !own.is_empty()) {
            if node.plugin_installed && !node.plugin_current {
                outdated.entry(own).or_insert(true);
            }
        }
        found.extend(outdated.into_iter().map(|(node, outdated)| PluginFinding {
            host_id: carrier.to_owned(),
            scope: scope.to_owned(),
            in_cluster: view.scope == ScopeKind::Cluster,
            node: node.to_owned(),
            outdated,
        }));
    }
    inputs.plugins = found;
    inputs.plugin_scopes = seen;
}

/// Stores when a guest was first seen unprotected, so a restart does not move the hour, and forgets the guests that left the state.
pub async fn record_unprotected(
    state: &AppState,
    inputs: &mut Inputs,
    now_ms: i64,
) -> anyhow::Result<()> {
    const CONTEXT: &str = "recording an unprotected guest";
    for (host_id, source_ref) in &inputs.unprotected_gone {
        sqlx::query("DELETE FROM guests_unprotected WHERE host_id = $1 AND source_ref = $2")
            .bind(host_id)
            .bind(source_ref)
            .execute(&state.db.pool)
            .await
            .context(CONTEXT)?;
    }
    for guest in inputs
        .unprotected
        .iter_mut()
        .filter(|g| g.since_ms.is_none())
    {
        sqlx::query(
            "INSERT INTO guests_unprotected (host_id, source_ref, since) VALUES ($1, $2, $3) \
             ON CONFLICT DO NOTHING",
        )
        .bind(&guest.host_id)
        .bind(&guest.source_ref)
        .bind(now_ms)
        .execute(&state.db.pool)
        .await
        .context(CONTEXT)?;
        guest.since_ms = Some(now_ms);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::agent_cannot_write;

    #[test]
    fn agent_cannot_write_is_read_from_the_checks() {
        let check = |status: &str| {
            format!(
                r#"[{{"id":"write","status":"pass"}},{{"id":"agent_delete","status":"{status}"}}]"#
            )
        };
        assert!(agent_cannot_write(Some(&check("fail"))));
        for json in [
            Some(check("pass")),
            Some(check("neutral")),
            Some(r#"[{"id":"write","status":"fail"}]"#.to_string()),
            Some("[]".to_string()),
            Some("not json".to_string()),
            Some(r#"{"id":"agent_delete","status":"fail"}"#.to_string()),
            None,
        ] {
            assert!(!agent_cannot_write(json.as_deref()), "{json:?}");
        }
    }
}
