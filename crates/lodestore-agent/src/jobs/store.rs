use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use data_encoding::BASE64;
use lodestore_proto::agent::v1::{JobCreated, JobEvent, JobFinished, JobSpec};
use prost::Message;
use serde::{Deserialize, Serialize};

use crate::AgentError;
use crate::identity::{ensure_dir, write_json_file, write_private_file};

pub const ACCEPTED: &str = "accepted";
pub const RUNNING: &str = "running";
pub const FINISHED: &str = "finished";

/// `<id>.json`. The finished and created messages are stored as base64 of
/// their protobuf encoding, so the file never drifts from the wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    pub job_id: String,
    pub kind: String,
    pub state: String,
    #[serde(default)]
    pub finished: Option<String>,
    #[serde(default)]
    pub created: Option<String>,
    #[serde(default)]
    pub last_event_seq: u64,
}

impl JobRecord {
    pub fn finished(&self) -> Option<JobFinished> {
        decode(self.finished.as_deref()?)
    }

    pub fn created(&self) -> Option<JobCreated> {
        decode(self.created.as_deref()?)
    }

    pub fn set_created(&mut self, created: &JobCreated) {
        self.created = Some(BASE64.encode(&created.encode_to_vec()));
    }

    pub fn set_finished(&mut self, finished: &JobFinished) {
        self.finished = Some(BASE64.encode(&finished.encode_to_vec()));
    }
}

fn decode<M: Message + Default>(text: &str) -> Option<M> {
    M::decode(BASE64.decode(text.as_bytes()).ok()?.as_slice()).ok()
}

#[derive(Serialize, Deserialize)]
struct EventLine {
    seq: u64,
    at_ms: i64,
    level: i32,
    kind: String,
    path: String,
    script: String,
    message: String,
}

impl From<&JobEvent> for EventLine {
    fn from(e: &JobEvent) -> Self {
        EventLine {
            seq: e.seq,
            at_ms: e.at_ms,
            level: e.level,
            kind: e.kind.clone(),
            path: e.path.clone(),
            script: e.script.clone(),
            message: e.message.clone(),
        }
    }
}

impl From<EventLine> for JobEvent {
    fn from(e: EventLine) -> Self {
        JobEvent {
            seq: e.seq,
            at_ms: e.at_ms,
            level: e.level,
            kind: e.kind,
            path: e.path,
            script: e.script,
            message: e.message,
        }
    }
}

pub struct JobStore {
    dir: PathBuf,
}

impl JobStore {
    pub fn open(dir: PathBuf) -> Result<Self, AgentError> {
        ensure_dir(&dir)?;
        Ok(JobStore { dir })
    }

    fn file(&self, id: &str, ext: &str) -> PathBuf {
        self.dir.join(format!("{id}.{ext}"))
    }

    pub fn save(&self, record: &JobRecord) -> Result<(), AgentError> {
        write_json_file(&self.file(&record.job_id, "json"), record)
    }

    pub fn load_all(&self) -> Vec<JobRecord> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut records: Vec<JobRecord> = entries
            .filter_map(|e| {
                let path = e.ok()?.path();
                if path.extension()? != "json" {
                    return None;
                }
                let loaded = std::fs::read(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()));
                match loaded {
                    Ok(record) => Some(record),
                    Err(e) => {
                        tracing::warn!("ignoring job file {}: {e}", path.display());
                        None
                    }
                }
            })
            .collect();
        records.sort_by(|a, b| a.job_id.cmp(&b.job_id));
        records
    }

    pub fn write_spec(&self, id: &str, spec: &JobSpec) -> Result<(), AgentError> {
        Ok(write_private_file(
            &self.file(id, "spec"),
            &spec.encode_to_vec(),
        )?)
    }

    pub fn append_event(&self, id: &str, event: &JobEvent) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(&EventLine::from(event))?;
        line.push(b'\n');
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(self.file(id, "events"))?
            .write_all(&line)
    }

    pub fn read_events(&self, id: &str) -> Vec<JobEvent> {
        let Ok(text) = std::fs::read_to_string(self.file(id, "events")) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|l| serde_json::from_str::<EventLine>(l).ok())
            .map(JobEvent::from)
            .collect()
    }

    pub fn remove(&self, id: &str) {
        for ext in ["json", "spec", "events"] {
            let _ = std::fs::remove_file(self.file(id, ext));
        }
    }
}
