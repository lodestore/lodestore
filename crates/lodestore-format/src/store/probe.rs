use bytes::Bytes;

use super::{BlobDelete, BlobError, BlobStore, Store};

const PROBE_PREFIX: &str = "packs/probe-";
const PROBE_BODY: &[u8] = b"lodestore capability probe";
const OVERWRITE_BODY: &[u8] = b"lodestore capability probe, overwritten";
pub const SKIPPED_NOTE: &str = "skipped because the write check failed";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckId {
    Write,
    List,
    ConditionalWrite,
    AgentDelete,
    ObjectLock,
}

impl CheckId {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckId::Write => "write",
            CheckId::List => "list",
            CheckId::ConditionalWrite => "conditional_write",
            CheckId::AgentDelete => "agent_delete",
            CheckId::ObjectLock => "object_lock",
        }
    }
}

pub const CHECK_ORDER: [CheckId; 5] = [
    CheckId::Write,
    CheckId::List,
    CheckId::ConditionalWrite,
    CheckId::AgentDelete,
    CheckId::ObjectLock,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
    Neutral,
}

/// `error` is a short human-readable message for a failed or doubtful check,
/// or a note for a neutral one. `detail` is the backend's raw error text, for
/// logs only. Neither contains credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub id: CheckId,
    pub status: CheckStatus,
    pub error: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKeyState {
    Verified,
    CanDelete,
    CanOverwrite,
    Unchecked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    pub existing_repository: bool,
    pub checks: [Check; 5],
    pub agent_key: AgentKeyState,
}

impl ProbeReport {
    fn new(existing_repository: bool) -> Self {
        ProbeReport {
            existing_repository,
            checks: CHECK_ORDER.map(|id| Check {
                id,
                status: CheckStatus::Neutral,
                error: None,
                detail: None,
            }),
            agent_key: AgentKeyState::Unchecked,
        }
    }

    pub fn check(&self, id: CheckId) -> &Check {
        &self.checks[CHECK_ORDER
            .iter()
            .position(|c| *c == id)
            .expect("every id is in CHECK_ORDER")]
    }

    fn set(&mut self, id: CheckId, status: CheckStatus, error: Option<String>) {
        let i = CHECK_ORDER
            .iter()
            .position(|c| *c == id)
            .expect("every id is in CHECK_ORDER");
        self.checks[i] = Check {
            id,
            status,
            error,
            detail: None,
        };
    }

    fn set_blob_error(&mut self, id: CheckId, status: CheckStatus, prefix: &str, e: &BlobError) {
        self.set(id, status, Some(format!("{prefix}{}", message(e))));
        let i = CHECK_ORDER
            .iter()
            .position(|c| *c == id)
            .expect("every id is in CHECK_ORDER");
        self.checks[i].detail = Some(e.to_string());
    }

    /// Whether the wizard must stop: the repository cannot be used as is.
    pub fn blocking(&self) -> bool {
        [CheckId::Write, CheckId::List, CheckId::AgentDelete]
            .iter()
            .any(|id| self.check(*id).status == CheckStatus::Fail)
    }

    pub fn agent_key_state(&self) -> AgentKeyState {
        self.agent_key
    }

    pub fn set_object_lock(&mut self, status: CheckStatus, error: Option<String>) {
        self.set(CheckId::ObjectLock, status, error);
    }
}

pub enum AgentProbe<'a> {
    NotEnforceable,
    Store(&'a Store),
}

fn probe_key() -> String {
    let mut rnd = [0u8; 8];
    getrandom::fill(&mut rnd).expect("os randomness");
    format!("{PROBE_PREFIX}{}", data_encoding::HEXLOWER.encode(&rnd))
}

fn message(e: &BlobError) -> &'static str {
    match e {
        BlobError::Denied { .. } => "access denied",
        BlobError::NotFound => "the bucket or path was not found",
        BlobError::Backend {
            temporary: true, ..
        } => "the storage is temporarily unavailable",
        _ => "the storage returned an unexpected error, see the server log",
    }
}

/// The names of the throwaway objects one probe run may leave behind.
pub struct ProbeKeys {
    server: String,
    agent: String,
}

impl Default for ProbeKeys {
    fn default() -> Self {
        ProbeKeys {
            server: probe_key(),
            agent: probe_key(),
        }
    }
}

/// Removes the throwaway objects with the server credentials and returns the
/// ones it could not remove. A key that was never written is not a failure.
pub async fn remove_probe_objects(server: &Store, keys: &ProbeKeys) -> Vec<(String, BlobError)> {
    let mut failed = Vec::new();
    for k in [&keys.server, &keys.agent] {
        match server.delete(k).await {
            Ok(()) | Err(BlobError::NotFound) => {}
            Err(e) => failed.push((k.clone(), e)),
        }
    }
    failed
}

/// Exercises the server credentials and, when there is one, the agent's
/// credentials against the same store, using throwaway objects under
/// `packs/probe-*` that the index and `rebuild_index` ignore, and removes them.
pub async fn probe(server: &Store, agent: AgentProbe<'_>) -> ProbeReport {
    let keys = ProbeKeys::default();
    let report = probe_keep(server, agent, &keys).await;
    let _ = remove_probe_objects(server, &keys).await;
    report
}

/// Like [`probe`] but leaves the objects in place, so a caller that bounds the
/// run in time can still clean up with [`remove_probe_objects`] afterwards.
pub async fn probe_keep(server: &Store, agent: AgentProbe<'_>, keys: &ProbeKeys) -> ProbeReport {
    let mut report = ProbeReport::new(server.get_range("config", 0..1).await.is_ok());
    let key = keys.server.clone();
    let agent_key = keys.agent.clone();
    let body = Bytes::from_static(PROBE_BODY);

    let wrote = match server.put_if_absent(&key, body.clone()).await {
        Ok(()) => {
            report.set(CheckId::Write, CheckStatus::Pass, None);
            true
        }
        Err(e) => {
            report.set_blob_error(CheckId::Write, CheckStatus::Fail, "", &e);
            false
        }
    };

    if wrote {
        match server.list(PROBE_PREFIX).await {
            Ok(entries) if entries.iter().any(|e| e.key == key) => {
                report.set(CheckId::List, CheckStatus::Pass, None);
            }
            Ok(_) => report.set(
                CheckId::List,
                CheckStatus::Fail,
                Some("the probe object is missing from the listing".into()),
            ),
            Err(e) => report.set_blob_error(CheckId::List, CheckStatus::Fail, "", &e),
        }
        if !server.conditional_write_supported() {
            report.set(
                CheckId::ConditionalWrite,
                CheckStatus::Warn,
                Some("unsupported".into()),
            );
        } else {
            match server.put_if_absent(&key, body.clone()).await {
                Err(BlobError::AlreadyExists) => {
                    report.set(CheckId::ConditionalWrite, CheckStatus::Pass, None);
                }
                Ok(()) => report.set(
                    CheckId::ConditionalWrite,
                    CheckStatus::Warn,
                    Some("a second write of the same key was not rejected".into()),
                ),
                Err(e) => {
                    report.set_blob_error(CheckId::ConditionalWrite, CheckStatus::Warn, "", &e)
                }
            }
        }
    } else {
        for id in [
            CheckId::List,
            CheckId::ConditionalWrite,
            CheckId::ObjectLock,
        ] {
            report.set(id, CheckStatus::Neutral, Some(SKIPPED_NOTE.into()));
        }
    }

    match agent {
        AgentProbe::NotEnforceable => report.set(
            CheckId::AgentDelete,
            CheckStatus::Neutral,
            Some("Not enforceable on a local folder".into()),
        ),
        AgentProbe::Store(_) if !wrote => report.set(
            CheckId::AgentDelete,
            CheckStatus::Neutral,
            Some(SKIPPED_NOTE.into()),
        ),
        AgentProbe::Store(agent) => probe_agent(&mut report, agent, &agent_key, body).await,
    }

    report
}

/// Writes, tries to overwrite and tries to delete a throwaway object with the
/// agent's credentials. Only a refusal reported as access denied counts as
/// "cannot": any other error leaves the verdict inconclusive.
async fn probe_agent(report: &mut ProbeReport, agent: &Store, key: &str, body: Bytes) {
    if let Err(e) = agent.put_if_absent(key, body).await {
        report.set_blob_error(
            CheckId::AgentDelete,
            CheckStatus::Fail,
            "agent key cannot write: ",
            &e,
        );
        return;
    }
    let overwrite = Bytes::from_static(OVERWRITE_BODY);
    let can_overwrite = match agent.overwrite_for_probe(key, overwrite).await {
        Ok(()) => true,
        Err(BlobError::Denied { .. } | BlobError::AlreadyExists) => false,
        Err(e) => {
            report.set_blob_error(
                CheckId::AgentDelete,
                CheckStatus::Neutral,
                "inconclusive: ",
                &e,
            );
            return;
        }
    };
    match agent.delete(key).await {
        Ok(()) => {
            report.agent_key = AgentKeyState::CanDelete;
            report.set(
                CheckId::AgentDelete,
                CheckStatus::Warn,
                Some("the agent key can delete objects".into()),
            );
        }
        Err(BlobError::Denied { .. }) if can_overwrite => {
            report.agent_key = AgentKeyState::CanOverwrite;
            report.set(
                CheckId::AgentDelete,
                CheckStatus::Warn,
                Some("the agent key cannot delete but can overwrite existing objects".into()),
            );
        }
        Err(BlobError::Denied { .. }) => {
            report.agent_key = AgentKeyState::Verified;
            report.set(CheckId::AgentDelete, CheckStatus::Pass, None);
        }
        Err(e) => report.set_blob_error(
            CheckId::AgentDelete,
            CheckStatus::Neutral,
            "inconclusive: ",
            &e,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_short_and_free_of_backend_text() {
        let raw = "PermissionDenied (persistent) at write, context: {service: s3}".to_owned();
        let cases = [
            (
                BlobError::Denied {
                    message: raw.clone(),
                },
                "access denied",
            ),
            (BlobError::NotFound, "the bucket or path was not found"),
            (
                BlobError::Backend {
                    message: raw.clone(),
                    temporary: true,
                },
                "the storage is temporarily unavailable",
            ),
            (
                BlobError::Backend {
                    message: raw,
                    temporary: false,
                },
                "the storage returned an unexpected error, see the server log",
            ),
            (
                BlobError::AlreadyExists,
                "the storage returned an unexpected error, see the server log",
            ),
        ];
        for (e, want) in cases {
            assert_eq!(message(&e), want);
            assert!(!message(&e).contains("context"));
        }
    }
}
