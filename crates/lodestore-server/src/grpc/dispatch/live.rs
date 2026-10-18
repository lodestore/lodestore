use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LiveProgress {
    pub files_done: u64,
    pub files_total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub bytes_uploaded: u64,
    pub bytes_downloaded: u64,
    pub scan_done: bool,
    pub speed_bps: u64,
    pub upload_speed_bps: u64,
    pub eta_s: Option<u64>,
    pub updated_at: i64,
}

struct Entry {
    progress: LiveProgress,
    dirty: bool,
    flushed_at: Option<Instant>,
}

#[derive(Default)]
struct Inner {
    progress: HashMap<String, Entry>,
    cancels: HashSet<String>,
    ready: HashSet<String>,
}

#[derive(Clone, Default)]
pub struct Live(Arc<Mutex<Inner>>);

impl Live {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn get(&self, job_id: &str) -> Option<LiveProgress> {
        self.lock().progress.get(job_id).map(|e| e.progress.clone())
    }

    pub fn set(&self, job_id: &str, progress: LiveProgress) {
        let mut inner = self.lock();
        match inner.progress.get_mut(job_id) {
            Some(entry) => {
                entry.progress = progress;
                entry.dirty = true;
            }
            None => {
                inner.progress.insert(
                    job_id.to_string(),
                    Entry {
                        progress,
                        dirty: true,
                        flushed_at: None,
                    },
                );
            }
        }
    }

    pub fn remove(&self, job_id: &str) {
        let mut inner = self.lock();
        inner.progress.remove(job_id);
        inner.cancels.remove(job_id);
    }

    pub fn take_due(&self, interval: Duration) -> Vec<(String, LiveProgress)> {
        let mut inner = self.lock();
        inner
            .progress
            .iter_mut()
            .filter(|(_, e)| e.dirty && e.flushed_at.is_none_or(|t| t.elapsed() >= interval))
            .map(|(id, e)| {
                e.dirty = false;
                e.flushed_at = Some(Instant::now());
                (id.clone(), e.progress.clone())
            })
            .collect()
    }

    pub fn cancel_is_new(&self, job_id: &str) -> bool {
        self.lock().cancels.insert(job_id.to_string())
    }

    pub fn cancel_unsent(&self, job_id: &str) {
        self.lock().cancels.remove(job_id);
    }

    pub fn set_ready(&self, agent_id: &str, ready: bool) {
        let mut inner = self.lock();
        if ready {
            inner.ready.insert(agent_id.to_string());
        } else {
            inner.ready.remove(agent_id);
        }
    }

    pub fn is_ready(&self, agent_id: &str) -> bool {
        self.lock().ready.contains(agent_id)
    }
}
