use std::collections::HashMap;

use lodestore_policy::source::Source;
use lodestore_policy::{BackupPolicy, Selector};
use serde::Serialize;
use serde_json::Value;
use sqlx::Row;
use utoipa::ToSchema;

use crate::error::ApiError;
use crate::hosts::labels::Label;
use crate::sources::select::{Selection, select};
use crate::sources::{AgentSources, SOURCE_TYPES, declares};
use crate::state::AppState;

pub struct AgentRow {
    pub id: String,
    pub hostname: String,
    pub repository_id: String,
    pub repository_name: String,
    pub labels: Vec<Label>,
    pub facts: Value,
}

impl AgentRow {
    pub fn selected_by(&self, selector: &Selector) -> bool {
        selector.matches(
            &self.id,
            self.labels
                .iter()
                .map(|l| (l.key.as_str(), l.value.as_str())),
        )
    }

    pub fn applies(&self, repository_id: &str, selector: &Selector) -> bool {
        self.repository_id == repository_id && self.selected_by(selector)
    }
}

pub async fn load_agents(state: &AppState) -> Result<Vec<AgentRow>, ApiError> {
    let rows = sqlx::query(
        "SELECT a.id, a.hostname, a.repository_id, a.labels_json, a.facts_json, \
         r.name AS repository_name FROM agents a JOIN repositories r ON r.id = a.repository_id \
         WHERE a.status <> 'revoked' ORDER BY a.hostname, a.id",
    )
    .fetch_all(&state.db.pool)
    .await?;
    rows.iter()
        .map(|row| {
            let labels_json: String = row.try_get("labels_json")?;
            let facts_json: Option<String> = row.try_get("facts_json")?;
            Ok(AgentRow {
                id: row.try_get("id")?,
                hostname: row.try_get("hostname")?,
                repository_id: row.try_get("repository_id")?,
                repository_name: row.try_get("repository_name")?,
                labels: serde_json::from_str(&labels_json)
                    .map_err(|e| ApiError::Internal(e.into()))?,
                facts: facts_json
                    .and_then(|text| serde_json::from_str(&text).ok())
                    .unwrap_or(Value::Null),
            })
        })
        .collect()
}

/// Why a job of this policy will fail on the host, from what the agent reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SourceMark {
    /// The agent does not declare support for the source type.
    AgentTooOld,
    /// The host has no source of the type.
    NotFound,
    /// The host has the type but the policy selects none of it.
    SelectsNothing,
}

#[derive(Serialize, ToSchema)]
pub struct MatchHost {
    pub id: String,
    pub hostname: String,
    pub online: bool,
    pub repository_id: String,
    pub repository_name: String,
    /// How many found instances the policy selects on the host; null when unknown, stale or marked.
    pub instances: Option<u32>,
    pub source_mark: Option<SourceMark>,
}

#[derive(Serialize, ToSchema)]
pub struct Matching {
    pub hosts: Vec<MatchHost>,
    pub other_repository: Vec<MatchHost>,
}

fn source_state(
    source: &Source,
    host: Option<&AgentSources>,
    now_ms: i64,
) -> (Option<u32>, Option<SourceMark>) {
    let capabilities = host.map_or(&[][..], |h| h.capabilities.as_slice());
    if !declares(capabilities, source.type_name()) {
        return (None, Some(SourceMark::AgentTooOld));
    }
    let stored = host.and_then(|h| h.stored.as_ref());
    match select(source, stored, now_ms) {
        Selection::Unknown => (None, None),
        Selection::NoSource => {
            let guests = matches!(source, Source::ProxmoxVm(_) | Source::ProxmoxCt(_));
            let node = stored.is_some_and(|s| s.discovery.proxmox.is_some());
            let mark = if guests && node {
                SourceMark::SelectsNothing
            } else {
                SourceMark::NotFound
            };
            (None, Some(mark))
        }
        Selection::Nothing => (None, Some(SourceMark::SelectsNothing)),
        Selection::Some { keys, .. } => (u32::try_from(keys.len()).ok(), None),
    }
}

fn match_host(
    state: &AppState,
    agent: &AgentRow,
    source: Option<(&Source, &HashMap<String, AgentSources>)>,
    now_ms: i64,
) -> MatchHost {
    let (instances, source_mark) = source.map_or((None, None), |(source, sources)| {
        source_state(source, sources.get(&agent.id), now_ms)
    });
    MatchHost {
        id: agent.id.clone(),
        hostname: agent.hostname.clone(),
        online: state.registry.is_online(&agent.id),
        repository_id: agent.repository_id.clone(),
        repository_name: agent.repository_name.clone(),
        instances,
        source_mark,
    }
}

/// `source` is the policy's source unless it is files; only hosts of the policy's repository
/// are marked, the others stay null.
pub fn matching(
    state: &AppState,
    agents: &[AgentRow],
    repository_id: &str,
    selector: &Selector,
    source: Option<&Source>,
    sources: &HashMap<String, AgentSources>,
    now_ms: i64,
) -> Matching {
    let mut out = Matching {
        hosts: Vec::new(),
        other_repository: Vec::new(),
    };
    for agent in agents.iter().filter(|a| a.selected_by(selector)) {
        if agent.repository_id == repository_id {
            let marked = source.map(|source| (source, sources));
            out.hosts.push(match_host(state, agent, marked, now_ms));
        } else {
            out.other_repository
                .push(match_host(state, agent, None, now_ms));
        }
    }
    out
}

/// How many of the hosts a policy takes can run a source type.
#[derive(Serialize, ToSchema)]
pub struct SourceAvailability {
    pub source_type: String,
    /// Hosts of the repository that the selector takes.
    pub hosts_total: u32,
    /// Of those, the hosts where the agent found the type.
    pub hosts_with: u32,
    /// Of those, the hosts whose agent does not declare support for the type.
    pub hosts_agent_outdated: u32,
}

/// The types the form may offer: every type some host reported, plus the draft's own.
pub fn availability(
    agents: &[AgentRow],
    sources: &HashMap<String, AgentSources>,
    repository_id: &str,
    selector: &Selector,
    own_type: Option<&str>,
) -> Vec<SourceAvailability> {
    let found = |agent: &AgentRow, source_type: &str| {
        sources
            .get(&agent.id)
            .and_then(|s| s.stored.as_ref())
            .is_some_and(|s| s.discovery.has(source_type))
    };
    let taken: Vec<&AgentRow> = agents
        .iter()
        .filter(|a| a.applies(repository_id, selector))
        .collect();
    let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    SOURCE_TYPES
        .into_iter()
        .filter(|t| Some(*t) == own_type || agents.iter().any(|a| found(a, t)))
        .map(|source_type| {
            let with: Vec<&&AgentRow> = taken.iter().filter(|a| found(a, source_type)).collect();
            let outdated = with
                .iter()
                .filter(|a| {
                    let caps = sources.get(&a.id).map_or(&[][..], |s| &s.capabilities[..]);
                    !declares(caps, source_type)
                })
                .count();
            SourceAvailability {
                source_type: source_type.to_string(),
                hosts_total: count(taken.len()),
                hosts_with: count(with.len()),
                hosts_agent_outdated: count(outdated),
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub enum HookWarningKind {
    #[serde(rename = "script_missing")]
    ScriptMissing,
    #[serde(rename = "script_rejected")]
    ScriptRejected,
    #[serde(rename = "inline_disabled")]
    InlineDisabled,
}

#[derive(Serialize, ToSchema)]
pub struct HookWarning {
    pub host_id: String,
    pub hostname: String,
    pub kind: HookWarningKind,
    pub script: Option<String>,
}

pub fn hook_warnings(policy: &BackupPolicy, agents: &[&AgentRow]) -> Vec<HookWarning> {
    let hooks = policy
        .spec
        .hooks
        .before
        .iter()
        .chain(&policy.spec.hooks.after);
    let mut out: Vec<HookWarning> = Vec::new();
    for agent in agents {
        let Some(facts) = agent.facts.get("hooks").filter(|h| h.is_object()) else {
            continue;
        };
        let allow_inline = facts["allow_inline"].as_bool().unwrap_or(false);
        let scripts = facts["scripts"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for hook in hooks.clone() {
            let (kind, script) = match (&hook.script, &hook.command) {
                (Some(name), _) => {
                    match scripts.iter().find(|s| s["name"].as_str() == Some(name)) {
                        None => (HookWarningKind::ScriptMissing, Some(name.clone())),
                        Some(s) if s["ok"].as_bool() != Some(true) => {
                            (HookWarningKind::ScriptRejected, Some(name.clone()))
                        }
                        Some(_) => continue,
                    }
                }
                (None, Some(_)) if !allow_inline => (HookWarningKind::InlineDisabled, None),
                _ => continue,
            };
            let seen = out
                .iter()
                .any(|w| w.host_id == agent.id && w.kind == kind && w.script == script);
            if !seen {
                out.push(HookWarning {
                    host_id: agent.id.clone(),
                    hostname: agent.hostname.clone(),
                    kind,
                    script,
                });
            }
        }
    }
    out
}
