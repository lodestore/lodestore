use std::collections::HashSet;
use std::sync::Arc;

use anyhow::anyhow;
use lodestore_format::Error as FormatError;
use lodestore_format::index::parse_hex32;
use lodestore_format::pb;
use sqlx::Row;
use tokio::sync::OwnedSemaphorePermit;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::api::{FindRequest, FindState, FindStop};
use super::cache;
use super::{Group, Session, SessionState, Version, VersionKey};
use crate::browse::AGENTLESS;
use crate::browse::tree::{self, EntryKind, kind_of, mtime_ms};
use crate::db::now_ms;
use crate::hosts::labels::Label;
use crate::state::AppState;

#[derive(Debug)]
pub struct Entry {
    pub path: Vec<u8>,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime_ms: i64,
    pub content_id: Option<[u8; 16]>,
}

pub struct SnapshotRow {
    pub id: String,
    pub repository_id: String,
    pub host_key: String,
    pub host_id: Option<String>,
    pub hostname: String,
    pub labels: Vec<Label>,
    /// When the server received the snapshot; the manifest time is chosen by the host.
    pub time_ms: i64,
    pub verified_by: Option<String>,
    /// The host and the source (policy, stdin name or paths) whose snapshots follow each other.
    pub lineage: String,
    pub incomplete: bool,
}

/// `key=value` pairs of a label filter; `None` when one lacks the `=` or a key.
pub fn parse_labels(labels: &[String]) -> Option<Vec<(&str, &str)>> {
    labels
        .iter()
        .map(|l| l.split_once('=').filter(|(key, _)| !key.is_empty()))
        .collect()
}

enum Bind {
    Text(String),
    Time(i64),
}

/// Live snapshots of the hosts in `hosts` or with all of `labels` (a union; all hosts when both
/// are empty), within the period, newest first.
pub async fn select_snapshots(
    state: &AppState,
    query: &FindRequest,
) -> Result<Vec<SnapshotRow>, sqlx::Error> {
    let mut binds: Vec<Bind> = Vec::new();
    let mut clauses: Vec<String> = Vec::new();
    let mut hosts: Vec<String> = Vec::new();
    let wanted = parse_labels(&query.labels).unwrap_or_default();
    let sql_hosts: &[String] = if wanted.is_empty() { &query.hosts } else { &[] };
    for host in sql_hosts {
        if let Some(hostname) = host.strip_prefix(AGENTLESS) {
            binds.push(Bind::Text(hostname.to_string()));
            hosts.push(format!(
                "(s.host_id IS NULL AND s.hostname = ${})",
                binds.len()
            ));
        } else {
            binds.push(Bind::Text(host.clone()));
            hosts.push(format!("s.host_id = ${}", binds.len()));
        }
    }
    if !hosts.is_empty() {
        clauses.push(format!("({})", hosts.join(" OR ")));
    }
    if let Some(since) = query.since {
        binds.push(Bind::Time(since));
        clauses.push(format!("s.received_at >= ${}", binds.len()));
    }
    if let Some(until) = query.until {
        binds.push(Bind::Time(until));
        clauses.push(format!("s.received_at <= ${}", binds.len()));
    }
    let filter = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {} ", clauses.join(" AND "))
    };
    let sql = format!(
        "SELECT s.id, s.repository_id, s.host_id, s.hostname AS snap_hostname, \
         COALESCE(a.hostname, s.hostname) AS hostname, a.labels_json, s.received_at AS time_ms, s.verified_by, \
         s.policy_name, s.stdin_name, s.paths_json, s.incomplete \
         FROM snapshots s LEFT JOIN agents a ON a.id = s.host_id \
         {filter}ORDER BY s.received_at DESC, s.id DESC"
    );
    let mut request = sqlx::query(sqlx::AssertSqlSafe(sql));
    for bind in binds {
        request = match bind {
            Bind::Text(text) => request.bind(text),
            Bind::Time(time) => request.bind(time),
        };
    }
    let rows = request.fetch_all(&state.db.pool).await?;
    let mut snapshots = rows
        .iter()
        .map(|row| {
            let host_id: Option<String> = row.try_get("host_id")?;
            let snap_hostname: String = row.try_get("snap_hostname")?;
            let labels: Option<String> = row.try_get("labels_json")?;
            let host_key = host_id
                .clone()
                .unwrap_or_else(|| format!("{AGENTLESS}{snap_hostname}"));
            let source = match (
                row.try_get::<Option<String>, _>("policy_name")?,
                row.try_get::<Option<String>, _>("stdin_name")?,
            ) {
                (Some(policy), _) => format!("policy:{policy}"),
                (None, Some(name)) => format!("stdin:{name}"),
                (None, None) => format!("paths:{}", row.try_get::<String, _>("paths_json")?),
            };
            Ok(SnapshotRow {
                id: row.try_get("id")?,
                repository_id: row.try_get("repository_id")?,
                lineage: format!("{host_key}\n{source}"),
                incomplete: row.try_get::<i64, _>("incomplete")? != 0,
                host_key,
                host_id,
                hostname: row.try_get("hostname")?,
                labels: labels
                    .and_then(|json| serde_json::from_str(&json).ok())
                    .unwrap_or_default(),
                time_ms: row.try_get("time_ms")?,
                verified_by: row.try_get("verified_by")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
    if !wanted.is_empty() {
        let hosts: HashSet<&str> = query.hosts.iter().map(String::as_str).collect();
        snapshots.retain(|snapshot| {
            hosts.contains(snapshot.host_key.as_str())
                || wanted.iter().all(|(key, value)| {
                    snapshot
                        .labels
                        .iter()
                        .any(|l| l.key == *key && l.value == *value)
                })
        });
    }
    Ok(snapshots)
}

/// The most memory one snapshot's walk may hold: paths plus the fixed size of each entry.
pub const MAX_WALK_BYTES: usize = 512 << 20;

/// Files and symlinks of one snapshot: from the cache of path lists when it has them, else from
/// a full walk of the trees, which then fills the cache.
pub async fn snapshot_entries(
    state: &AppState,
    repository_id: &str,
    snapshot_id: &str,
    cancel: &CancellationToken,
) -> anyhow::Result<Vec<Entry>> {
    let id = parse_hex32(snapshot_id).ok_or_else(|| anyhow!("the snapshot id is not valid"))?;
    let dir = cache::dir(&state.config.data_dir);
    let sealer = state.sealer.clone();
    let max_bytes = state.config.find.cache_max_bytes;
    let cached = {
        let (dir, sealer, snapshot_id) = (dir.clone(), sealer.clone(), snapshot_id.to_string());
        tokio::task::spawn_blocking(move || cache::load(&sealer, &dir, &snapshot_id, max_bytes))
            .await?
    };
    if let Some(entries) = cached {
        return Ok(entries);
    }
    let mut reader = tree::reader(state, repository_id).await?;
    // The visitor of `walk_subtree` is not `Send`, so the walk gets a thread of its own.
    let handle = tokio::runtime::Handle::current();
    let cancel = cancel.clone();
    let entries = tokio::task::spawn_blocking(move || {
        handle.block_on(async move {
            let mut entries = Vec::new();
            let mut budget = MAX_WALK_BYTES;
            reader
                .walk_subtree(&id, b"", &mut |path: &[u8], node: &pb::Node| {
                    collect(&mut entries, &mut budget, &cancel, path, node)
                })
                .await?;
            anyhow::Ok(entries)
        })
    })
    .await??;
    let entries = {
        let (dir, snapshot_id) = (dir.clone(), snapshot_id.to_string());
        tokio::task::spawn_blocking(move || {
            match cache::store(&sealer, &dir, &snapshot_id, &entries) {
                Ok(len) if len > max_bytes => cache::remove(&dir, &snapshot_id),
                Ok(_) => cache::evict(&dir, max_bytes),
                Err(error) => {
                    tracing::warn!(snapshot = %snapshot_id, %error, "find: cache not written")
                }
            }
            entries
        })
        .await?
    };
    // Retention may have forgotten the snapshot and removed its list while the walk ran.
    let live = sqlx::query("SELECT 1 FROM snapshots WHERE id = $1")
        .bind(snapshot_id)
        .fetch_optional(&state.db.pool)
        .await?;
    if live.is_none() {
        cache::remove(&dir, snapshot_id);
    }
    Ok(entries)
}

/// Keeps the files and symlinks a walk visits; stops the walk once the search is stopped or the
/// snapshot holds more than `budget` bytes of entries.
fn collect(
    entries: &mut Vec<Entry>,
    budget: &mut usize,
    cancel: &CancellationToken,
    path: &[u8],
    node: &pb::Node,
) -> lodestore_format::error::Result<()> {
    if cancel.is_cancelled() {
        return Err(FormatError::Interrupted);
    }
    *budget = budget
        .checked_sub(path.len() + size_of::<Entry>())
        .ok_or_else(|| {
            FormatError::InvalidInput("the snapshot holds more paths than a search keeps".into())
        })?;
    let kind = kind_of(node);
    if matches!(kind, EntryKind::File | EntryKind::Symlink) {
        entries.push(entry_of(path, kind, node));
    }
    Ok(())
}

fn entry_of(path: &[u8], kind: EntryKind, node: &pb::Node) -> Entry {
    let mut content_id = [0u8; 16];
    content_id.copy_from_slice(&blake3::hash(&node.content.concat()).as_bytes()[..16]);
    Entry {
        path: path.to_vec(),
        kind,
        size: node.size,
        mtime_ms: mtime_ms(node),
        content_id: (!node.content.is_empty()).then_some(content_id),
    }
}

enum Outcome {
    Complete,
    Failed(anyhow::Error),
    TimedOut,
    Stopped,
}

pub async fn run(state: AppState, session: Arc<Session>) {
    let outcome = tokio::select! {
        () = session.cancel.cancelled() => Outcome::Stopped,
        outcome = scan(&state, &session) => outcome,
    };
    // Ends the walks still running and turns late scan tasks into no-ops.
    session.cancel.cancel();
    let mut s = session.lock();
    if matches!(outcome, Outcome::TimedOut) {
        s.stop_reason = Some(FindStop::Timeout);
    }
    s.state = match outcome {
        Outcome::Failed(error) => {
            tracing::warn!(session = %session.id, %error, "find: search failed");
            s.error = Some("the search failed; see the server log".into());
            FindState::Failed
        }
        _ if s.stop_reason.is_some() => FindState::Incomplete,
        Outcome::Stopped => FindState::Cancelled,
        _ => FindState::Done,
    };
    s.finished_at = Some(now_ms());
    s.revision += 1;
    let revision = s.revision;
    for times in s.scanned_times.values_mut() {
        times.sort_unstable();
    }
    let gone: Vec<usize> = (0..s.groups.len())
        .filter(|&i| s.next_scanned_after(&s.groups[i]).is_some())
        .collect();
    for i in gone {
        s.groups[i].revision = revision;
    }
}

/// The time limit starts once the search has a running slot, not while it waits for one.
async fn scan(state: &AppState, session: &Arc<Session>) -> Outcome {
    let _slot = match state.find.running.clone().acquire_owned().await {
        Ok(slot) => slot,
        Err(error) => return Outcome::Failed(error.into()),
    };
    match tokio::time::timeout(state.config.find.timeout, scan_all(state, session)).await {
        Ok(Ok(())) => Outcome::Complete,
        Ok(Err(error)) => Outcome::Failed(error),
        Err(_) => Outcome::TimedOut,
    }
}

async fn scan_all(state: &AppState, session: &Arc<Session>) -> anyhow::Result<()> {
    let snapshots = select_snapshots(state, &session.query).await?;
    {
        let mut s = session.lock();
        s.total = snapshots.len() as u64;
        s.searched_hosts = snapshots
            .iter()
            .map(|snapshot| snapshot.host_key.as_str())
            .collect::<HashSet<_>>()
            .len() as u64;
    }
    let mut tasks = JoinSet::new();
    for snapshot in snapshots {
        let permit = state.find.walks.clone().acquire_owned().await?;
        tasks.spawn(scan_snapshot(
            state.clone(),
            Arc::clone(session),
            snapshot,
            permit,
        ));
        while let Some(joined) = tasks.try_join_next() {
            note_join(session, joined);
        }
    }
    while let Some(joined) = tasks.join_next().await {
        note_join(session, joined);
    }
    Ok(())
}

fn note_join(session: &Session, joined: Result<(), tokio::task::JoinError>) {
    if joined.is_err() {
        let mut s = session.lock();
        s.scanned += 1;
        s.skipped += 1;
    }
}

async fn scan_snapshot(
    state: AppState,
    session: Arc<Session>,
    snapshot: SnapshotRow,
    _permit: OwnedSemaphorePermit,
) {
    let found = match snapshot_entries(
        &state,
        &snapshot.repository_id,
        &snapshot.id,
        &session.cancel,
    )
    .await
    {
        Ok(entries) => {
            let session = Arc::clone(&session);
            tokio::task::spawn_blocking(move || {
                entries
                    .into_iter()
                    .filter(|e| session.matcher.matches(&e.path))
                    .collect::<Vec<_>>()
            })
            .await
            .map_err(anyhow::Error::from)
        }
        Err(error) => Err(error),
    };
    let mut s = session.lock();
    if session.cancel.is_cancelled() {
        return;
    }
    s.scanned += 1;
    match found {
        Ok(hits) => {
            if !snapshot.incomplete {
                s.scanned_times
                    .entry(snapshot.lineage.clone())
                    .or_default()
                    .push(snapshot.time_ms);
            }
            let mut full = false;
            for entry in &hits {
                full |= !merge(&mut s, &snapshot, entry, state.config.find.max_matches);
            }
            if full {
                session.cancel.cancel();
            }
        }
        Err(error) => {
            tracing::warn!(snapshot = %snapshot.id, %error, "find: snapshot skipped");
            s.skipped += 1;
        }
    }
}

/// Adds one hit; false when it would be a new group past the limit, which is dropped.
fn merge(s: &mut SessionState, snapshot: &SnapshotRow, entry: &Entry, max_groups: u64) -> bool {
    let key = (snapshot.host_key.clone(), entry.path.clone());
    let index = match s.index.get(&key) {
        Some(&index) => index,
        None => {
            if s.groups.len() as u64 >= max_groups {
                s.stop_reason = Some(FindStop::Limit);
                return false;
            }
            s.groups.push(Group {
                host_key: snapshot.host_key.clone(),
                host_id: snapshot.host_id.clone(),
                hostname: snapshot.hostname.clone(),
                labels: snapshot.labels.clone(),
                path: entry.path.clone(),
                last_seen_ms: snapshot.time_ms,
                lineage: snapshot.lineage.clone(),
                versions: Vec::new(),
                revision: 0,
            });
            s.index.insert(key, s.groups.len() - 1);
            s.groups.len() - 1
        }
    };
    let version_key = entry.content_id.map_or(
        VersionKey::Meta {
            size: entry.size,
            mtime_ms: entry.mtime_ms,
        },
        VersionKey::Content,
    );
    s.revision += 1;
    let revision = s.revision;
    let group = &mut s.groups[index];
    group.revision = revision;
    if snapshot.time_ms >= group.last_seen_ms {
        group.last_seen_ms = snapshot.time_ms;
        group.lineage.clone_from(&snapshot.lineage);
    }
    let at = match group.versions.iter().position(|v| v.key == version_key) {
        Some(at) => at,
        None => {
            group.versions.push(Version {
                key: version_key,
                kind: entry.kind,
                size: entry.size,
                mtime_ms: entry.mtime_ms,
                first_ms: snapshot.time_ms,
                last_ms: snapshot.time_ms,
                verified_by: None,
                snapshots: Vec::new(),
            });
            group.versions.len() - 1
        }
    };
    let version = &mut group.versions[at];
    version.first_ms = version.first_ms.min(snapshot.time_ms);
    version.last_ms = version.last_ms.max(snapshot.time_ms);
    version.verified_by = version.verified_by.take().max(snapshot.verified_by.clone());
    version
        .snapshots
        .push((snapshot.id.clone(), snapshot.time_ms));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> pb::Node {
        pb::Node {
            r#type: pb::NodeType::File as i32,
            ..pb::Node::default()
        }
    }

    #[test]
    fn collect_stops_past_the_budget() {
        let (mut entries, cancel) = (Vec::new(), CancellationToken::new());
        let mut budget = 2 * (4 + size_of::<Entry>());
        for path in [b"/a/1", b"/a/2"] {
            collect(&mut entries, &mut budget, &cancel, path, &file()).unwrap();
        }
        let over = collect(&mut entries, &mut budget, &cancel, b"/a/3", &file());
        assert!(matches!(over, Err(FormatError::InvalidInput(_))));
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn collect_stops_once_cancelled() {
        let (mut entries, cancel) = (Vec::new(), CancellationToken::new());
        let mut budget = MAX_WALK_BYTES;
        cancel.cancel();
        let stopped = collect(&mut entries, &mut budget, &cancel, b"/a", &file());
        assert!(matches!(stopped, Err(FormatError::Interrupted)));
        assert!(entries.is_empty());
    }
}
