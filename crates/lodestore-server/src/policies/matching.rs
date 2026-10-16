use lodestore_policy::{BackupPolicy, Selector};
use serde::Serialize;
use serde_json::Value;
use sqlx::Row;
use utoipa::ToSchema;

use crate::error::ApiError;
use crate::hosts::labels::Label;
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

#[derive(Serialize, ToSchema)]
pub struct MatchHost {
    pub id: String,
    pub hostname: String,
    pub online: bool,
    pub repository_id: String,
    pub repository_name: String,
}

#[derive(Serialize, ToSchema)]
pub struct Matching {
    pub hosts: Vec<MatchHost>,
    pub other_repository: Vec<MatchHost>,
}

fn match_host(state: &AppState, agent: &AgentRow) -> MatchHost {
    MatchHost {
        id: agent.id.clone(),
        hostname: agent.hostname.clone(),
        online: state.registry.is_online(&agent.id),
        repository_id: agent.repository_id.clone(),
        repository_name: agent.repository_name.clone(),
    }
}

pub fn matching(
    state: &AppState,
    agents: &[AgentRow],
    repository_id: &str,
    selector: &Selector,
) -> Matching {
    let mut out = Matching {
        hosts: Vec::new(),
        other_repository: Vec::new(),
    };
    for agent in agents.iter().filter(|a| a.selected_by(selector)) {
        let host = match_host(state, agent);
        if agent.repository_id == repository_id {
            out.hosts.push(host);
        } else {
            out.other_repository.push(host);
        }
    }
    out
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
