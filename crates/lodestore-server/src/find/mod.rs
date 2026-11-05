use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::browse::tree::EntryKind;
use crate::db::now_ms;
use crate::hosts::labels::Label;
use crate::state::AppState;

pub mod api;
pub mod cache;
pub mod pattern;
pub mod walk;

use api::{FindRequest, FindState, FindStop};
use pattern::Pattern;

pub const SESSION_TTL_MS: i64 = 3_600_000;
pub const MAX_KEPT_SESSIONS: usize = 32;
pub const JANITOR_PERIOD: Duration = Duration::from_secs(60);
const SWEEP_EVERY_TICKS: u32 = 60;
pub const RUNNING_SESSIONS: usize = 2;
pub const WALK_PERMITS: usize = 4;
pub const MAX_UNFINISHED_SESSIONS: usize = 8;
pub const SNAPSHOT_CHIPS: usize = 6;

#[derive(Clone)]
pub struct FindSessions {
    sessions: Arc<Mutex<HashMap<String, Arc<Session>>>>,
    pub(crate) running: Arc<Semaphore>,
    pub(crate) walks: Arc<Semaphore>,
}

impl Default for FindSessions {
    fn default() -> Self {
        Self::new()
    }
}

impl FindSessions {
    pub fn new() -> Self {
        FindSessions {
            sessions: Arc::default(),
            running: Arc::new(Semaphore::new(RUNNING_SESSIONS)),
            walks: Arc::new(Semaphore::new(WALK_PERMITS)),
        }
    }

    /// Adds a search unless 8 are queued or running already.
    #[must_use]
    pub fn insert(&self, session: Arc<Session>) -> bool {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        let unfinished = sessions
            .values()
            .filter(|s| s.lock().finished_at.is_none())
            .count();
        if unfinished >= MAX_UNFINISHED_SESSIONS {
            return false;
        }
        sessions.insert(session.id.clone(), session);
        true
    }

    /// Forgets searches that ended over an hour ago and the oldest ended ones beyond 32; a
    /// search still running is never dropped.
    pub fn sweep(&self, now_ms: i64) {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        let mut ended: Vec<(String, i64)> = sessions
            .iter()
            .filter_map(|(id, s)| Some((id.clone(), s.lock().finished_at?)))
            .collect();
        ended.sort_by_key(|(_, at)| std::cmp::Reverse(*at));
        for (rank, (id, at)) in ended.into_iter().enumerate() {
            if now_ms - at > SESSION_TTL_MS || rank >= MAX_KEPT_SESSIONS {
                sessions.remove(&id);
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
    }
}

pub async fn janitor(state: AppState, stop: CancellationToken) -> anyhow::Result<()> {
    let mut tick = tokio::time::interval(JANITOR_PERIOD);
    let mut ticks = 0u32;
    loop {
        tokio::select! {
            () = stop.cancelled() => return Ok(()),
            _ = tick.tick() => {}
        }
        state.find.sweep(now_ms());
        if ticks.is_multiple_of(SWEEP_EVERY_TICKS)
            && let Err(error) = cache::sweep_orphans(&state).await
        {
            tracing::warn!(%error, "find: cache sweep failed");
        }
        ticks = ticks.wrapping_add(1);
    }
}

pub struct Session {
    pub id: String,
    pub query: FindRequest,
    pub matcher: Pattern,
    pub started_at: i64,
    pub cancel: CancellationToken,
    state: Mutex<SessionState>,
}

impl Session {
    pub fn new(id: String, query: FindRequest, matcher: Pattern, started_at: i64) -> Session {
        Session {
            id,
            query,
            matcher,
            started_at,
            cancel: CancellationToken::new(),
            state: Mutex::new(SessionState::default()),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, SessionState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum VersionKey {
    Content([u8; 16]),
    Meta { size: u64, mtime_ms: i64 },
}

pub struct Version {
    pub key: VersionKey,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime_ms: i64,
    pub first_ms: i64,
    pub last_ms: i64,
    /// Who signed the provenance of a snapshot holding this version; none when none is signed.
    pub verified_by: Option<String>,
    pub snapshots: Vec<(String, i64)>,
}

pub struct Group {
    pub host_key: String,
    pub host_id: Option<String>,
    pub hostname: String,
    pub labels: Vec<Label>,
    pub path: Vec<u8>,
    pub last_seen_ms: i64,
    /// The lineage of the newest snapshot holding the file.
    pub lineage: String,
    pub versions: Vec<Version>,
    pub revision: u64,
}

pub struct SessionState {
    pub state: FindState,
    pub total: u64,
    pub searched_hosts: u64,
    pub scanned: u64,
    pub skipped: u64,
    pub groups: Vec<Group>,
    pub index: HashMap<(String, Vec<u8>), usize>,
    pub revision: u64,
    pub error: Option<String>,
    pub finished_at: Option<i64>,
    pub stop_reason: Option<FindStop>,
    /// Times of the complete snapshots read without error, per lineage (host and source).
    pub scanned_times: HashMap<String, Vec<i64>>,
}

impl SessionState {
    /// The time of the nearest scanned snapshot of the same host and source that is newer
    /// than the last one holding the file, once the search is over.
    pub fn next_scanned_after(&self, group: &Group) -> Option<i64> {
        if self.state == FindState::Running {
            return None;
        }
        let times = self.scanned_times.get(&group.lineage)?;
        let at = times.partition_point(|&t| t <= group.last_seen_ms);
        times.get(at).copied()
    }
}

impl Default for SessionState {
    fn default() -> Self {
        SessionState {
            state: FindState::Running,
            total: 0,
            searched_hosts: 0,
            scanned: 0,
            skipped: 0,
            groups: Vec::new(),
            index: HashMap::new(),
            revision: 0,
            error: None,
            finished_at: None,
            stop_reason: None,
            scanned_times: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 10_000_000_000;
    const MINUTE: i64 = 60_000;

    fn session(id: &str, finished_at: Option<i64>) -> Arc<Session> {
        let query = FindRequest {
            pattern: "x".into(),
            hosts: Vec::new(),
            labels: Vec::new(),
            since: None,
            until: None,
        };
        let session = Session::new(id.into(), query, Pattern::compile("x").unwrap(), 0);
        session.lock().finished_at = finished_at;
        Arc::new(session)
    }

    #[test]
    fn sweep_drops_ended_sessions_after_an_hour() {
        let sessions = FindSessions::new();
        assert!(sessions.insert(session("old", Some(NOW - 61 * MINUTE))));
        assert!(sessions.insert(session("recent", Some(NOW - 59 * MINUTE))));
        assert!(sessions.insert(session("running", None)));
        sessions.sweep(NOW);
        assert!(sessions.get("old").is_none());
        assert!(sessions.get("recent").is_some());
        assert!(sessions.get("running").is_some());
    }

    #[test]
    fn insert_refuses_a_ninth_unfinished_session() {
        let sessions = FindSessions::new();
        assert!(sessions.insert(session("ended", Some(NOW))));
        for n in 0..MAX_UNFINISHED_SESSIONS {
            assert!(sessions.insert(session(&format!("r{n}"), None)));
        }
        assert!(!sessions.insert(session("extra", None)));
        assert!(sessions.get("extra").is_none());
    }

    #[test]
    fn sweep_keeps_the_newest_32_ended_sessions() {
        let sessions = FindSessions::new();
        for n in 0..33 {
            assert!(sessions.insert(session(&format!("s{n}"), Some(NOW - 50 * MINUTE + n))));
        }
        sessions.sweep(NOW);
        assert!(sessions.get("s0").is_none());
        for n in 1..33 {
            assert!(sessions.get(&format!("s{n}")).is_some(), "s{n}");
        }
    }
}
