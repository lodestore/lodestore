use std::collections::{BTreeMap, HashMap};

use anyhow::Context;
use lodestore_format::keys::{MasterKey, ReadKeys};
use lodestore_format::store::probe::HostKeyChange;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use zeroize::Zeroizing;

use crate::catalog::RepositoryUnavailable;
use crate::db::now_ms;
use crate::state::AppState;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoLocation {
    pub backend: String,
    pub options: BTreeMap<String, String>,
}

impl std::fmt::Debug for RepoLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let secret = lodestore_format::store::location::secret_option_names(&self.backend);
        let options: BTreeMap<&str, &str> = self
            .options
            .iter()
            .map(|(k, v)| {
                let hidden = secret.contains(&k.as_str())
                    || ["sas_token", "account_key", "key", "credential", "token"]
                        .contains(&k.as_str());
                (k.as_str(), if hidden { "<redacted>" } else { v.as_str() })
            })
            .collect();
        f.debug_struct("RepoLocation")
            .field("backend", &self.backend)
            .field("options", &options)
            .finish()
    }
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

/// The agents' own secret options of a backend; never printed or returned.
pub type AgentOptions = BTreeMap<String, Zeroizing<String>>;

pub struct NewRepository {
    pub name: String,
    pub location: RepoLocation,
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
    /// When the SAS signed for this material stops working, if one was signed.
    pub sas_expires_at_ms: Option<i64>,
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

    fn agent_options(self) -> AgentOptions {
        self.options
            .into_iter()
            .map(|(k, v)| (k, Zeroizing::new(v)))
            .collect()
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
/// are secrets (Azure Blob, GCS, SFTP) and the capability probe outcome.
#[derive(Default)]
pub struct RepoExtras {
    pub secret_options: BTreeMap<String, String>,
    pub agent_options: AgentOptions,
    pub checks_json: Option<String>,
    pub agent_key_state: Option<String>,
    pub checked_at: Option<i64>,
    pub guarantee_level: Option<String>,
    pub guarantee_notes_json: Option<String>,
    /// Set only by the wizard's create path, never by connect or by a migration.
    pub agent_sas_vending: bool,
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
    let agent_creds = if new.agent_creds.is_some() || !extras.agent_options.is_empty() {
        let options = extras
            .agent_options
            .iter()
            .map(|(k, v)| (k.clone(), v.to_string()))
            .collect();
        let blob = sealed_json(new.agent_creds.as_ref(), &options)?;
        Some(
            sealer
                .put_secret_on(&mut tx, "repo-agent-creds", &blob)
                .await?,
        )
    } else {
        None
    };
    let now = now_ms();
    let guarantee_level = extras
        .guarantee_level
        .unwrap_or_else(|| "unchecked".to_string());
    let guarantee_notes_json = extras
        .guarantee_notes_json
        .unwrap_or_else(|| "[]".to_string());
    sqlx::query(
        "INSERT INTO repositories (id, name, backend, location_json, format_repo_id, \
         config_hash, master_secret_id, server_slot_secret_id, server_creds_secret_id, \
         agent_creds_secret_id, agent_key_state, checks_json, checked_at, created_at, \
         provenance_backfilled_at, guarantee_level, guarantee_accepted, guarantee_notes_json, \
         guarantee_checked_at, agent_sas_vending) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, \
         $18, $19, $20)",
    )
    .bind(&id)
    .bind(&new.name)
    .bind(&new.location.backend)
    .bind(serde_json::to_string(&new.location.options)?)
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
    .bind(&guarantee_level)
    .bind(&guarantee_level)
    .bind(&guarantee_notes_json)
    .bind(extras.checked_at)
    .bind(i64::from(extras.agent_sas_vending))
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

/// What an agent needs to write: the writer keys and the location as
/// [`crate::repos::checks::agent_location`] shapes it. Backends other than S3
/// and a local folder get the server's sealed options too, unless the agent has
/// credentials of its own there, which then take their place.
pub async fn writer_material(
    state: &AppState,
    repository_id: &str,
) -> anyhow::Result<WriterMaterial> {
    let row = sqlx::query(
        "SELECT backend, location_json, format_repo_id, config_hash, \
         master_secret_id, server_creds_secret_id, agent_creds_secret_id, open_error, \
         agent_sas_vending FROM repositories WHERE id = $1",
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
    if !matches!(backend.as_str(), "s3" | "local")
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

    let (agent_creds, agent_options) =
        match row.try_get::<Option<String>, _>("agent_creds_secret_id")? {
            Some(id) => {
                let sealed: SealedAccess =
                    serde_json::from_slice(&state.sealer.get_secret(&id).await?)?;
                (sealed.key(), sealed.agent_options())
            }
            None => (None, AgentOptions::new()),
        };
    let full = lodestore_format::store::RepoLocation { backend, options };
    let sas_vending = row.try_get::<i64, _>("agent_sas_vending")? != 0;
    let (for_agent, expires_at) =
        crate::repos::checks::agent_location_with(&full, &agent_options, sas_vending)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    let sas_expires_at_ms = expires_at
        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_millis()).ok());
    Ok(WriterMaterial {
        writer_keys_text,
        location: RepoLocation {
            backend: for_agent.backend,
            options: for_agent.options,
        },
        agent_creds,
        sas_expires_at_ms,
    })
}

/// Keeps on the host the latest expiry of a SAS sent to it; a smaller value
/// never replaces a larger one.
pub async fn record_sas_issued(
    state: &AppState,
    agent_id: &str,
    expires_at_ms: i64,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE agents SET sas_expires_at = CASE WHEN sas_expires_at IS NULL \
         OR sas_expires_at < $1 THEN $1 ELSE sas_expires_at END WHERE id = $2",
    )
    .bind(expires_at_ms)
    .bind(agent_id)
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct RepoFull {
    pub id: String,
    pub name: String,
    pub backend: String,
    pub options: BTreeMap<String, String>,
    pub agent_key_state: String,
    pub checks_json: Option<String>,
    pub checked_at: Option<i64>,
    pub guarantee_level: String,
    pub guarantee_accepted: String,
    pub guarantee_notes_json: String,
    pub guarantee_checked_at: Option<i64>,
    pub open_error: Option<String>,
}

fn full_row(r: &sqlx::any::AnyRow) -> anyhow::Result<RepoFull> {
    Ok(RepoFull {
        id: r.try_get("id")?,
        name: r.try_get("name")?,
        backend: r.try_get("backend")?,
        options: serde_json::from_str(&r.try_get::<String, _>("location_json")?)?,
        agent_key_state: r.try_get("agent_key_state")?,
        checks_json: r.try_get("checks_json")?,
        checked_at: r.try_get("checked_at")?,
        guarantee_level: r.try_get("guarantee_level")?,
        guarantee_accepted: r.try_get("guarantee_accepted")?,
        guarantee_notes_json: r.try_get("guarantee_notes_json")?,
        guarantee_checked_at: r.try_get("guarantee_checked_at")?,
        open_error: r.try_get("open_error")?,
    })
}

pub async fn list_repositories(state: &AppState) -> anyhow::Result<Vec<RepoFull>> {
    sqlx::query(
        "SELECT id, name, backend, location_json, agent_key_state, checks_json, \
         checked_at, guarantee_level, guarantee_accepted, guarantee_notes_json, \
         guarantee_checked_at, open_error FROM repositories ORDER BY created_at, id",
    )
    .fetch_all(&state.db.pool)
    .await?
    .iter()
    .map(full_row)
    .collect()
}

pub async fn find_repository(state: &AppState, id: &str) -> anyhow::Result<Option<RepoFull>> {
    sqlx::query(
        "SELECT id, name, backend, location_json, agent_key_state, checks_json, \
         checked_at, guarantee_level, guarantee_accepted, guarantee_notes_json, \
         guarantee_checked_at, open_error FROM repositories WHERE id = $1",
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

/// A measured level to store, with the accepted level already decided from
/// `read_accepted`, the accepted level stored when the decision was made.
pub struct GuaranteeWrite {
    pub level: String,
    pub notes_json: String,
    pub accepted: String,
    pub read_accepted: String,
}

/// One check to store. `generation` is the access generation read together
/// with the credentials the probe used; `at_ms` is when the probe started and
/// `now_ms` is the same clock at the write. A stored check later than `now_ms`
/// was written by a wrong clock and does not block this one.
pub struct CheckWrite<'a> {
    pub generation: i64,
    pub at_ms: i64,
    pub now_ms: i64,
    pub checks_json: &'a str,
    pub agent_key_state: &'a str,
    pub guarantee: Option<&'a GuaranteeWrite>,
    pub daily: bool,
    /// The host key mismatch column, as [`mismatch_update`] decides it.
    pub host_key_mismatch: Option<Option<String>>,
}

/// What [`update_checks`] did with a check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stored {
    Written,
    /// The accepted level is no longer the one the write was decided from.
    AcceptedChanged,
    Stale,
    Missing,
}

/// Stores a check. With a level, nothing is written when the accepted level
/// is no longer `read_accepted`: someone accepted a level meanwhile, and the
/// caller decides again from the new value.
pub async fn update_checks(
    state: &AppState,
    id: &str,
    write: &CheckWrite<'_>,
) -> anyhow::Result<Stored> {
    let daily = i64::from(write.daily);
    let set_mismatch = i64::from(write.host_key_mismatch.is_some());
    let mismatch = write.host_key_mismatch.clone().flatten();
    let done =
        match write.guarantee {
            None => sqlx::query(
                "UPDATE repositories SET checks_json = $1, agent_key_state = $2, checked_at = $3, \
                 guarantee_tick_at = CASE WHEN $5 = 1 THEN $3 ELSE guarantee_tick_at END, \
                 guarantee_unchecked_runs = CASE WHEN $5 = 1 \
                 THEN guarantee_unchecked_runs + 1 ELSE guarantee_unchecked_runs END, \
                 guarantee_failed_runs = CASE WHEN $5 = 1 THEN 0 ELSE guarantee_failed_runs END, \
                 host_key_mismatch_json = CASE WHEN $8 = 1 THEN $9 ELSE host_key_mismatch_json END \
                 WHERE id = $4 AND access_generation = $6 \
                 AND (checked_at IS NULL OR checked_at <= $3 OR checked_at > $7)",
            )
            .bind(write.checks_json)
            .bind(write.agent_key_state)
            .bind(write.at_ms)
            .bind(id)
            .bind(daily)
            .bind(write.generation)
            .bind(write.now_ms)
            .bind(set_mismatch)
            .bind(mismatch)
            .execute(&state.db.pool)
            .await?,
            Some(g) => sqlx::query(
                "UPDATE repositories SET checks_json = $1, agent_key_state = $2, checked_at = $3, \
                 guarantee_level = $4, guarantee_notes_json = $5, guarantee_accepted = $6, \
                 guarantee_checked_at = $3, \
                 guarantee_tick_at = CASE WHEN $9 = 1 THEN $3 ELSE guarantee_tick_at END, \
                 guarantee_unchecked_runs = 0, guarantee_failed_runs = 0, \
                 host_key_mismatch_json = CASE WHEN $12 = 1 THEN $13 ELSE host_key_mismatch_json END \
                 WHERE id = $7 AND guarantee_accepted = $8 AND access_generation = $10 \
                 AND (checked_at IS NULL OR checked_at <= $3 OR checked_at > $11)",
            )
            .bind(write.checks_json)
            .bind(write.agent_key_state)
            .bind(write.at_ms)
            .bind(&g.level)
            .bind(&g.notes_json)
            .bind(&g.accepted)
            .bind(id)
            .bind(&g.read_accepted)
            .bind(daily)
            .bind(write.generation)
            .bind(write.now_ms)
            .bind(set_mismatch)
            .bind(mismatch)
            .execute(&state.db.pool)
            .await?,
        };
    if done.rows_affected() > 0 {
        return Ok(Stored::Written);
    }
    let row = sqlx::query("SELECT access_generation, checked_at FROM repositories WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db.pool)
        .await?;
    Ok(match row {
        None => Stored::Missing,
        Some(row) => {
            let checked_at: Option<i64> = row.try_get("checked_at")?;
            if row.try_get::<i64, _>("access_generation")? != write.generation
                || checked_at.is_some_and(|at| at > write.at_ms && at <= write.now_ms)
            {
                Stored::Stale
            } else {
                Stored::AcceptedChanged
            }
        }
    })
}

/// What a probe says about the host key mismatch column: `None` leaves it,
/// `Some(None)` clears it, `Some(Some(json))` records the two fingerprints.
/// The column clears only when the server answered a write again, so a probe
/// that failed for another reason keeps a standing alert.
pub fn mismatch_update(change: Option<&HostKeyChange>, reached: bool) -> Option<Option<String>> {
    match change {
        Some(c) => Some(Some(
            serde_json::json!({ "pinned": c.pinned, "offered": c.offered }).to_string(),
        )),
        None => reached.then_some(None),
    }
}

/// Records that the daily recheck failed before storing a result.
pub async fn record_failed_recheck(state: &AppState, id: &str, at_ms: i64) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE repositories SET guarantee_tick_at = $1, \
         guarantee_failed_runs = guarantee_failed_runs + 1 WHERE id = $2",
    )
    .bind(at_ms)
    .bind(id)
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

/// Makes the measured level the accepted one when it is still `expected`;
/// false when there is none to accept or it is another level by now.
pub async fn accept_measured_guarantee(
    state: &AppState,
    id: &str,
    expected: &str,
) -> anyhow::Result<bool> {
    let done = sqlx::query(
        "UPDATE repositories SET guarantee_accepted = guarantee_level \
         WHERE id = $1 AND guarantee_level <> 'unchecked' AND guarantee_level = $2",
    )
    .bind(id)
    .bind(expected)
    .execute(&state.db.pool)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// Everything the server needs to talk to a repository's storage and to open
/// the repository with its own key slot.
pub struct ServerAccess {
    pub location: lodestore_format::store::RepoLocation,
    pub server_creds: Option<AccessKey>,
    pub agent_creds: Option<AccessKey>,
    pub agent_options: AgentOptions,
    pub config_hash: [u8; 32],
    pub format_repo_id: [u8; 16],
    pub master: Zeroizing<Vec<u8>>,
    pub server_slot_secret: Option<Zeroizing<Vec<u8>>>,
    pub open_error: Option<String>,
    pub generation: i64,
    pub sas_vending: bool,
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
         server_slot_secret_id, server_creds_secret_id, agent_creds_secret_id, open_error, \
         access_generation, agent_sas_vending FROM repositories WHERE id = $1",
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
    let (agent_creds, agent_options) =
        match row.try_get::<Option<String>, _>("agent_creds_secret_id")? {
            Some(sid) => {
                let sealed: SealedAccess =
                    serde_json::from_slice(&state.sealer.get_secret(&sid).await?)?;
                (sealed.key(), sealed.agent_options())
            }
            None => (None, AgentOptions::new()),
        };
    let server_slot_secret = match row.try_get::<Option<String>, _>("server_slot_secret_id")? {
        Some(sid) => Some(state.sealer.get_secret(&sid).await?),
        None => None,
    };
    Ok(ServerAccess {
        location: lodestore_format::store::RepoLocation { backend, options },
        server_creds,
        agent_creds,
        agent_options,
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
        generation: row.try_get("access_generation")?,
        sas_vending: row.try_get::<i64, _>("agent_sas_vending")? != 0,
    })
}

/// Takes the write lock before the first read of a transaction: a read first
/// fails with "database is locked" on SQLite when another connection commits
/// in between. Returns the rows touched, 0 when the repository is missing.
async fn take_write_lock(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    id: &str,
) -> anyhow::Result<u64> {
    Ok(
        sqlx::query("UPDATE repositories SET access_generation = access_generation WHERE id = $1")
            .bind(id)
            .execute(&mut **tx)
            .await?
            .rows_affected(),
    )
}

/// Swaps the sealed agent key for a new one in one transaction and records
/// the outcome of the check that approved it, including the level it measured.
/// Changes nothing and returns false when the accepted level is no longer
/// `read_accepted`, as [`update_checks`] does.
pub async fn replace_agent_creds(
    state: &AppState,
    id: &str,
    key: &AccessKey,
    agent_key_state: &str,
    checks_json: &str,
    guarantee: &GuaranteeWrite,
) -> anyhow::Result<bool> {
    let mut tx = state.db.pool.begin().await?;
    if take_write_lock(&mut tx, id).await? == 0 {
        anyhow::bail!("repository {id} is missing");
    }
    let old = sqlx::query("SELECT agent_creds_secret_id FROM repositories WHERE id = $1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?
        .try_get::<Option<String>, _>("agent_creds_secret_id")?;
    let new = state
        .sealer
        .put_secret_on(&mut tx, "repo-agent-creds", &agent_json(key)?)
        .await?;
    let done = sqlx::query(
        "UPDATE repositories SET agent_creds_secret_id = $1, agent_key_state = $2, \
         checks_json = $3, checked_at = $4, guarantee_level = $5, guarantee_notes_json = $6, \
         guarantee_accepted = $7, guarantee_checked_at = $4, \
         access_generation = access_generation + 1, guarantee_unchecked_runs = 0, \
         guarantee_failed_runs = 0 WHERE id = $8 AND guarantee_accepted = $9",
    )
    .bind(new)
    .bind(agent_key_state)
    .bind(checks_json)
    .bind(now_ms())
    .bind(&guarantee.level)
    .bind(&guarantee.notes_json)
    .bind(&guarantee.accepted)
    .bind(id)
    .bind(&guarantee.read_accepted)
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() == 0 {
        return Ok(false);
    }
    if let Some(old) = old {
        sqlx::query("DELETE FROM secrets WHERE id = $1")
            .bind(old)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// Replaces the sealed agent options after a probe with them reached the
/// storage. The write needs the access generation the probe read with its
/// credentials and the accepted level `guarantee` was decided from; it bumps
/// the generation, so a probe that started with the old options stores nothing.
/// The agent's S3 key, if any, is kept.
pub async fn replace_agent_options(
    state: &AppState,
    id: &str,
    generation: i64,
    options: &AgentOptions,
    agent_key_state: &str,
    checks_json: &str,
    guarantee: &GuaranteeWrite,
) -> anyhow::Result<Stored> {
    let mut tx = state.db.pool.begin().await?;
    if take_write_lock(&mut tx, id).await? == 0 {
        return Ok(Stored::Missing);
    }
    let old = sqlx::query("SELECT agent_creds_secret_id FROM repositories WHERE id = $1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?
        .try_get::<Option<String>, _>("agent_creds_secret_id")?;
    let key = match &old {
        Some(sid) => {
            let sealed: SealedAccess =
                serde_json::from_slice(&state.sealer.get_secret(sid).await?)?;
            sealed.key()
        }
        None => None,
    };
    let plain: BTreeMap<String, String> = options
        .iter()
        .map(|(k, v)| (k.clone(), v.to_string()))
        .collect();
    let new = state
        .sealer
        .put_secret_on(
            &mut tx,
            "repo-agent-creds",
            &sealed_json(key.as_ref(), &plain)?,
        )
        .await?;
    let done = sqlx::query(
        "UPDATE repositories SET agent_creds_secret_id = $1, agent_key_state = $2, \
         checks_json = $3, checked_at = $4, guarantee_level = $5, guarantee_notes_json = $6, \
         guarantee_accepted = $7, guarantee_checked_at = $4, guarantee_unchecked_runs = 0, \
         guarantee_failed_runs = 0, access_generation = access_generation + 1 \
         WHERE id = $8 AND guarantee_accepted = $9 AND access_generation = $10",
    )
    .bind(new)
    .bind(agent_key_state)
    .bind(checks_json)
    .bind(now_ms())
    .bind(&guarantee.level)
    .bind(&guarantee.notes_json)
    .bind(&guarantee.accepted)
    .bind(id)
    .bind(&guarantee.read_accepted)
    .bind(generation)
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() == 0 {
        let current: Option<i64> =
            sqlx::query("SELECT access_generation FROM repositories WHERE id = $1")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
                .map(|r| r.try_get("access_generation"))
                .transpose()?;
        return Ok(match current {
            None => Stored::Missing,
            Some(c) if c == generation => Stored::AcceptedChanged,
            Some(_) => Stored::Stale,
        });
    }
    if let Some(old) = old {
        sqlx::query("DELETE FROM secrets WHERE id = $1")
            .bind(old)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(Stored::Written)
}

/// Pins a new host key after a probe with it reached the storage. The write
/// needs the access generation the probe read with its credentials and the
/// accepted level `guarantee` was decided from; it clears the host key
/// mismatch and bumps the generation, so a probe that started with the old key
/// stores nothing.
pub async fn replace_host_key(
    state: &AppState,
    id: &str,
    generation: i64,
    host_key: &str,
    agent_key_state: &str,
    checks_json: &str,
    guarantee: &GuaranteeWrite,
) -> anyhow::Result<Stored> {
    let mut tx = state.db.pool.begin().await?;
    if take_write_lock(&mut tx, id).await? == 0 {
        return Ok(Stored::Missing);
    }
    let row = sqlx::query("SELECT location_json FROM repositories WHERE id = $1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let mut options: BTreeMap<String, String> =
        serde_json::from_str(&row.try_get::<String, _>("location_json")?)?;
    options.insert("host_key".to_string(), host_key.to_string());
    let done = sqlx::query(
        "UPDATE repositories SET location_json = $1, agent_key_state = $2, checks_json = $3, \
         checked_at = $4, guarantee_level = $5, guarantee_notes_json = $6, \
         guarantee_accepted = $7, guarantee_checked_at = $4, guarantee_unchecked_runs = 0, \
         guarantee_failed_runs = 0, host_key_mismatch_json = NULL, \
         access_generation = access_generation + 1 \
         WHERE id = $8 AND guarantee_accepted = $9 AND access_generation = $10",
    )
    .bind(serde_json::to_string(&options)?)
    .bind(agent_key_state)
    .bind(checks_json)
    .bind(now_ms())
    .bind(&guarantee.level)
    .bind(&guarantee.notes_json)
    .bind(&guarantee.accepted)
    .bind(id)
    .bind(&guarantee.read_accepted)
    .bind(generation)
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() > 0 {
        tx.commit().await?;
        return Ok(Stored::Written);
    }
    let current: i64 = sqlx::query("SELECT access_generation FROM repositories WHERE id = $1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?
        .try_get("access_generation")?;
    Ok(if current == generation {
        Stored::AcceptedChanged
    } else {
        Stored::Stale
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_location_debug_hides_secrets() {
        let location = RepoLocation {
            backend: "azblob".into(),
            options: BTreeMap::from([
                ("container".into(), "visible-container".into()),
                ("account_key".into(), "SECRET-KEY".into()),
                ("sas_token".into(), "SECRET-SAS".into()),
                ("encryption_key".into(), "SECRET-ENC".into()),
                ("key".into(), "SECRET-K".into()),
                ("credential".into(), "SECRET-C".into()),
                ("token".into(), "SECRET-T".into()),
            ]),
        };
        let shown = format!("{location:?}");
        assert!(shown.contains("visible-container"), "{shown}");
        assert!(!shown.contains("SECRET"), "{shown}");
        assert!(shown.contains("<redacted>"), "{shown}");
    }

    #[test]
    fn mismatch_update_follows_the_table() {
        let change = HostKeyChange {
            pinned: "SHA256:old".into(),
            offered: "SHA256:new".into(),
        };
        let json = r#"{"offered":"SHA256:new","pinned":"SHA256:old"}"#;
        for reached in [true, false] {
            assert_eq!(
                mismatch_update(Some(&change), reached),
                Some(Some(json.to_string()))
            );
        }
        assert_eq!(mismatch_update(None, true), Some(None));
        assert_eq!(mismatch_update(None, false), None);
    }

    #[test]
    fn sealed_access_reads_a_legacy_agent_key() {
        let legacy = serde_json::to_vec(&AccessKey {
            access_key_id: "a".into(),
            secret_access_key: "b".into(),
        })
        .unwrap();
        let sealed: SealedAccess = serde_json::from_slice(&legacy).unwrap();
        let key = sealed.key().unwrap();
        assert_eq!(
            (key.access_key_id.as_str(), key.secret_access_key.as_str()),
            ("a", "b")
        );
        assert!(sealed.agent_options().is_empty());

        let only_options =
            sealed_json(None, &BTreeMap::from([("key".into(), "k".into())])).unwrap();
        let sealed: SealedAccess = serde_json::from_slice(&only_options).unwrap();
        assert!(sealed.key().is_none());
        assert_eq!(sealed.agent_options()["key"].as_str(), "k");
    }
}
