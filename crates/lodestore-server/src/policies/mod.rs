pub mod apply;
pub mod matching;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use lodestore_policy::jiff::Timestamp;
use lodestore_policy::schedule::DstNote;
use lodestore_policy::{
    BackupPolicy, Hooks, Problem, Retention, Schedule, parse_yaml, to_yaml, validate,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::any::AnyRow;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use self::matching::{AgentRow, HookWarning, Matching, hook_warnings, load_agents, matching};
use crate::auth::tokens::{JobsRun, PoliciesRead, PoliciesWrite, Principal, Scope, Scoped};
use crate::db::now_ms;
use crate::error::{ApiError, ErrorBody};
use crate::hosts::labels::Label;
use crate::jobs::{ACTIVE_STATES, JobBrief, RunResult, enqueue, job_spec, last_job};
use crate::state::AppState;

pub(crate) const RETENTION_PURPOSE: &str = "set or change retention";

pub(crate) fn retention_of(policy: &BackupPolicy) -> Option<&Retention> {
    policy.spec.retention.as_ref().filter(|r| !r.is_empty())
}

pub(crate) fn hooks_purpose(names: &[String]) -> String {
    format!(
        "change hooks or the hosts they run on (policies: {})",
        names.join(", ")
    )
}

pub(crate) fn gains_hook_hosts(
    old: Option<(&str, &BackupPolicy)>,
    new: (&str, &BackupPolicy),
    agents: &[AgentRow],
) -> bool {
    let (new_repository, new_policy) = new;
    let no_hooks = Hooks::default();
    let old_hooks = old.map_or(&no_hooks, |(_, policy)| &policy.spec.hooks);
    if *old_hooks != new_policy.spec.hooks {
        return true;
    }
    let Some((old_repository, old_policy)) = old else {
        return false;
    };
    if new_policy.spec.hooks == no_hooks {
        return false;
    }
    agents.iter().any(|a| {
        a.applies(new_repository, &new_policy.spec.selector)
            && !a.applies(old_repository, &old_policy.spec.selector)
    })
}

const RESERVED_NAMES: [&str; 2] = ["validate", "apply"];
const NEXT_RUNS: usize = 3;
const DST_HORIZON_DAYS: u32 = 366;

#[derive(Deserialize, ToSchema)]
pub struct ValidateRequest {
    pub yaml: Option<String>,
    pub policy: Option<BackupPolicy>,
    pub original_name: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct ValidateResponse {
    pub valid: bool,
    pub policy: Option<BackupPolicy>,
    pub yaml: Option<String>,
    pub errors: Vec<Problem>,
    pub name_taken: bool,
    pub matching: Option<Matching>,
    pub warnings: Vec<HookWarning>,
    pub schedule: Option<ScheduleInfo>,
}

#[derive(Serialize, ToSchema)]
pub struct ScheduleInfo {
    pub summary: String,
    pub timezone: String,
    pub jitter_secs: u64,
    pub catch_up_secs: u64,
    pub next_runs: Vec<i64>,
    pub dst_notes: Vec<DstNote>,
}

#[derive(Deserialize, ToSchema)]
pub struct PolicyInput {
    pub yaml: Option<String>,
    pub policy: Option<BackupPolicy>,
}

#[derive(Deserialize, ToSchema)]
pub struct PolicyUpdate {
    pub base_revision: i64,
    pub yaml: Option<String>,
    pub policy: Option<BackupPolicy>,
}

#[derive(Serialize, ToSchema)]
pub struct PolicySummary {
    pub name: String,
    pub repository_id: String,
    pub repository_name: String,
    pub match_labels: Vec<Label>,
    pub host_list: bool,
    pub hosts: i64,
    pub other_repository_hosts: i64,
    pub last_job: Option<JobBrief>,
    pub managed_by: Option<String>,
    pub schedule: Option<ScheduleInfo>,
}

#[derive(Serialize, ToSchema)]
pub struct PolicyDetail {
    pub name: String,
    pub policy: BackupPolicy,
    pub yaml: String,
    pub revision: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub repository_id: String,
    pub repository_name: String,
    pub matching: Matching,
    pub warnings: Vec<HookWarning>,
    pub managed_by: Option<String>,
    pub schedule: Option<ScheduleInfo>,
}

#[derive(Deserialize, ToSchema)]
pub struct HostRunRequest {
    pub policies: Option<Vec<String>>,
}

#[derive(Serialize, ToSchema)]
pub struct HostPolicy {
    pub name: String,
    pub last_job: Option<JobBrief>,
    pub next_run: Option<i64>,
}

enum Failure {
    Problems(Vec<Problem>),
    Api(ApiError),
}

impl From<ApiError> for Failure {
    fn from(e: ApiError) -> Self {
        Failure::Api(e)
    }
}

impl From<sqlx::Error> for Failure {
    fn from(e: sqlx::Error) -> Self {
        Failure::Api(e.into())
    }
}

impl From<Failure> for ApiError {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Api(e) => e,
            Failure::Problems(problems) => {
                let first = problems.first();
                ApiError::Validation(match first {
                    Some(Problem {
                        line: Some(line),
                        message,
                        ..
                    }) => format!("Line {line}: {message}"),
                    Some(Problem {
                        path: Some(path),
                        message,
                        ..
                    }) => format!("{path}: {message}"),
                    Some(problem) => problem.message.clone(),
                    None => "The policy is invalid.".to_string(),
                })
            }
        }
    }
}

struct Checked {
    policy: BackupPolicy,
    yaml: String,
    repository_id: String,
}

fn problem(path: &str, message: String) -> Problem {
    Problem {
        line: None,
        column: None,
        path: Some(path.to_string()),
        message,
    }
}

async fn check(
    state: &AppState,
    yaml: Option<String>,
    policy: Option<BackupPolicy>,
) -> Result<Checked, Failure> {
    let (policy, yaml) = match (yaml, policy) {
        (Some(yaml), _) => (parse_yaml(&yaml).map_err(Failure::Problems)?, yaml),
        (None, Some(policy)) => {
            let problems = validate(&policy);
            if !problems.is_empty() {
                return Err(Failure::Problems(problems));
            }
            let yaml = to_yaml(&policy);
            (policy, yaml)
        }
        (None, None) => {
            return Err(Failure::Problems(vec![Problem {
                line: None,
                column: None,
                path: None,
                message: "Provide the policy as YAML or as an object.".into(),
            }]));
        }
    };
    resolve(state, policy, yaml).await
}

async fn resolve(state: &AppState, policy: BackupPolicy, yaml: String) -> Result<Checked, Failure> {
    let mut problems = Vec::new();
    if RESERVED_NAMES.contains(&policy.metadata.name.as_str()) {
        problems.push(problem(
            "metadata.name",
            format!("The name \"{}\" is reserved.", policy.metadata.name),
        ));
    }
    let repository = sqlx::query("SELECT id FROM repositories WHERE name = $1")
        .bind(&policy.spec.repository)
        .fetch_optional(&state.db.pool)
        .await?;
    let Some(repository) = repository else {
        problems.push(problem(
            "spec.repository",
            format!("Repository \"{}\" does not exist.", policy.spec.repository),
        ));
        return Err(Failure::Problems(problems));
    };
    if !problems.is_empty() {
        return Err(Failure::Problems(problems));
    }
    Ok(Checked {
        repository_id: repository.try_get("id")?,
        policy,
        yaml,
    })
}

struct PolicyRow {
    id: String,
    policy: BackupPolicy,
    yaml: String,
    revision: i64,
    created_at: i64,
    updated_at: i64,
    repository_id: String,
    repository_name: String,
    managed_by: Option<String>,
}

const POLICY_SELECT: &str = "SELECT p.id, p.repository_id, p.yaml, p.spec_json, p.revision, \
    p.created_at, p.updated_at, p.managed_by, r.name AS repository_name \
    FROM policies p JOIN repositories r ON r.id = p.repository_id";

fn policy_from_row(row: &AnyRow) -> Result<PolicyRow, ApiError> {
    Ok(PolicyRow {
        id: row.try_get("id")?,
        policy: serde_json::from_str(&row.try_get::<String, _>("spec_json")?)
            .map_err(|e| ApiError::Internal(e.into()))?,
        yaml: row.try_get("yaml")?,
        revision: row.try_get("revision")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        repository_id: row.try_get("repository_id")?,
        repository_name: row.try_get("repository_name")?,
        managed_by: row.try_get("managed_by")?,
    })
}

async fn find_policy(state: &AppState, name: &str) -> Result<PolicyRow, ApiError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "{POLICY_SELECT} WHERE p.name = $1"
    )))
    .bind(name)
    .fetch_optional(&state.db.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("policy not found".into()))?;
    policy_from_row(&row)
}

pub async fn policy_for_rerun(
    state: &AppState,
    policy_id: &str,
    host_id: &str,
) -> Result<Option<BackupPolicy>, ApiError> {
    let Some(row) = sqlx::query(sqlx::AssertSqlSafe(format!(
        "{POLICY_SELECT} WHERE p.id = $1"
    )))
    .bind(policy_id)
    .fetch_optional(&state.db.pool)
    .await?
    else {
        return Ok(None);
    };
    let row = policy_from_row(&row)?;
    let host = host_agent(state, host_id).await?;
    if !host.applies(&row.repository_id, &row.policy.spec.selector) {
        return Err(ApiError::Conflict(
            "The policy no longer applies to this host.".into(),
        ));
    }
    Ok(Some(row.policy))
}

async fn all_policies(state: &AppState) -> Result<Vec<PolicyRow>, ApiError> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "{POLICY_SELECT} ORDER BY p.name"
    )))
    .fetch_all(&state.db.pool)
    .await?
    .iter()
    .map(policy_from_row)
    .collect()
}

pub(crate) async fn hook_policies_gained_by_labels(
    state: &AppState,
    host_id: &str,
    labels: &[Label],
) -> Result<Vec<String>, ApiError> {
    let Some(before) = load_agents(state)
        .await?
        .into_iter()
        .find(|a| a.id == host_id)
    else {
        return Ok(Vec::new());
    };
    let after = AgentRow {
        id: before.id.clone(),
        hostname: before.hostname.clone(),
        repository_id: before.repository_id.clone(),
        repository_name: before.repository_name.clone(),
        labels: labels.to_vec(),
        facts: serde_json::Value::Null,
    };
    Ok(all_policies(state)
        .await?
        .iter()
        .filter(|row| row.policy.spec.hooks != Hooks::default())
        .filter(|row| {
            let selector = &row.policy.spec.selector;
            after.applies(&row.repository_id, selector)
                && !before.applies(&row.repository_id, selector)
        })
        .map(|row| row.policy.metadata.name.clone())
        .collect())
}

async fn detail(state: &AppState, row: PolicyRow) -> Result<PolicyDetail, ApiError> {
    let agents = load_agents(state).await?;
    let selector = &row.policy.spec.selector;
    let own: Vec<&AgentRow> = agents
        .iter()
        .filter(|a| a.applies(&row.repository_id, selector))
        .collect();
    Ok(PolicyDetail {
        name: row.policy.metadata.name.clone(),
        matching: matching(state, &agents, &row.repository_id, selector),
        warnings: hook_warnings(&row.policy, &own),
        schedule: schedule_info(&row.policy, now_ms()),
        managed_by: row.managed_by,
        yaml: row.yaml,
        revision: row.revision,
        created_at: row.created_at,
        updated_at: row.updated_at,
        repository_id: row.repository_id,
        repository_name: row.repository_name,
        policy: row.policy,
    })
}

fn schedule_info(policy: &BackupPolicy, now: i64) -> Option<ScheduleInfo> {
    schedule_info_of(policy.spec.schedule.as_ref()?, now)
}

pub(crate) fn schedule_info_of(schedule: &Schedule, now: i64) -> Option<ScheduleInfo> {
    let compiled = schedule.compile().ok()?;
    let from = Timestamp::from_millisecond(now).ok()?;
    Some(ScheduleInfo {
        summary: compiled.describe(),
        timezone: compiled.timezone_name().to_string(),
        jitter_secs: compiled.jitter().as_secs(),
        catch_up_secs: compiled.catch_up_window().as_secs(),
        next_runs: compiled
            .next_slots(from, NEXT_RUNS)
            .into_iter()
            .map(|slot| slot.as_millisecond())
            .collect(),
        dst_notes: compiled.dst_notes(from, DST_HORIZON_DAYS),
    })
}

async fn policy_exists(state: &AppState, name: &str) -> Result<bool, ApiError> {
    Ok(sqlx::query("SELECT 1 FROM policies WHERE name = $1")
        .bind(name)
        .fetch_optional(&state.db.pool)
        .await?
        .is_some())
}

#[utoipa::path(
    post,
    tag = "Policies",
    summary = "Validate a policy",
    security(("session" = [], "csrf" = []), ("bearer" = ["policies:read"])),
    path = "/api/v1/policies/validate",
    request_body = ValidateRequest,
    responses((status = 200, description = "Validation result with the hosts the policy would take", body = ValidateResponse))
)]
pub async fn validate_policy(
    State(state): State<AppState>,
    _auth: Scoped<PoliciesRead>,
    Json(req): Json<ValidateRequest>,
) -> Result<Json<ValidateResponse>, ApiError> {
    let checked = match check(&state, req.yaml, req.policy).await {
        Ok(checked) => checked,
        Err(Failure::Problems(errors)) => {
            return Ok(Json(ValidateResponse {
                valid: false,
                policy: None,
                yaml: None,
                errors,
                name_taken: false,
                matching: None,
                warnings: Vec::new(),
                schedule: None,
            }));
        }
        Err(Failure::Api(e)) => return Err(e),
    };
    let name = &checked.policy.metadata.name;
    let name_taken =
        req.original_name.as_deref() != Some(name) && policy_exists(&state, name).await?;
    let agents = load_agents(&state).await?;
    let selector = &checked.policy.spec.selector;
    let own: Vec<&AgentRow> = agents
        .iter()
        .filter(|a| a.applies(&checked.repository_id, selector))
        .collect();
    Ok(Json(ValidateResponse {
        valid: true,
        errors: Vec::new(),
        name_taken,
        matching: Some(matching(&state, &agents, &checked.repository_id, selector)),
        warnings: hook_warnings(&checked.policy, &own),
        schedule: schedule_info(&checked.policy, now_ms()),
        yaml: Some(checked.yaml),
        policy: Some(checked.policy),
    }))
}

#[utoipa::path(
    get,
    tag = "Policies",
    summary = "List policies",
    security(("session" = []), ("bearer" = ["policies:read"])),
    path = "/api/v1/policies",
    responses((status = 200, description = "All policies by name", body = Vec<PolicySummary>))
)]
pub async fn list_policies(
    State(state): State<AppState>,
    _auth: Scoped<PoliciesRead>,
) -> Result<Json<Vec<PolicySummary>>, ApiError> {
    let agents = load_agents(&state).await?;
    let mut out = Vec::new();
    for row in all_policies(&state).await? {
        let selector = &row.policy.spec.selector;
        let selected = agents.iter().filter(|a| a.selected_by(selector));
        let hosts = selected
            .clone()
            .filter(|a| a.repository_id == row.repository_id)
            .count();
        out.push(PolicySummary {
            name: row.policy.metadata.name.clone(),
            match_labels: selector
                .match_labels
                .iter()
                .map(|(key, value)| Label {
                    key: key.clone(),
                    value: value.clone(),
                })
                .collect(),
            host_list: selector.hosts.is_some(),
            hosts: hosts as i64,
            other_repository_hosts: (selected.count() - hosts) as i64,
            last_job: last_job(&state, &row.id, None).await?,
            managed_by: row.managed_by.clone(),
            schedule: schedule_info(&row.policy, now_ms()),
            repository_id: row.repository_id,
            repository_name: row.repository_name,
        });
    }
    Ok(Json(out))
}

#[utoipa::path(
    post,
    tag = "Policies",
    summary = "Create a policy",
    security(("session" = [], "csrf" = []), ("bearer" = ["policies:write"])),
    path = "/api/v1/policies",
    request_body = PolicyInput,
    responses(
        (status = 201, description = "The created policy", body = PolicyDetail),
        (status = 409, description = "The name is taken", body = ErrorBody),
        (status = 422, description = "Invalid policy", body = ErrorBody),
    )
)]
pub async fn create_policy(
    State(state): State<AppState>,
    auth: Scoped<PoliciesWrite>,
    Json(input): Json<PolicyInput>,
) -> Result<(StatusCode, Json<PolicyDetail>), ApiError> {
    let checked = check(&state, input.yaml, input.policy).await?;
    if retention_of(&checked.policy).is_some() {
        auth.require_also(Scope::SnapshotsDelete, RETENTION_PURPOSE)?;
    }
    let name = checked.policy.metadata.name.clone();
    if gains_hook_hosts(None, (&checked.repository_id, &checked.policy), &[]) {
        auth.require_also(
            Scope::HooksWrite,
            hooks_purpose(std::slice::from_ref(&name)),
        )?;
    }
    let now = now_ms();
    let inserted = sqlx::query(
        "INSERT INTO policies (id, name, repository_id, yaml, spec_json, revision, created_at, \
         updated_at) VALUES ($1, $2, $3, $4, $5, 1, $6, $6) ON CONFLICT (name) DO NOTHING",
    )
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(&name)
    .bind(&checked.repository_id)
    .bind(&checked.yaml)
    .bind(serde_json::to_string(&checked.policy).map_err(|e| ApiError::Internal(e.into()))?)
    .bind(now)
    .execute(&state.db.pool)
    .await?
    .rows_affected()
        == 1;
    if !inserted {
        return Err(ApiError::Conflict(format!(
            "A policy named \"{name}\" already exists. Choose another name."
        )));
    }
    let row = find_policy(&state, &name).await?;
    Ok((StatusCode::CREATED, Json(detail(&state, row).await?)))
}

#[utoipa::path(
    get,
    tag = "Policies",
    summary = "Get a policy",
    security(("session" = []), ("bearer" = ["policies:read"])),
    path = "/api/v1/policies/{name}",
    params(("name" = String, Path, description = "Policy name")),
    responses(
        (status = 200, description = "The policy", body = PolicyDetail),
        (status = 404, description = "Unknown policy", body = ErrorBody),
    )
)]
pub async fn get_policy(
    State(state): State<AppState>,
    _auth: Scoped<PoliciesRead>,
    Path(name): Path<String>,
) -> Result<Json<PolicyDetail>, ApiError> {
    let row = find_policy(&state, &name).await?;
    Ok(Json(detail(&state, row).await?))
}

#[utoipa::path(
    put,
    tag = "Policies",
    summary = "Update a policy",
    security(("session" = [], "csrf" = []), ("bearer" = ["policies:write"])),
    path = "/api/v1/policies/{name}",
    params(("name" = String, Path, description = "Policy name")),
    request_body = PolicyUpdate,
    responses(
        (status = 200, description = "The updated policy", body = PolicyDetail),
        (status = 404, description = "Unknown policy", body = ErrorBody),
        (status = 409, description = "The policy changed since base_revision (code policy_conflict)", body = ErrorBody),
        (status = 422, description = "Invalid policy or changed name", body = ErrorBody),
    )
)]
pub async fn update_policy(
    State(state): State<AppState>,
    auth: Scoped<PoliciesWrite>,
    Path(name): Path<String>,
    Json(input): Json<PolicyUpdate>,
) -> Result<Json<PolicyDetail>, ApiError> {
    let checked = check(&state, input.yaml, input.policy).await?;
    if checked.policy.metadata.name != name {
        return Err(ApiError::Validation("The name cannot be changed.".into()));
    }
    if matches!(auth.principal(), Principal::Token { .. }) {
        let current = find_policy(&state, &name).await?;
        if retention_of(&current.policy) != retention_of(&checked.policy) {
            auth.require_also(Scope::SnapshotsDelete, RETENTION_PURPOSE)?;
        }
        let agents = load_agents(&state).await?;
        let old = (current.repository_id.as_str(), &current.policy);
        if gains_hook_hosts(
            Some(old),
            (&checked.repository_id, &checked.policy),
            &agents,
        ) {
            auth.require_also(
                Scope::HooksWrite,
                hooks_purpose(std::slice::from_ref(&name)),
            )?;
        }
    }
    let updated = sqlx::query(
        "UPDATE policies SET repository_id = $1, yaml = $2, spec_json = $3, \
         revision = revision + 1, updated_at = $4 WHERE name = $5 AND revision = $6",
    )
    .bind(&checked.repository_id)
    .bind(&checked.yaml)
    .bind(serde_json::to_string(&checked.policy).map_err(|e| ApiError::Internal(e.into()))?)
    .bind(now_ms())
    .bind(&name)
    .bind(input.base_revision)
    .execute(&state.db.pool)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(if policy_exists(&state, &name).await? {
            ApiError::PolicyConflict(format!(
                "Policy \"{name}\" was changed elsewhere. Reload it and apply your edits again."
            ))
        } else {
            ApiError::NotFound("policy not found".into())
        });
    }
    let row = find_policy(&state, &name).await?;
    Ok(Json(detail(&state, row).await?))
}

#[utoipa::path(
    delete,
    tag = "Policies",
    summary = "Delete a policy",
    security(("session" = [], "csrf" = []), ("bearer" = ["policies:write"])),
    path = "/api/v1/policies/{name}",
    params(("name" = String, Path, description = "Policy name")),
    responses(
        (status = 204, description = "The policy is deleted"),
        (status = 404, description = "Unknown policy", body = ErrorBody),
        (status = 409, description = "The policy has jobs in progress", body = ErrorBody),
    )
)]
pub async fn delete_policy(
    State(state): State<AppState>,
    _auth: Scoped<PoliciesWrite>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    let deleted = sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM policies WHERE name = $1 AND NOT EXISTS \
         (SELECT 1 FROM jobs WHERE jobs.policy_id = policies.id AND jobs.state IN ({ACTIVE_STATES}))"
    )))
    .bind(&name)
    .execute(&state.db.pool)
    .await?
    .rows_affected();
    if deleted == 1 {
        return Ok(StatusCode::NO_CONTENT);
    }
    let row = find_policy(&state, &name).await?;
    let active: i64 = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) AS n FROM jobs WHERE policy_id = $1 AND state IN ({ACTIVE_STATES})"
    )))
    .bind(&row.id)
    .fetch_one(&state.db.pool)
    .await?
    .try_get("n")?;
    Err(ApiError::Conflict(format!(
        "Policy \"{name}\" has {active} jobs in progress. Cancel them first, then delete the policy."
    )))
}

fn hosts_of(agents: &[AgentRow], row: &PolicyRow) -> Vec<(String, String)> {
    agents
        .iter()
        .filter(|a| a.applies(&row.repository_id, &row.policy.spec.selector))
        .map(|a| (a.id.clone(), a.hostname.clone()))
        .collect()
}

#[utoipa::path(
    post,
    tag = "Policies",
    summary = "Run a policy now",
    security(("session" = [], "csrf" = []), ("bearer" = ["jobs:run"])),
    path = "/api/v1/policies/{name}/run",
    params(("name" = String, Path, description = "Policy name")),
    responses(
        (status = 200, description = "One job per matching host; existing active jobs are returned as is", body = RunResult),
        (status = 404, description = "Unknown policy", body = ErrorBody),
    )
)]
pub async fn run_policy(
    State(state): State<AppState>,
    _auth: Scoped<JobsRun>,
    Path(name): Path<String>,
) -> Result<Json<RunResult>, ApiError> {
    let row = find_policy(&state, &name).await?;
    let hosts = hosts_of(&load_agents(&state).await?, &row);
    let jobs = enqueue(&state, &row.id, &name, &hosts, &job_spec(&row.policy)).await?;
    Ok(Json(RunResult { jobs }))
}

async fn host_agent(state: &AppState, id: &str) -> Result<AgentRow, ApiError> {
    if let Some(agent) = load_agents(state).await?.into_iter().find(|a| a.id == id) {
        return Ok(agent);
    }
    let row = sqlx::query("SELECT status FROM agents WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db.pool)
        .await?
        .ok_or_else(|| ApiError::NotFound("host not found".into()))?;
    let _: String = row.try_get("status")?;
    Err(ApiError::Conflict("the host is revoked".into()))
}

fn policies_of_host<'a>(host: &AgentRow, rows: &'a [PolicyRow]) -> Vec<&'a PolicyRow> {
    rows.iter()
        .filter(|r| host.applies(&r.repository_id, &r.policy.spec.selector))
        .collect()
}

#[utoipa::path(
    post,
    tag = "Hosts",
    summary = "Run a backup on a host",
    security(("session" = [], "csrf" = []), ("bearer" = ["jobs:run"])),
    path = "/api/v1/hosts/{id}/run",
    params(("id" = String, Path, description = "Host id")),
    request_body(content = Option<HostRunRequest>, description = "Policy names; all policies of the host when omitted"),
    responses(
        (status = 200, description = "One job per policy", body = RunResult),
        (status = 404, description = "Unknown host", body = ErrorBody),
        (status = 409, description = "The host is revoked", body = ErrorBody),
        (status = 422, description = "A policy does not apply to the host", body = ErrorBody),
    )
)]
pub async fn run_host(
    State(state): State<AppState>,
    _auth: Scoped<JobsRun>,
    Path(id): Path<String>,
    request: Option<Json<HostRunRequest>>,
) -> Result<Json<RunResult>, ApiError> {
    let host = host_agent(&state, &id).await?;
    let rows = all_policies(&state).await?;
    let mut chosen = policies_of_host(&host, &rows);
    if let Some(names) = request.and_then(|Json(r)| r.policies) {
        let mut picked = Vec::new();
        for name in &names {
            let row = chosen
                .iter()
                .find(|r| &r.policy.metadata.name == name)
                .ok_or_else(|| {
                    ApiError::Validation(format!("Policy \"{name}\" does not apply to this host."))
                })?;
            picked.push(*row);
        }
        chosen = picked;
    }
    let target = [(host.id.clone(), host.hostname.clone())];
    let mut jobs = Vec::new();
    for row in chosen {
        jobs.extend(
            enqueue(
                &state,
                &row.id,
                &row.policy.metadata.name,
                &target,
                &job_spec(&row.policy),
            )
            .await?,
        );
    }
    Ok(Json(RunResult { jobs }))
}

#[utoipa::path(
    get,
    tag = "Hosts",
    summary = "List policies that apply to a host",
    security(("session" = []), ("bearer" = ["policies:read"])),
    path = "/api/v1/hosts/{id}/policies",
    params(("id" = String, Path, description = "Host id")),
    responses(
        (status = 200, description = "Policies that apply to the host", body = Vec<HostPolicy>),
        (status = 404, description = "Unknown host", body = ErrorBody),
        (status = 409, description = "The host is revoked", body = ErrorBody),
    )
)]
pub async fn list_host_policies(
    State(state): State<AppState>,
    _auth: Scoped<PoliciesRead>,
    Path(id): Path<String>,
) -> Result<Json<Vec<HostPolicy>>, ApiError> {
    let host = host_agent(&state, &id).await?;
    let rows = all_policies(&state).await?;
    let mut out = Vec::new();
    for row in policies_of_host(&host, &rows) {
        out.push(HostPolicy {
            name: row.policy.metadata.name.clone(),
            last_job: last_job(&state, &row.id, Some(&host.id)).await?,
            next_run: schedule_info(&row.policy, now_ms())
                .and_then(|s| s.next_runs.first().copied()),
        });
    }
    Ok(Json(out))
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(validate_policy))
        .routes(routes!(apply::apply_policies))
        .routes(routes!(list_policies, create_policy))
        .routes(routes!(get_policy, update_policy, delete_policy))
        .routes(routes!(run_policy))
        .routes(routes!(run_host))
        .routes(routes!(list_host_policies))
}
