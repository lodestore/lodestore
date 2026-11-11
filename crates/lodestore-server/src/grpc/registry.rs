use std::collections::{HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lodestore_format::crypto::{BlobId, Key32};
use lodestore_proto::agent::v1::server_message::Body;
use lodestore_proto::agent::v1::{Disconnect, DisconnectReason, ServerMessage};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::grpc::dispatch::live::Live;

pub const CAP_RESTORE: &str = "restore_v1";
pub const CAP_LIMITS: &str = "limits_v1";
pub const CAP_GC: &str = "gc_v1";
pub const CAP_UPDATE: &str = "update_v1";
pub const CAP_RESTORE_ROOTS: &str = "restore_roots_v1";
pub const CAP_SHARD_LIMIT: &str = "shard_limit_v1";
pub const ONLINE_WINDOW: Duration = Duration::from_secs(45);
pub const CONFLICT_WINDOW: Duration = Duration::from_secs(10 * 60);
pub const CONFLICT_REPLACEMENTS: usize = 5;
pub const SECURITY_EVENT_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const CHANNEL_DEPTH: usize = 16;
const SERIAL_MAX_CHARS: usize = 64;
const REJECT_LOG_EVERY: Duration = Duration::from_secs(10 * 60);

struct Entry {
    generation: u64,
    repository_id: String,
    last_seen: Instant,
    tx: mpsc::Sender<ServerMessage>,
    cancel: CancellationToken,
    capabilities: Vec<String>,
    peer: Option<IpAddr>,
}

/// A live session of an agent was replaced from another network, or live sessions were replaced over and over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionConflict {
    pub at_ms: i64,
    pub ip: Option<IpAddr>,
    pub previous_ip: Option<IpAddr>,
}

/// The server refused the certificate an agent presented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertRejection {
    pub at_ms: i64,
    pub serial: String,
    pub ip: Option<IpAddr>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SecurityEvents {
    pub conflict: Option<SessionConflict>,
    pub rejected: Option<CertRejection>,
}

struct Issued {
    generation: u64,
    packs: HashMap<u32, (BlobId, Key32)>,
}

#[derive(Default)]
struct Inner {
    next_generation: u64,
    sessions: HashMap<String, Entry>,
    revoked: HashSet<String>,
    issued: HashMap<String, Issued>,
    replacements: HashMap<String, VecDeque<Instant>>,
    security: HashMap<String, SecurityEvents>,
    reject_logged: HashMap<String, Instant>,
    unknown_logged: Option<Instant>,
    unknown_refusals: u64,
}

/// The same /56 rule as the login limits, so a host that moves within its prefix is not a conflict.
fn same_network(a: IpAddr, b: IpAddr) -> bool {
    match (a.to_canonical(), b.to_canonical()) {
        (IpAddr::V4(a), IpAddr::V4(b)) => a == b,
        (IpAddr::V6(a), IpAddr::V6(b)) => (u128::from(a) ^ u128::from(b)) >> 72 == 0,
        _ => false,
    }
}

#[derive(Clone, Default)]
pub struct Registry {
    inner: Arc<Mutex<Inner>>,
    pub live: Live,
}

pub struct SessionGuard {
    rx: Option<mpsc::Receiver<ServerMessage>>,
    pub cancel: CancellationToken,
    registry: Registry,
    agent_id: String,
    generation: u64,
}

impl SessionGuard {
    pub fn take_rx(&mut self) -> mpsc::Receiver<ServerMessage> {
        self.rx.take().expect("the receiver is taken once")
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let mut inner = self.registry.lock();
        if inner
            .sessions
            .get(&self.agent_id)
            .is_some_and(|e| e.generation == self.generation)
        {
            inner.sessions.remove(&self.agent_id);
        }
    }
}

impl Inner {
    fn note_live_replacement(
        &mut self,
        agent_id: &str,
        previous_ip: Option<IpAddr>,
        ip: Option<IpAddr>,
    ) {
        let now = Instant::now();
        let recent = self.replacements.entry(agent_id.to_string()).or_default();
        recent.retain(|at| now.duration_since(*at) < CONFLICT_WINDOW);
        recent.push_back(now);
        let other_network = matches!((previous_ip, ip), (Some(a), Some(b)) if !same_network(a, b));
        if other_network || recent.len() >= CONFLICT_REPLACEMENTS {
            self.security
                .entry(agent_id.to_string())
                .or_default()
                .conflict = Some(SessionConflict {
                at_ms: crate::db::now_ms(),
                ip,
                previous_ip,
            });
        }
    }
}

impl Registry {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn register(
        &self,
        agent_id: &str,
        repository_id: &str,
        peer: Option<IpAddr>,
    ) -> SessionGuard {
        let (tx, rx) = mpsc::channel(CHANNEL_DEPTH);
        let cancel = CancellationToken::new();
        self.live.set_ready(agent_id, false);
        let mut inner = self.lock();
        inner.next_generation += 1;
        let generation = inner.next_generation;
        let entry = Entry {
            generation,
            repository_id: repository_id.to_string(),
            last_seen: Instant::now(),
            tx,
            cancel: cancel.clone(),
            capabilities: Vec::new(),
            peer,
        };
        if let Some(old) = inner.sessions.insert(agent_id.to_string(), entry) {
            if old.last_seen.elapsed() < ONLINE_WINDOW {
                inner.note_live_replacement(agent_id, old.peer, peer);
            }
            let _ = old.tx.try_send(ServerMessage {
                body: Some(Body::Disconnect(Disconnect {
                    reason: DisconnectReason::Replaced as i32,
                })),
            });
            old.cancel.cancel();
        }
        SessionGuard {
            rx: Some(rx),
            cancel,
            registry: self.clone(),
            agent_id: agent_id.to_string(),
            generation,
        }
    }

    /// The caller records only agents that exist in the catalog, so the maps are bounded by the number of agents.
    /// Returns whether this refusal should be logged: at most once per agent every ten minutes.
    pub fn note_cert_rejected(&self, agent_id: &str, serial: &str, ip: Option<IpAddr>) -> bool {
        let mut inner = self.lock();
        inner
            .security
            .entry(agent_id.to_string())
            .or_default()
            .rejected = Some(CertRejection {
            at_ms: crate::db::now_ms(),
            serial: serial.chars().take(SERIAL_MAX_CHARS).collect(),
            ip,
        });
        let due = inner
            .reject_logged
            .get(agent_id)
            .is_none_or(|at| at.elapsed() >= REJECT_LOG_EVERY);
        if due {
            inner
                .reject_logged
                .insert(agent_id.to_string(), Instant::now());
        }
        due
    }

    /// A certificate of no known agent (the catalog was restored from an older backup, or the host was removed).
    /// There is no host to alert on, so this is a counter and a rate-limited log line, not a security event.
    pub fn note_unknown_agent(&self) -> bool {
        let mut inner = self.lock();
        inner.unknown_refusals += 1;
        let due = inner
            .unknown_logged
            .is_none_or(|at| at.elapsed() >= REJECT_LOG_EVERY);
        if due {
            inner.unknown_logged = Some(Instant::now());
        }
        due
    }

    pub fn unknown_agent_refusals(&self) -> u64 {
        self.lock().unknown_refusals
    }

    /// A copy of the recent security events per agent; older ones are dropped here.
    pub fn security_events(&self) -> Vec<(String, SecurityEvents)> {
        let oldest = crate::db::now_ms() - SECURITY_EVENT_TTL.as_millis() as i64;
        let mut inner = self.lock();
        inner.security.retain(|_, e| {
            e.conflict.take_if(|c| c.at_ms <= oldest);
            e.rejected.take_if(|r| r.at_ms <= oldest);
            e.conflict.is_some() || e.rejected.is_some()
        });
        inner
            .security
            .iter()
            .map(|(id, e)| (id.clone(), e.clone()))
            .collect()
    }

    pub fn touch(&self, agent_id: &str) {
        if let Some(e) = self.lock().sessions.get_mut(agent_id) {
            e.last_seen = Instant::now();
        }
    }

    pub fn set_capabilities(&self, agent_id: &str, caps: Vec<String>) {
        if let Some(e) = self.lock().sessions.get_mut(agent_id) {
            e.capabilities = caps;
        }
    }

    pub fn supports(&self, agent_id: &str, cap: &str) -> bool {
        self.lock().sessions.get(agent_id).is_some_and(|e| {
            e.last_seen.elapsed() < ONLINE_WINDOW && e.capabilities.iter().any(|c| c == cap)
        })
    }

    pub fn begin_plan(&self, job_id: &str) -> u64 {
        let mut inner = self.lock();
        inner.next_generation += 1;
        let generation = inner.next_generation;
        inner.issued.insert(
            job_id.to_string(),
            Issued {
                generation,
                packs: HashMap::new(),
            },
        );
        generation
    }

    pub fn issue_pack(
        &self,
        job_id: &str,
        generation: u64,
        number: u32,
        pack_id: BlobId,
        key: Key32,
    ) -> bool {
        let mut inner = self.lock();
        match inner.issued.get_mut(job_id) {
            Some(issued) if issued.generation == generation => {
                issued.packs.insert(number, (pack_id, key));
                true
            }
            _ => false,
        }
    }

    pub fn issued_pack(&self, job_id: &str, number: u32) -> Option<(BlobId, Key32)> {
        self.lock()
            .issued
            .get(job_id)
            .and_then(|issued| issued.packs.get(&number).cloned())
    }

    pub fn clear_issued(&self, job_id: &str) {
        self.lock().issued.remove(job_id);
    }

    pub fn is_online(&self, agent_id: &str) -> bool {
        self.lock()
            .sessions
            .get(agent_id)
            .is_some_and(|e| e.last_seen.elapsed() < ONLINE_WINDOW)
    }

    pub fn online_in_repository(&self, repository_id: &str) -> Vec<String> {
        self.lock()
            .sessions
            .iter()
            .filter(|(_, e)| {
                e.repository_id == repository_id && e.last_seen.elapsed() < ONLINE_WINDOW
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub fn revoke(&self, agent_id: &str) {
        let mut inner = self.lock();
        inner.revoked.insert(agent_id.to_string());
        if let Some(entry) = inner.sessions.get(agent_id) {
            let _ = entry.tx.try_send(ServerMessage {
                body: Some(Body::Disconnect(Disconnect {
                    reason: DisconnectReason::Revoked as i32,
                })),
            });
            entry.cancel.cancel();
        }
    }

    pub fn is_revoked(&self, agent_id: &str) -> bool {
        self.lock().revoked.contains(agent_id)
    }

    pub fn disconnect_all(&self, reason: DisconnectReason) {
        for entry in self.lock().sessions.values() {
            let _ = entry.tx.try_send(ServerMessage {
                body: Some(Body::Disconnect(Disconnect {
                    reason: reason as i32,
                })),
            });
            entry.cancel.cancel();
        }
    }

    pub fn send(&self, agent_id: &str, message: ServerMessage) -> bool {
        let tx = self.lock().sessions.get(agent_id).map(|e| e.tx.clone());
        tx.is_some_and(|tx| tx.try_send(message).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> Option<IpAddr> {
        Some(text.parse().unwrap())
    }

    fn conflict(registry: &Registry, agent: &str) -> Option<SessionConflict> {
        registry
            .security_events()
            .into_iter()
            .find(|(id, _)| id == agent)
            .and_then(|(_, e)| e.conflict)
    }

    #[test]
    fn registry_flags_live_session_from_other_network() {
        let registry = Registry::default();
        let _first = registry.register("a1", "r1", ip("10.0.0.7"));
        assert!(conflict(&registry, "a1").is_none());
        let _second = registry.register("a1", "r1", ip("10.0.0.8"));
        let found = conflict(&registry, "a1").expect("a live session was replaced from elsewhere");
        assert_eq!(found.ip, ip("10.0.0.8"));
        assert_eq!(found.previous_ip, ip("10.0.0.7"));
    }

    #[test]
    fn registry_ignores_restart_from_same_network() {
        let registry = Registry::default();
        let _first = registry.register("a1", "r1", ip("10.0.0.7"));
        let _second = registry.register("a1", "r1", ip("10.0.0.7"));
        let _third = registry.register("a1", "r1", ip("10.0.0.7"));
        assert!(conflict(&registry, "a1").is_none());
        let v6 = Registry::default();
        let _first = v6.register("a2", "r1", ip("2001:db8:0:ab::1"));
        let _second = v6.register("a2", "r1", ip("2001:db8:0:ab::2"));
        assert!(conflict(&v6, "a2").is_none());
    }

    #[test]
    fn registry_flags_repeated_replacements() {
        let registry = Registry::default();
        let mut guards = Vec::new();
        guards.push(registry.register("a1", "r1", ip("10.0.0.7")));
        for _ in 1..CONFLICT_REPLACEMENTS {
            guards.push(registry.register("a1", "r1", ip("10.0.0.7")));
            assert!(conflict(&registry, "a1").is_none());
        }
        guards.push(registry.register("a1", "r1", ip("10.0.0.7")));
        assert_eq!(
            conflict(&registry, "a1").map(|c| c.previous_ip),
            Some(ip("10.0.0.7"))
        );
    }

    #[test]
    fn same_network_compares_ipv4_exactly_and_ipv6_by_56_bits() {
        let same = |a: &str, b: &str| same_network(a.parse().unwrap(), b.parse().unwrap());
        assert!(same("10.0.0.7", "10.0.0.7"));
        assert!(!same("10.0.0.7", "10.0.0.8"));
        assert!(same("2001:db8:0:ab::1", "2001:db8:0:ff::2"));
        assert!(!same("2001:db8:0:ab::1", "2001:db8:0:1ab::1"));
        assert!(same("::ffff:10.0.0.7", "10.0.0.7"));
        assert!(!same("10.0.0.7", "2001:db8::1"));
    }

    #[test]
    fn cert_rejection_is_kept_with_a_truncated_serial() {
        let registry = Registry::default();
        registry.note_cert_rejected("a1", &"f".repeat(100), ip("10.0.0.7"));
        let events = registry.security_events();
        let rejected = events[0].1.rejected.clone().unwrap();
        assert_eq!(rejected.serial.len(), SERIAL_MAX_CHARS);
        assert_eq!(rejected.ip, ip("10.0.0.7"));
    }
}
