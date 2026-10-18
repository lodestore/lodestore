use std::collections::{BTreeMap, BTreeSet};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use lodestore_policy::{BackupPolicy, Problem, name_ok, to_yaml, validate, yaml_problem};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;
use utoipa::ToSchema;

use super::{Checked, Failure, PolicyRow, RETENTION_PURPOSE, all_policies, resolve, retention_of};
use crate::auth::tokens::{PoliciesWrite, Scope, Scoped};
use crate::db::now_ms;
use crate::error::{ApiError, ErrorBody};
use crate::jobs::ACTIVE_STATES;
use crate::state::AppState;

const DEFAULT_MANAGER: &str = "lode-apply";
const MAX_DOCUMENTS: usize = 1000;
const MANAGER_RULE: &str =
    "Use up to 63 lowercase letters, digits and \"-\"; start and end with a letter or digit.";
const RACE: &str = "Policies changed while applying. Run apply again.";

static NULL: Value = Value::Null;

#[derive(Deserialize, ToSchema)]
pub struct ApplyDocument {
    pub source: String,
    pub yaml: String,
}

#[derive(Deserialize, ToSchema)]
pub struct ApplyRequest {
    pub documents: Vec<ApplyDocument>,
    pub manager: Option<String>,
    #[serde(default)]
    pub prune: bool,
    #[serde(default)]
    pub adopt: bool,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChangeAction {
    Create,
    Update,
    Delete,
}

#[derive(Serialize, ToSchema)]
pub struct FieldChange {
    pub path: String,
    pub from: Option<Value>,
    pub to: Option<Value>,
}

#[derive(Serialize, ToSchema)]
pub struct PolicyChange {
    pub action: ChangeAction,
    pub name: String,
    pub source: Option<String>,
    pub adopted: bool,
    pub managed_by: Option<String>,
    pub fields: Vec<FieldChange>,
}

#[derive(Serialize, ToSchema)]
pub struct ApplyResponse {
    pub dry_run: bool,
    pub manager: String,
    pub changes: Vec<PolicyChange>,
    pub unchanged: u64,
}

#[derive(Serialize, ToSchema)]
pub struct ApplyProblem {
    pub source: String,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub path: Option<String>,
    pub message: String,
}

#[derive(Serialize, ToSchema)]
pub struct ApplyErrorBody {
    pub code: String,
    pub message: String,
    pub errors: Vec<ApplyProblem>,
}

pub enum ApplyFailure {
    Problems(Vec<ApplyProblem>),
    Api(ApiError),
}

impl From<ApiError> for ApplyFailure {
    fn from(e: ApiError) -> Self {
        ApplyFailure::Api(e)
    }
}

impl From<sqlx::Error> for ApplyFailure {
    fn from(e: sqlx::Error) -> Self {
        ApplyFailure::Api(e.into())
    }
}

impl IntoResponse for ApplyFailure {
    fn into_response(self) -> Response {
        match self {
            ApplyFailure::Api(e) => e.into_response(),
            ApplyFailure::Problems(errors) => {
                let message = match errors.first() {
                    Some(ApplyProblem {
                        source,
                        line: Some(line),
                        message,
                        ..
                    }) => format!("{source}: line {line}: {message}"),
                    Some(ApplyProblem {
                        source, message, ..
                    }) => format!("{source}: {message}"),
                    None => "The input is invalid.".to_string(),
                };
                (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(ApplyErrorBody {
                        code: "validation".into(),
                        message,
                        errors,
                    }),
                )
                    .into_response()
            }
        }
    }
}

struct Input {
    source: String,
    checked: Checked,
}

enum Step {
    Create(Checked),
    Update {
        id: String,
        revision: i64,
        checked: Checked,
    },
    Delete {
        id: String,
        name: String,
    },
}

fn located(source: &str, problem: Problem) -> ApplyProblem {
    ApplyProblem {
        source: source.to_string(),
        line: problem.line,
        column: problem.column,
        path: problem.path,
        message: problem.message,
    }
}

fn plain(source: &str, path: &str, message: String) -> ApplyProblem {
    ApplyProblem {
        source: source.to_string(),
        line: None,
        column: None,
        path: Some(path.to_string()),
        message,
    }
}

fn parse_documents(
    documents: &[ApplyDocument],
) -> (Vec<(String, BackupPolicy)>, Vec<ApplyProblem>) {
    let mut parsed = Vec::new();
    let mut problems = Vec::new();
    let mut count = 0;
    'files: for doc in documents {
        for de in yaml_serde::Deserializer::from_str(&doc.yaml) {
            count += 1;
            if count > MAX_DOCUMENTS {
                problems.push(ApplyProblem {
                    source: doc.source.clone(),
                    line: None,
                    column: None,
                    path: None,
                    message: format!("At most {MAX_DOCUMENTS} documents are allowed."),
                });
                break 'files;
            }
            match Option::<BackupPolicy>::deserialize(de) {
                Ok(None) => {}
                Ok(Some(policy)) => {
                    let found = validate(&policy);
                    if found.is_empty() {
                        parsed.push((doc.source.clone(), policy));
                    }
                    problems.extend(found.into_iter().map(|p| located(&doc.source, p)));
                }
                Err(e) => {
                    let problem = located(&doc.source, yaml_problem(&e));
                    let repeated = problems.last().is_some_and(|last| {
                        (last.line, last.column, &last.message)
                            == (problem.line, problem.column, &problem.message)
                    });
                    if repeated {
                        break;
                    }
                    problems.push(problem);
                }
            }
        }
    }
    (parsed, problems)
}

async fn collect(state: &AppState, req: &ApplyRequest) -> Result<Vec<Input>, ApplyFailure> {
    let (parsed, mut problems) = parse_documents(&req.documents);
    let mut inputs: Vec<Input> = Vec::new();
    for (source, policy) in parsed {
        let name = policy.metadata.name.clone();
        if let Some(first) = inputs
            .iter()
            .find(|i| i.checked.policy.metadata.name == name)
        {
            problems.push(plain(
                &source,
                "metadata.name",
                format!("Policy \"{name}\" is also defined in {}.", first.source),
            ));
            continue;
        }
        let yaml = to_yaml(&policy);
        match resolve(state, policy, yaml).await {
            Ok(checked) => inputs.push(Input { source, checked }),
            Err(Failure::Problems(found)) => {
                problems.extend(found.into_iter().map(|p| located(&source, p)));
            }
            Err(Failure::Api(e)) => return Err(e.into()),
        }
    }
    if problems.is_empty() {
        Ok(inputs)
    } else {
        Err(ApplyFailure::Problems(problems))
    }
}

fn diff(path: &str, from: &Value, to: &Value, out: &mut Vec<FieldChange>) {
    if from == to {
        return;
    }
    let empty = serde_json::Map::new();
    let (a, b) = match (from, to) {
        (Value::Object(a), Value::Object(b)) => (a, b),
        (Value::Object(a), Value::Null) => (a, &empty),
        (Value::Null, Value::Object(b)) => (&empty, b),
        _ => {
            out.push(FieldChange {
                path: path.to_string(),
                from: Some(from.clone()).filter(|v| !v.is_null()),
                to: Some(to.clone()).filter(|v| !v.is_null()),
            });
            return;
        }
    };
    let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    for key in keys {
        diff(
            &format!("{path}.{key}"),
            a.get(key).unwrap_or(&NULL),
            b.get(key).unwrap_or(&NULL),
            out,
        );
    }
}

fn spec_of(policy: &BackupPolicy) -> Result<Value, ApiError> {
    let value = serde_json::to_value(policy).map_err(|e| ApiError::Internal(e.into()))?;
    Ok(value.get("spec").cloned().unwrap_or(Value::Null))
}

struct Plan {
    changes: Vec<PolicyChange>,
    steps: Vec<Step>,
    unchanged: u64,
    retention_changed: bool,
}

fn plan(
    req: &ApplyRequest,
    manager: &str,
    mut inputs: Vec<Input>,
    current: Vec<PolicyRow>,
) -> Result<Plan, ApplyFailure> {
    inputs.sort_by(|a, b| {
        a.checked
            .policy
            .metadata
            .name
            .cmp(&b.checked.policy.metadata.name)
    });
    let by_name: BTreeMap<&str, &PolicyRow> = current
        .iter()
        .map(|row| (row.policy.metadata.name.as_str(), row))
        .collect();
    let mut problems = Vec::new();
    let mut creates = Vec::new();
    let mut updates = Vec::new();
    let mut unchanged = 0;
    let mut retention_changed = false;
    let mut wanted = BTreeSet::new();
    for input in inputs {
        let name = input.checked.policy.metadata.name.clone();
        wanted.insert(name.clone());
        let change = |action, adopted, fields| PolicyChange {
            action,
            name: name.clone(),
            source: Some(input.source.clone()),
            adopted,
            managed_by: Some(manager.to_string()),
            fields,
        };
        let Some(row) = by_name.get(name.as_str()) else {
            retention_changed |= retention_of(&input.checked.policy).is_some();
            creates.push((
                change(ChangeAction::Create, false, Vec::new()),
                Step::Create(input.checked),
            ));
            continue;
        };
        let mut adopted = false;
        match row.managed_by.as_deref() {
            Some(owner) if owner == manager => {}
            Some(owner) => {
                problems.push(plain(
                    &input.source,
                    "metadata.name",
                    format!("policy/{name} is managed by {owner}."),
                ));
                continue;
            }
            None if req.adopt => adopted = true,
            None => {
                problems.push(plain(
                    &input.source,
                    "metadata.name",
                    format!(
                        "policy/{name} exists and is not managed by {manager}. Use --adopt to take it over."
                    ),
                ));
                continue;
            }
        }
        let mut fields = Vec::new();
        diff(
            "spec",
            &spec_of(&row.policy)?,
            &spec_of(&input.checked.policy)?,
            &mut fields,
        );
        if fields.is_empty() && !adopted {
            unchanged += 1;
            continue;
        }
        retention_changed |= retention_of(&row.policy) != retention_of(&input.checked.policy);
        updates.push((
            change(ChangeAction::Update, adopted, fields),
            Step::Update {
                id: row.id.clone(),
                revision: row.revision,
                checked: input.checked,
            },
        ));
    }
    if !problems.is_empty() {
        return Err(ApplyFailure::Problems(problems));
    }
    let deletes = current
        .iter()
        .filter(|row| req.prune && row.managed_by.as_deref() == Some(manager))
        .filter(|row| !wanted.contains(&row.policy.metadata.name))
        .map(|row| {
            let name = row.policy.metadata.name.clone();
            (
                PolicyChange {
                    action: ChangeAction::Delete,
                    name: name.clone(),
                    source: None,
                    adopted: false,
                    managed_by: Some(manager.to_string()),
                    fields: Vec::new(),
                },
                Step::Delete {
                    id: row.id.clone(),
                    name,
                },
            )
        });
    let (changes, steps) = creates.into_iter().chain(updates).chain(deletes).unzip();
    Ok(Plan {
        changes,
        steps,
        unchanged,
        retention_changed,
    })
}

async fn execute(state: &AppState, manager: &str, steps: Vec<Step>) -> Result<(), ApplyFailure> {
    let now = now_ms();
    let mut tx = state.db.pool.begin().await?;
    for step in steps {
        match step {
            Step::Create(checked) => {
                let inserted = sqlx::query(
                    "INSERT INTO policies (id, name, repository_id, yaml, spec_json, revision, \
                     created_at, updated_at, managed_by) VALUES ($1, $2, $3, $4, $5, 1, $6, $6, $7) \
                     ON CONFLICT (name) DO NOTHING",
                )
                .bind(uuid::Uuid::now_v7().to_string())
                .bind(&checked.policy.metadata.name)
                .bind(&checked.repository_id)
                .bind(&checked.yaml)
                .bind(spec_json(&checked.policy)?)
                .bind(now)
                .bind(manager)
                .execute(&mut *tx)
                .await?
                .rows_affected();
                if inserted != 1 {
                    return Err(ApiError::Conflict(RACE.into()).into());
                }
            }
            Step::Update {
                id,
                revision,
                checked,
            } => {
                let updated = sqlx::query(
                    "UPDATE policies SET repository_id = $1, yaml = $2, spec_json = $3, \
                     revision = revision + 1, updated_at = $4, managed_by = $5 \
                     WHERE id = $6 AND revision = $7",
                )
                .bind(&checked.repository_id)
                .bind(&checked.yaml)
                .bind(spec_json(&checked.policy)?)
                .bind(now)
                .bind(manager)
                .bind(&id)
                .bind(revision)
                .execute(&mut *tx)
                .await?
                .rows_affected();
                if updated != 1 {
                    return Err(ApiError::Conflict(RACE.into()).into());
                }
            }
            Step::Delete { id, name } => {
                let deleted = sqlx::query(sqlx::AssertSqlSafe(format!(
                    "DELETE FROM policies WHERE id = $1 AND managed_by = $2 AND NOT EXISTS \
                     (SELECT 1 FROM jobs WHERE jobs.policy_id = policies.id \
                     AND jobs.state IN ({ACTIVE_STATES}))"
                )))
                .bind(&id)
                .bind(manager)
                .execute(&mut *tx)
                .await?
                .rows_affected();
                if deleted != 1 {
                    let active: i64 = sqlx::query(sqlx::AssertSqlSafe(format!(
                        "SELECT COUNT(*) AS n FROM jobs WHERE policy_id = $1 \
                         AND state IN ({ACTIVE_STATES})"
                    )))
                    .bind(&id)
                    .fetch_one(&mut *tx)
                    .await?
                    .try_get("n")?;
                    return Err(ApiError::Conflict(if active > 0 {
                        format!(
                            "Policy \"{name}\" has {active} jobs in progress. Cancel them first, then apply again."
                        )
                    } else {
                        RACE.into()
                    })
                    .into());
                }
            }
        }
    }
    tx.commit().await?;
    Ok(())
}

fn spec_json(policy: &BackupPolicy) -> Result<String, ApiError> {
    serde_json::to_string(policy).map_err(|e| ApiError::Internal(e.into()))
}

#[utoipa::path(
    post,
    tag = "Policies",
    summary = "Apply a set of policies",
    security(("session" = [], "csrf" = []), ("bearer" = ["policies:write"])),
    path = "/api/v1/policies/apply",
    request_body = ApplyRequest,
    responses(
        (status = 200, description = "The plan; applied unless dry_run is set", body = ApplyResponse),
        (status = 409, description = "A policy to delete has jobs in progress, or policies changed while applying", body = ErrorBody),
        (status = 422, description = "Invalid input; nothing is applied", body = ApplyErrorBody),
    )
)]
pub async fn apply_policies(
    State(state): State<AppState>,
    auth: Scoped<PoliciesWrite>,
    Json(req): Json<ApplyRequest>,
) -> Result<Json<ApplyResponse>, ApplyFailure> {
    let manager = req
        .manager
        .clone()
        .unwrap_or_else(|| DEFAULT_MANAGER.into());
    if !name_ok(&manager) {
        return Err(ApplyFailure::Problems(vec![plain(
            "request",
            "manager",
            MANAGER_RULE.into(),
        )]));
    }
    let inputs = collect(&state, &req).await?;
    if req.prune && inputs.is_empty() {
        return Err(ApplyFailure::Problems(vec![plain(
            "request",
            "documents",
            "--prune needs at least one policy in the input. Nothing was deleted.".into(),
        )]));
    }
    let Plan {
        changes,
        steps,
        unchanged,
        retention_changed,
    } = plan(&req, &manager, inputs, all_policies(&state).await?)?;
    if retention_changed {
        auth.require_also(Scope::SnapshotsDelete, RETENTION_PURPOSE)?;
    }
    if !req.dry_run && !steps.is_empty() {
        execute(&state, &manager, steps).await?;
    }
    Ok(Json(ApplyResponse {
        dry_run: req.dry_run,
        manager,
        changes,
        unchanged,
    }))
}
