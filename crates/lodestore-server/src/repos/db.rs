use std::collections::{BTreeMap, HashMap};

use anyhow::Context;
use lodestore_format::keys::{MasterKey, ReadKeys};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use zeroize::Zeroizing;

use crate::catalog::RepositoryUnavailable;
use crate::db::now_ms;
use crate::state::AppState;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoLocation {
    pub backend: String,
    pub options: BTreeMap<String, String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct AccessKey {
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl std::fmt::Debug for AccessKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessKey")
            .field("access_key_id", &self.access_key_id)
            .finish_non_exhaustive()
    }
}

pub struct NewRepository {
    pub name: String,
    pub location: RepoLocation,
    pub experimental: bool,
    pub format_repo_id: [u8; 16],
    pub config_hash: [u8; 32],
    pub master: MasterKey,
    pub server_slot_secret: Option<Zeroizing<Vec<u8>>>,
    pub server_creds: Option<AccessKey>,
    pub agent_creds: Option<AccessKey>,
}

#[derive(Debug, Clone)]
pub struct RepositoryRow {
    pub id: String,
    pub name: String,
    pub backend: String,
}

pub struct WriterMaterial {
    pub writer_keys_text: Zeroizing<String>,
    pub location: RepoLocation,
    pub agent_creds: Option<AccessKey>,
}

#[derive(Serialize, Deserialize, Default)]
struct SealedAccess {
    #[serde(default)]
    access_key_id: String,
    #[serde(default)]
    secret_access_key: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    options: BTreeMap<String, String>,
}

impl SealedAccess {
    fn key(&self) -> Option<AccessKey> {
        (!self.access_key_id.is_empty()).then(|| AccessKey {
            access_key_id: self.access_key_id.clone(),
            secret_access_key: self.secret_access_key.clone(),
        })
    }
}

fn sealed_json(
    key: Option<&AccessKey>,
    options: &BTreeMap<String, String>,
) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    let access = SealedAccess {
        access_key_id: key.map(|k| k.access_key_id.clone()).unwrap_or_default(),
        secret_access_key: key.map(|k| k.secret_access_key.clone()).unwrap_or_default(),
        options: options.clone(),
    };
    Ok(Zeroizing::new(serde_json::to_vec(&access)?))
}

fn agent_json(key: &AccessKey) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    Ok(Zeroizing::new(serde_json::to_vec(key)?))
}

/// What the wizard knows beyond the plain registration: option values that
/// are secrets (experimental backends) and the capability probe outcome.
#[derive(Default)]
pub struct RepoExtras {
    pub secret_options: BTreeMap<String, String>,
    pub checks_json: Option<String>,
    pub agent_key_state: Option<String>,
    pub checked_at: Option<i64>,
}

pub async fn create_repository(state: &AppState, new: NewRepository) -> anyhow::Result<String> {
    create_repository_ext(state, new, RepoExtras::default()).await
}

pub async fn create_repository_ext(
    state: &AppState,
    new: NewRepository,
    extras: RepoExtras,
) -> anyhow::Result<String> {
    let id = uuid::Uuid::now_v7().to_string();
    let mut tx = state.db.pool.begin().await?;
    let sealer = &state.sealer;
    let master = sealer
        .put_secret_on(&mut tx, "repo-master", new.master.as_bytes())
        .await?;
    let slot = match &new.server_slot_secret {
        Some(s) => Some(sealer.put_secret_on(&mut tx, "repo-server-slot", s).await?),
        None => None,
    };
    let server_creds = if new.server_creds.is_some() || !extras.secret_options.is_empty() {
        let blob = sealed_json(new.server_creds.as_ref(), &extras.secret_options)?;
        Some(
            sealer
                .put_secret_on(&mut tx, "repo-server-creds", &blob)
                .await?,
        )
    } else {
        None
    };
    let agent_creds = match &new.agent_creds {
        Some(k) => Some(
            sealer
                .put_secret_on(&mut tx, "repo-agent-creds", &agent_json(k)?)
                .await?,
        ),
        None => None,
    };
    let now = now_ms();
    sqlx::query(
        "INSERT INTO repositories (id, name, backend, location_json, experimental, format_repo_id, \
         config_hash, master_secret_id, server_slot_secret_id, server_creds_secret_id, \
         agent_creds_secret_id, agent_key_state, checks_json, checked_at, created_at, \
         provenance_backfilled_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
    )
    .bind(&id)
    .bind(&new.name)
    .bind(&new.location.backend)
    .bind(serde_json::to_string(&new.location.options)?)
    .bind(i64::from(new.experimental))
    .bind(new.format_repo_id.to_vec())
    .bind(new.config_hash.to_vec())
    .bind(master)
    .bind(slot)
    .bind(server_creds)
    .bind(agent_creds)
    .bind(
        extras
            .agent_key_state
            .unwrap_or_else(|| "unchecked".to_string()),
    )
    .bind(extras.checks_json)
    .bind(extras.checked_at)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn get_repository(state: &AppState, id: &str) -> anyhow::Result<Option<RepositoryRow>> {
    let row = sqlx::query("SELECT id, name, backend FROM repositories WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db.pool)
        .await?;
    row.map(|r| {
        Ok(RepositoryRow {
            id: r.try_get("id")?,
            name: r.try_get("name")?,
            backend: r.try_get("backend")?,
        })
    })
    .transpose()
}

pub async fn writer_material(
    state: &AppState,
    repository_id: &str,
) -> anyhow::Result<WriterMaterial> {
    let row = sqlx::query(
        "SELECT backend, location_json, experimental, format_repo_id, config_hash, \
         master_secret_id, server_creds_secret_id, agent_creds_secret_id, open_error \
         FROM repositories WHERE id = $1",
    )
    .bind(repository_id)
    .fetch_optional(&state.db.pool)
    .await?
    .with_context(|| format!("repository {repository_id} is missing"))?;
    if let Some(reason) = row.try_get::<Option<String>, _>("open_error")? {
        return Err(RepositoryUnavailable { reason }.into());
    }

    let backend: String = row.try_get("backend")?;
    let mut options: BTreeMap<String, String> =
        serde_json::from_str(&row.try_get::<String, _>("location_json")?)?;
    if row.try_get::<i64, _>("experimental")? != 0
        && let Some(id) = row.try_get::<Option<String>, _>("server_creds_secret_id")?
    {
        let sealed: SealedAccess = serde_json::from_slice(&state.sealer.get_secret(&id).await?)?;
        options.extend(sealed.options);
    }
    let repo_id: [u8; 16] = row
        .try_get::<Vec<u8>, _>("format_repo_id")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("format_repo_id must be 16 bytes"))?;
    let config_hash: [u8; 32] = row
        .try_get::<Vec<u8>, _>("config_hash")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("config_hash must be 32 bytes"))?;

    let master_bytes = state
        .sealer
        .get_secret(&row.try_get::<String, _>("master_secret_id")?)
        .await?;
    let master: [u8; 32] = master_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("master secret must be 32 bytes"))?;
    let writer_keys_text = ReadKeys::derive(MasterKey::from_bytes(master), repo_id)
        .write_keys(config_hash)
        .to_file_text();

    let agent_creds = match row.try_get::<Option<String>, _>("agent_creds_secret_id")? {
        Some(id) => Some(serde_json::from_slice(
            &state.sealer.get_secret(&id).await?,
        )?),
        None => None,
    };
    Ok(WriterMaterial {
        writer_keys_text,
        location: RepoLocation { backend, options },
        agent_creds,
    })
}

#[derive(Debug, Clone)]
pub struct RepoFull {
    pub id: String,
    pub name: String,
    pub backend: String,
    pub options: BTreeMap<String, String>,
    pub experimental: bool,
    pub agent_key_state: String,
    pub checks_json: Option<String>,
    pub checked_at: Option<i64>,
    pub open_error: Option<String>,
}

fn full_row(r: &sqlx::any::AnyRow) -> anyhow::Result<RepoFull> {
    Ok(RepoFull {
        id: r.try_get("id")?,
        name: r.try_get("name")?,
        backend: r.try_get("backend")?,
        options: serde_json::from_str(&r.try_get::<String, _>("location_json")?)?,
        experimental: r.try_get::<i64, _>("experimental")? != 0,
        agent_key_state: r.try_get("agent_key_state")?,
        checks_json: r.try_get("checks_json")?,
        checked_at: r.try_get("checked_at")?,
        open_error: r.try_get("open_error")?,
    })
}

pub async fn list_repositories(state: &AppState) -> anyhow::Result<Vec<RepoFull>> {
    sqlx::query(
        "SELECT id, name, backend, location_json, experimental, agent_key_state, checks_json, \
         checked_at, open_error FROM repositories ORDER BY created_at, id",
    )
    .fetch_all(&state.db.pool)
    .await?
    .iter()
    .map(full_row)
    .collect()
}

pub async fn find_repository(state: &AppState, id: &str) -> anyhow::Result<Option<RepoFull>> {
    sqlx::query(
        "SELECT id, name, backend, location_json, experimental, agent_key_state, checks_json, \
         checked_at, open_error FROM repositories WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db.pool)
    .await?
    .as_ref()
    .map(full_row)
    .transpose()
}

pub async fn name_exists(state: &AppState, name: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query("SELECT 1 FROM repositories WHERE name = $1")
        .bind(name)
        .fetch_optional(&state.db.pool)
        .await?
        .is_some())
}

pub async fn active_hosts(state: &AppState) -> anyhow::Result<HashMap<String, i64>> {
    sqlx::query(
        "SELECT repository_id, COUNT(*) AS n FROM agents WHERE status <> 'revoked' \
         GROUP BY repository_id",
    )
    .fetch_all(&state.db.pool)
    .await?
    .iter()
    .map(|r| Ok((r.try_get("repository_id")?, r.try_get("n")?)))
    .collect()
}

pub async fn update_checks(
    state: &AppState,
    id: &str,
    checks_json: &str,
    agent_key_state: &str,
) -> anyhow::Result<i64> {
    let at = now_ms();
    sqlx::query(
        "UPDATE repositories SET checks_json = $1, agent_key_state = $2, checked_at = $3 \
         WHERE id = $4",
    )
    .bind(checks_json)
    .bind(agent_key_state)
    .bind(at)
    .bind(id)
    .execute(&state.db.pool)
    .await?;
    Ok(at)
}

/// Everything the server needs to talk to a repository's storage and to open
/// the repository with its own key slot.
pub struct ServerAccess {
    pub location: lodestore_format::store::RepoLocation,
    pub server_creds: Option<AccessKey>,
    pub agent_creds: Option<AccessKey>,
    pub config_hash: [u8; 32],
    pub format_repo_id: [u8; 16],
    pub master: Zeroizing<Vec<u8>>,
    pub server_slot_secret: Option<Zeroizing<Vec<u8>>>,
    pub open_error: Option<String>,
}

pub(crate) async fn set_open_error(
    state: &AppState,
    id: &str,
    error: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE repositories SET open_error = $1 WHERE id = $2")
        .bind(error)
        .bind(id)
        .execute(&state.db.pool)
        .await?;
    Ok(())
}

/// For paths that do not reopen the repository themselves; `open_repository` reopens it and clears the flag.
pub async fn server_access_checked(state: &AppState, id: &str) -> anyhow::Result<ServerAccess> {
    let access = server_access(state, id).await?;
    match access.open_error {
        Some(reason) => Err(RepositoryUnavailable { reason }.into()),
        None => Ok(access),
    }
}

pub async fn server_access(state: &AppState, id: &str) -> anyhow::Result<ServerAccess> {
    let row = sqlx::query(
        "SELECT backend, location_json, config_hash, format_repo_id, master_secret_id, \
         server_slot_secret_id, server_creds_secret_id, agent_creds_secret_id, open_error \
         FROM repositories WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db.pool)
    .await?
    .with_context(|| format!("repository {id} is missing"))?;
    let backend: String = row.try_get("backend")?;
    let mut options: BTreeMap<String, String> =
        serde_json::from_str(&row.try_get::<String, _>("location_json")?)?;
    let mut server_creds = None;
    if let Some(sid) = row.try_get::<Option<String>, _>("server_creds_secret_id")? {
        let sealed: SealedAccess = serde_json::from_slice(&state.sealer.get_secret(&sid).await?)?;
        server_creds = sealed.key();
        options.extend(sealed.options);
    }
    let agent_creds = match row.try_get::<Option<String>, _>("agent_creds_secret_id")? {
        Some(sid) => Some(serde_json::from_slice(
            &state.sealer.get_secret(&sid).await?,
        )?),
        None => None,
    };
    let server_slot_secret = match row.try_get::<Option<String>, _>("server_slot_secret_id")? {
        Some(sid) => Some(state.sealer.get_secret(&sid).await?),
        None => None,
    };
    Ok(ServerAccess {
        location: lodestore_format::store::RepoLocation { backend, options },
        server_creds,
        agent_creds,
        config_hash: row
            .try_get::<Vec<u8>, _>("config_hash")?
            .try_into()
            .map_err(|_| anyhow::anyhow!("config_hash must be 32 bytes"))?,
        format_repo_id: row
            .try_get::<Vec<u8>, _>("format_repo_id")?
            .try_into()
            .map_err(|_| anyhow::anyhow!("format_repo_id must be 16 bytes"))?,
        master: state
            .sealer
            .get_secret(&row.try_get::<String, _>("master_secret_id")?)
            .await?,
        server_slot_secret,
        open_error: row.try_get("open_error")?,
    })
}

/// Swaps the sealed agent key for a new one in one transaction and records
/// the outcome of the check that approved it.
pub async fn replace_agent_creds(
    state: &AppState,
    id: &str,
    key: &AccessKey,
    agent_key_state: &str,
    checks_json: &str,
) -> anyhow::Result<()> {
    let mut tx = state.db.pool.begin().await?;
    let old = sqlx::query("SELECT agent_creds_secret_id FROM repositories WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .with_context(|| format!("repository {id} is missing"))?
        .try_get::<Option<String>, _>("agent_creds_secret_id")?;
    let new = state
        .sealer
        .put_secret_on(&mut tx, "repo-agent-creds", &agent_json(key)?)
        .await?;
    sqlx::query(
        "UPDATE repositories SET agent_creds_secret_id = $1, agent_key_state = $2, \
         checks_json = $3, checked_at = $4 WHERE id = $5",
    )
    .bind(new)
    .bind(agent_key_state)
    .bind(checks_json)
    .bind(now_ms())
    .bind(id)
    .execute(&mut *tx)
    .await?;
    if let Some(old) = old {
        sqlx::query("DELETE FROM secrets WHERE id = $1")
            .bind(old)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}
