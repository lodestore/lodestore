use std::collections::{HashMap, HashSet};
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
const CHANNEL_DEPTH: usize = 16;

struct Entry {
    generation: u64,
    repository_id: String,
    last_seen: Instant,
    tx: mpsc::Sender<ServerMessage>,
    cancel: CancellationToken,
    capabilities: Vec<String>,
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

impl Registry {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn register(&self, agent_id: &str, repository_id: &str) -> SessionGuard {
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
        };
        if let Some(old) = inner.sessions.insert(agent_id.to_string(), entry) {
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
