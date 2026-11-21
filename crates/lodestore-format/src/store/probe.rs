use bytes::Bytes;

use super::{BlobDelete, BlobError, BlobPurge, BlobStore, PurgeOutcome, SftpStore, Store};

/// The key prefixes of the repository layout an agent may write under. A probe
/// object under any of them must stay invisible to every reader.
pub const PROBE_PREFIXES: [&str; 3] = ["packs/probe-", "index/probe-", "snapshots/probe-"];
const PROBE_PREFIX: &str = PROBE_PREFIXES[0];
const MAX_PROBE_VERSIONS: usize = 8;
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

/// What a probe measured about the storage's protection of backups, never what
/// a capability flag promises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuaranteeLevel {
    CreateOnlyEnforced,
    OverwriteKeepsOldVersion,
    HostCanOverwriteOrDelete,
    Unchecked,
}

impl GuaranteeLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            GuaranteeLevel::CreateOnlyEnforced => "create_only_enforced",
            GuaranteeLevel::OverwriteKeepsOldVersion => "overwrite_keeps_old_version",
            GuaranteeLevel::HostCanOverwriteOrDelete => "host_can_overwrite_or_delete",
            GuaranteeLevel::Unchecked => "unchecked",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [
            GuaranteeLevel::CreateOnlyEnforced,
            GuaranteeLevel::OverwriteKeepsOldVersion,
            GuaranteeLevel::HostCanOverwriteOrDelete,
            GuaranteeLevel::Unchecked,
        ]
        .into_iter()
        .find(|l| l.as_str() == s)
    }

    /// Higher is stronger; `Unchecked` is not on the scale.
    pub fn rank(self) -> Option<u8> {
        match self {
            GuaranteeLevel::HostCanOverwriteOrDelete => Some(0),
            GuaranteeLevel::OverwriteKeepsOldVersion => Some(1),
            GuaranteeLevel::CreateOnlyEnforced => Some(2),
            GuaranteeLevel::Unchecked => None,
        }
    }
}

/// Why a level is not the strongest one. The codes are shared by the server,
/// the CLI and the web UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weakness {
    AgentCanDelete,
    AgentCanOverwrite,
    AgentCanDeleteVersions,
    NotEnforceable,
    NoAtomicPublish,
    HeldHandleRewrite,
    AgentCanWriteServerFiles,
    NoDurableFsync,
    SharedCredentials,
    LifecycleDeletes,
    LifecycleUnverified,
}

const WEAKNESSES: [Weakness; 11] = [
    Weakness::AgentCanDelete,
    Weakness::AgentCanOverwrite,
    Weakness::AgentCanDeleteVersions,
    Weakness::NotEnforceable,
    Weakness::NoAtomicPublish,
    Weakness::HeldHandleRewrite,
    Weakness::AgentCanWriteServerFiles,
    Weakness::NoDurableFsync,
    Weakness::SharedCredentials,
    Weakness::LifecycleDeletes,
    Weakness::LifecycleUnverified,
];

impl Weakness {
    pub fn as_str(self) -> &'static str {
        match self {
            Weakness::AgentCanDelete => "agent_can_delete",
            Weakness::AgentCanOverwrite => "agent_can_overwrite",
            Weakness::AgentCanDeleteVersions => "agent_can_delete_versions",
            Weakness::NotEnforceable => "not_enforceable",
            Weakness::NoAtomicPublish => "no_atomic_publish",
            Weakness::HeldHandleRewrite => "held_handle_rewrite",
            Weakness::AgentCanWriteServerFiles => "agent_can_write_server_files",
            Weakness::NoDurableFsync => "no_durable_fsync",
            Weakness::SharedCredentials => "shared_credentials",
            Weakness::LifecycleDeletes => "lifecycle_deletes",
            Weakness::LifecycleUnverified => "lifecycle_unverified",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        WEAKNESSES.into_iter().find(|w| w.as_str() == s)
    }

    pub fn message(self) -> &'static str {
        match self {
            Weakness::AgentCanDelete => "A host can delete backups.",
            Weakness::AgentCanOverwrite => {
                "A host can replace backups, and the storage keeps no earlier version."
            }
            Weakness::AgentCanDeleteVersions => {
                "A host can delete the earlier versions the storage keeps."
            }
            Weakness::NotEnforceable => {
                "Every host that writes to this folder can change or delete its backups."
            }
            Weakness::NoAtomicPublish => {
                "An interrupted upload stays in the storage as a broken file."
            }
            Weakness::HeldHandleRewrite => {
                "A host can keep a file open after publishing it and change it later."
            }
            Weakness::AgentCanWriteServerFiles => "A host can change files the server wrote.",
            Weakness::NoDurableFsync => {
                "The server does not confirm that uploads reached its disk."
            }
            Weakness::SharedCredentials => {
                "Hosts use the server's own credentials for this storage, so they can do anything the server can."
            }
            Weakness::LifecycleDeletes => {
                "A lifecycle rule on the bucket deletes objects or their earlier versions."
            }
            Weakness::LifecycleUnverified => {
                "The bucket's lifecycle rules could not be read; check that none deletes objects or earlier versions."
            }
        }
    }
}

/// The SFTP server presented a host key other than the pinned one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKeyChange {
    pub pinned: String,
    pub offered: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    pub existing_repository: bool,
    pub checks: [Check; 5],
    pub agent_key: AgentKeyState,
    pub guarantee: GuaranteeLevel,
    pub weaknesses: Vec<Weakness>,
    pub host_key_changed: Option<HostKeyChange>,
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
            guarantee: GuaranteeLevel::Unchecked,
            weaknesses: Vec::new(),
            host_key_changed: None,
        }
    }

    fn add_weakness(&mut self, w: Weakness) {
        if !self.weaknesses.contains(&w) {
            self.weaknesses.push(w);
        }
    }

    /// Lowers the level to `HostCanOverwriteOrDelete` and records why. An
    /// unchecked report stays as it is.
    pub fn cap_guarantee(&mut self, w: Weakness) {
        if self.guarantee == GuaranteeLevel::Unchecked {
            return;
        }
        self.add_weakness(w);
        self.guarantee = GuaranteeLevel::HostCanOverwriteOrDelete;
    }

    /// Records a reason without changing the level. An unchecked report stays
    /// as it is.
    pub fn note(&mut self, w: Weakness) {
        if self.guarantee != GuaranteeLevel::Unchecked {
            self.add_weakness(w);
        }
    }

    /// The level never exceeds what the store the agent writes through
    /// guarantees about publishing: a store that can leave a torn object under
    /// the final name is capped.
    fn apply_publish_mode(&mut self, publishes_atomically: bool) {
        if !publishes_atomically {
            self.cap_guarantee(Weakness::NoAtomicPublish);
        }
    }

    /// The one rule for a bucket whose lifecycle rules could not be read: a
    /// level that rests on earlier versions can no longer be trusted, the
    /// others only carry the note.
    pub fn lifecycle_unverified(&mut self) {
        match self.guarantee {
            GuaranteeLevel::OverwriteKeepsOldVersion => {
                self.cap_guarantee(Weakness::LifecycleUnverified);
            }
            _ => self.note(Weakness::LifecycleUnverified),
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
        let text = match e {
            BlobError::HostKeyMismatch { pinned, offered } => {
                self.host_key_changed = Some(HostKeyChange {
                    pinned: pinned.clone(),
                    offered: offered.clone(),
                });
                e.to_string()
            }
            _ => message(e).to_string(),
        };
        self.set(id, status, Some(format!("{prefix}{text}")));
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

fn probe_key(prefix: &str) -> String {
    let mut rnd = [0u8; 8];
    getrandom::fill(&mut rnd).expect("os randomness");
    format!("{prefix}{}", data_encoding::HEXLOWER.encode(&rnd))
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
    agent: [String; 3],
    held: String,
    foreign: String,
}

impl Default for ProbeKeys {
    fn default() -> Self {
        ProbeKeys {
            server: probe_key(PROBE_PREFIX),
            agent: PROBE_PREFIXES.map(probe_key),
            held: probe_key(PROBE_PREFIX),
            foreign: probe_key(PROBE_PREFIXES[1]),
        }
    }
}

pub fn is_probe_key(key: &str) -> bool {
    PROBE_PREFIXES.iter().any(|p| key.starts_with(p))
}

/// Removes the throwaway objects with the server credentials, every version
/// of them, and returns the ones it could not remove. A key that was never
/// written is not a failure.
pub async fn remove_probe_objects(server: &Store, keys: &ProbeKeys) -> Vec<(String, BlobError)> {
    let mut failed = Vec::new();
    let all = [&keys.server, &keys.held, &keys.foreign]
        .into_iter()
        .chain(&keys.agent);
    for k in all {
        match server.purge(k).await {
            PurgeOutcome::Purged { .. } | PurgeOutcome::NotFound => {}
            PurgeOutcome::Retained { .. } => failed.push((
                k.clone(),
                BlobError::backend("a probe object version is retained by the store"),
            )),
            PurgeOutcome::Failed(e) => failed.push((k.clone(), e)),
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
        AgentProbe::Store(agent) => {
            probe_agent(&mut report, server, agent, &keys.agent, body.clone()).await
        }
    }
    if let AgentProbe::Store(Store::Sftp(sftp)) = agent
        && wrote
    {
        probe_sftp(&mut report, server, sftp, keys, body).await;
    }
    if wrote && matches!(agent, AgentProbe::NotEnforceable) {
        report.guarantee = GuaranteeLevel::HostCanOverwriteOrDelete;
        report.add_weakness(Weakness::NotEnforceable);
    }
    let agent_publishes_atomically = match agent {
        AgentProbe::Store(agent) => agent.publishes_atomically(),
        AgentProbe::NotEnforceable => server.publishes_atomically(),
    };
    report.apply_publish_mode(agent_publishes_atomically);

    report
}

/// The steps only an SFTP agent needs. Whether the agent can rewrite a file it
/// published through a handle it kept open is not measured to decide the level:
/// the server cannot revoke that handle, so the level never exceeds
/// `HostCanOverwriteOrDelete`, and the outcome only goes into the check's
/// detail. A step that cannot be run leaves the level unchecked.
async fn probe_sftp(
    report: &mut ProbeReport,
    server: &Store,
    agent: &SftpStore,
    keys: &ProbeKeys,
    body: Bytes,
) {
    if report.guarantee == GuaranteeLevel::Unchecked {
        return;
    }
    let mut evidence = Vec::new();

    evidence.push(
        match agent.rewrite_held_handle_for_probe(&keys.held).await {
            Ok(true) => "held handle: a published file was changed through it".to_owned(),
            Ok(false) => "held handle: a published file was not changed through it".to_owned(),
            Err(e) => format!("held handle: not exercised ({e})"),
        },
    );
    report.cap_guarantee(Weakness::HeldHandleRewrite);

    let server_file = match server.put_if_absent(&keys.foreign, body).await {
        Ok(()) => agent.open_for_write_for_probe(&keys.foreign).await,
        Err(e) => Err(e),
    };
    match server_file {
        Ok(true) => {
            evidence.push("server files: the agent opened a file of the server for writing".into());
            report.cap_guarantee(Weakness::AgentCanWriteServerFiles);
        }
        Ok(false) => evidence.push("server files: the agent may not write them".into()),
        Err(e) => {
            evidence.push(format!("server files: not exercised ({e})"));
            report.guarantee = GuaranteeLevel::Unchecked;
        }
    }

    match agent.durable_fsync().await {
        Ok(true) => {}
        Ok(false) => {
            evidence.push("fsync: the server does not offer fsync@openssh.com".into());
            report.cap_guarantee(Weakness::NoDurableFsync);
        }
        Err(e) => {
            evidence.push(format!("fsync: not exercised ({e})"));
            report.guarantee = GuaranteeLevel::Unchecked;
        }
    }

    let i = CHECK_ORDER
        .iter()
        .position(|c| *c == CheckId::AgentDelete)
        .expect("every id is in CHECK_ORDER");
    report.checks[i].detail = Some(evidence.join("; "));
}

/// What the probe learned from one agent object.
enum KeyVerdict {
    Unwritable(BlobError),
    Inconclusive(BlobError),
    Verified,
    CanDelete,
    CanOverwrite {
        level: GuaranteeLevel,
        weakness: Option<Weakness>,
    },
}

/// Writes, tries to overwrite and tries to delete a throwaway object with the
/// agent's credentials. Only a refusal reported as access denied counts as
/// "cannot": any other error leaves the verdict inconclusive. The level comes
/// from these measurements alone, never from what the backend claims to support.
/// It is taken on every layout prefix and the worst outcome wins: a delete on
/// any prefix is a delete, an inconclusive answer on any prefix leaves the
/// level unchecked, and a level that rests on kept versions is the lowest one.
async fn probe_agent(
    report: &mut ProbeReport,
    server: &Store,
    agent: &Store,
    keys: &[String],
    body: Bytes,
) {
    let mut can_delete = false;
    let mut inconclusive = None;
    let mut overwrote: Option<(GuaranteeLevel, Option<Weakness>)> = None;
    for key in keys {
        match probe_agent_key(server, agent, key, body.clone()).await {
            KeyVerdict::Unwritable(e) => {
                report.set_blob_error(
                    CheckId::AgentDelete,
                    CheckStatus::Fail,
                    "agent key cannot write: ",
                    &e,
                );
                return;
            }
            KeyVerdict::Inconclusive(e) => {
                inconclusive.get_or_insert(e);
            }
            KeyVerdict::Verified => {}
            KeyVerdict::CanDelete => can_delete = true,
            KeyVerdict::CanOverwrite { level, weakness } => {
                overwrote = Some(weaker(overwrote, (level, weakness)));
            }
        }
    }
    if can_delete {
        report.agent_key = AgentKeyState::CanDelete;
        report.guarantee = GuaranteeLevel::HostCanOverwriteOrDelete;
        report.add_weakness(Weakness::AgentCanDelete);
        report.set(
            CheckId::AgentDelete,
            CheckStatus::Warn,
            Some("the agent key can delete objects".into()),
        );
    } else if let Some(e) = inconclusive {
        report.set_blob_error(
            CheckId::AgentDelete,
            CheckStatus::Neutral,
            "inconclusive: ",
            &e,
        );
    } else if let Some((level, weakness)) = overwrote {
        report.agent_key = AgentKeyState::CanOverwrite;
        report.guarantee = level;
        if let Some(w) = weakness {
            report.add_weakness(w);
        }
        report.set(
            CheckId::AgentDelete,
            CheckStatus::Warn,
            Some(overwrite_note(level).into()),
        );
    } else {
        report.agent_key = AgentKeyState::Verified;
        report.guarantee = GuaranteeLevel::CreateOnlyEnforced;
        report.set(CheckId::AgentDelete, CheckStatus::Pass, None);
    }
}

async fn probe_agent_key(server: &Store, agent: &Store, key: &str, body: Bytes) -> KeyVerdict {
    if let Err(e) = agent.put_if_absent(key, body).await {
        return KeyVerdict::Unwritable(e);
    }
    let overwrite = Bytes::from_static(OVERWRITE_BODY);
    let can_overwrite = match agent.overwrite_for_probe(key, overwrite).await {
        Ok(()) => true,
        Err(BlobError::Denied { .. } | BlobError::AlreadyExists) => false,
        Err(e) => return KeyVerdict::Inconclusive(e),
    };
    match agent.delete(key).await {
        Ok(()) => KeyVerdict::CanDelete,
        Err(BlobError::Denied { .. }) if can_overwrite => overwritten(server, agent, key).await,
        Err(BlobError::Denied { .. }) => KeyVerdict::Verified,
        Err(e) => KeyVerdict::Inconclusive(e),
    }
}

type Overwrite = (GuaranteeLevel, Option<Weakness>);

/// The outcome to keep across keys: the weakest level wins and a tie keeps the
/// first, so the order of the layout prefixes never lifts the level.
fn weaker(have: Option<Overwrite>, new: Overwrite) -> Overwrite {
    match have {
        Some(have) if new.0.rank() >= have.0.rank() => have,
        _ => new,
    }
}

/// Says whether the overwritten data survives, so the operator sees why the
/// level is not the lowest one.
fn overwrite_note(level: GuaranteeLevel) -> &'static str {
    if level == GuaranteeLevel::OverwriteKeepsOldVersion {
        "the agent key cannot delete but can overwrite existing objects; the storage keeps the earlier version"
    } else {
        "the agent key cannot delete but can overwrite existing objects"
    }
}

/// A single version means the overwrite replaced the original in place.
fn keeps_two_versions(versions: &[(String, u64)]) -> bool {
    versions.len() >= 2
}

/// Only a version holding the original probe bytes proves the data was kept.
fn is_probe_body(bytes: &[u8]) -> bool {
    bytes == PROBE_BODY
}

/// The agent replaced the object and could not delete it. The storage keeps
/// the original only if the server sees an earlier version holding the
/// original bytes and the agent cannot delete that version.
async fn overwritten(server: &Store, agent: &Store, key: &str) -> KeyVerdict {
    let replaced = KeyVerdict::CanOverwrite {
        level: GuaranteeLevel::HostCanOverwriteOrDelete,
        weakness: Some(Weakness::AgentCanOverwrite),
    };
    let versions = match server.versions_for_probe(key).await {
        Ok(Some(v)) if keeps_two_versions(&v) => v,
        Ok(_) => return replaced,
        Err(e) => return KeyVerdict::Inconclusive(e),
    };
    let mut original = None;
    for (id, size) in versions.iter().take(MAX_PROBE_VERSIONS) {
        match server.read_version_for_probe(key, id, *size).await {
            Ok(bytes) if is_probe_body(&bytes) => {
                original = Some(id);
                break;
            }
            Ok(_) => {}
            Err(e) => return KeyVerdict::Inconclusive(e),
        }
    }
    let Some(original) = original else {
        return replaced;
    };
    match agent.delete_version_for_probe(key, original).await {
        Err(BlobError::Denied { .. }) => KeyVerdict::CanOverwrite {
            level: GuaranteeLevel::OverwriteKeepsOldVersion,
            weakness: None,
        },
        Ok(()) => KeyVerdict::CanOverwrite {
            level: GuaranteeLevel::HostCanOverwriteOrDelete,
            weakness: Some(Weakness::AgentCanDeleteVersions),
        },
        Err(e) => KeyVerdict::Inconclusive(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weakest_overwrite_wins_in_any_order() {
        let kept = (GuaranteeLevel::OverwriteKeepsOldVersion, None);
        let replaced = (
            GuaranteeLevel::HostCanOverwriteOrDelete,
            Some(Weakness::AgentCanOverwrite),
        );
        let versions_gone = (
            GuaranteeLevel::HostCanOverwriteOrDelete,
            Some(Weakness::AgentCanDeleteVersions),
        );
        assert_eq!(weaker(None, kept), kept);
        assert_eq!(weaker(Some(kept), replaced), replaced);
        assert_eq!(weaker(Some(replaced), kept), replaced);
        assert_eq!(weaker(Some(replaced), versions_gone), replaced);
        assert_eq!(weaker(Some(versions_gone), replaced), versions_gone);
    }

    #[test]
    fn overwrite_note_names_the_kept_version() {
        assert_eq!(
            overwrite_note(GuaranteeLevel::OverwriteKeepsOldVersion),
            "the agent key cannot delete but can overwrite existing objects; the storage keeps the earlier version"
        );
        assert_eq!(
            overwrite_note(GuaranteeLevel::HostCanOverwriteOrDelete),
            "the agent key cannot delete but can overwrite existing objects"
        );
    }

    #[test]
    fn original_needs_two_versions_and_the_probe_body() {
        let v = |id: &str| (id.to_owned(), 1);
        assert!(!keeps_two_versions(&[]));
        assert!(!keeps_two_versions(&[v("a")]));
        assert!(keeps_two_versions(&[v("a"), v("b")]));
        assert!(is_probe_body(PROBE_BODY));
        assert!(!is_probe_body(OVERWRITE_BODY));
        assert!(!is_probe_body(b""));
    }

    #[test]
    fn only_a_typed_host_key_change_reaches_the_error() {
        let mut r = ProbeReport::new(false);
        let changed = BlobError::HostKeyMismatch {
            pinned: "SHA256:pinned".into(),
            offered: "SHA256:offered".into(),
        };
        r.set_blob_error(CheckId::Write, CheckStatus::Fail, "", &changed);
        assert_eq!(r.check(CheckId::Write).error, Some(changed.to_string()));
        assert_eq!(
            r.host_key_changed,
            Some(HostKeyChange {
                pinned: "SHA256:pinned".into(),
                offered: "SHA256:offered".into(),
            })
        );
        r.host_key_changed = None;

        let hostile = BlobError::backend("the SFTP server's host key changed: pinned <a href=x>");
        r.set_blob_error(CheckId::List, CheckStatus::Fail, "", &hostile);
        let error = r.check(CheckId::List).error.clone().unwrap_or_default();
        assert!(!error.contains("host key"), "{error}");
        assert_eq!(r.host_key_changed, None);
    }

    fn report_at(level: GuaranteeLevel) -> ProbeReport {
        let mut r = ProbeReport::new(false);
        r.guarantee = level;
        r
    }

    #[test]
    fn guarantee_and_weakness_names_round_trip() {
        for l in [
            GuaranteeLevel::CreateOnlyEnforced,
            GuaranteeLevel::OverwriteKeepsOldVersion,
            GuaranteeLevel::HostCanOverwriteOrDelete,
            GuaranteeLevel::Unchecked,
        ] {
            assert_eq!(GuaranteeLevel::parse(l.as_str()), Some(l));
        }
        for w in WEAKNESSES {
            assert_eq!(Weakness::parse(w.as_str()), Some(w));
            assert!(w.message().ends_with('.'));
        }
        assert_eq!(GuaranteeLevel::parse("nope"), None);
        assert_eq!(Weakness::parse("nope"), None);
    }

    #[test]
    fn guarantee_rank_orders_the_levels() {
        let rank = |l: GuaranteeLevel| l.rank();
        assert!(
            rank(GuaranteeLevel::HostCanOverwriteOrDelete)
                < rank(GuaranteeLevel::OverwriteKeepsOldVersion)
        );
        assert!(
            rank(GuaranteeLevel::OverwriteKeepsOldVersion)
                < rank(GuaranteeLevel::CreateOnlyEnforced)
        );
        assert_eq!(rank(GuaranteeLevel::Unchecked), None);
    }

    #[test]
    fn weakness_cap_lowers_and_note_keeps_the_level() {
        let mut r = report_at(GuaranteeLevel::CreateOnlyEnforced);
        r.note(Weakness::NoDurableFsync);
        assert_eq!(r.guarantee, GuaranteeLevel::CreateOnlyEnforced);
        r.cap_guarantee(Weakness::NoAtomicPublish);
        r.cap_guarantee(Weakness::NoAtomicPublish);
        assert_eq!(r.guarantee, GuaranteeLevel::HostCanOverwriteOrDelete);
        assert_eq!(
            r.weaknesses,
            [Weakness::NoDurableFsync, Weakness::NoAtomicPublish]
        );

        let mut u = report_at(GuaranteeLevel::Unchecked);
        u.cap_guarantee(Weakness::AgentCanDelete);
        u.note(Weakness::NoAtomicPublish);
        assert_eq!(u, report_at(GuaranteeLevel::Unchecked));
    }

    #[test]
    fn lifecycle_unverified_follows_the_single_rule() {
        let mut r = report_at(GuaranteeLevel::OverwriteKeepsOldVersion);
        r.lifecycle_unverified();
        assert_eq!(r.guarantee, GuaranteeLevel::HostCanOverwriteOrDelete);
        assert_eq!(r.weaknesses, [Weakness::LifecycleUnverified]);

        let mut r = report_at(GuaranteeLevel::CreateOnlyEnforced);
        r.lifecycle_unverified();
        assert_eq!(r.guarantee, GuaranteeLevel::CreateOnlyEnforced);
        assert_eq!(r.weaknesses, [Weakness::LifecycleUnverified]);

        let mut r = report_at(GuaranteeLevel::HostCanOverwriteOrDelete);
        r.lifecycle_unverified();
        assert_eq!(r.guarantee, GuaranteeLevel::HostCanOverwriteOrDelete);
        assert_eq!(r.weaknesses, [Weakness::LifecycleUnverified]);

        let mut r = report_at(GuaranteeLevel::Unchecked);
        r.lifecycle_unverified();
        assert_eq!(r, report_at(GuaranteeLevel::Unchecked));
    }

    #[test]
    fn non_atomic_publish_caps_the_level() {
        let mut r = report_at(GuaranteeLevel::CreateOnlyEnforced);
        r.apply_publish_mode(true);
        assert_eq!(r, report_at(GuaranteeLevel::CreateOnlyEnforced));
        r.apply_publish_mode(false);
        assert_eq!(r.guarantee, GuaranteeLevel::HostCanOverwriteOrDelete);
        assert_eq!(r.weaknesses, [Weakness::NoAtomicPublish]);

        let mut u = report_at(GuaranteeLevel::Unchecked);
        u.apply_publish_mode(false);
        assert_eq!(u, report_at(GuaranteeLevel::Unchecked));
    }

    #[test]
    fn probe_keys_are_probe_names() {
        let keys = ProbeKeys::default();
        assert!(is_probe_key(&keys.server));
        assert!(keys.agent.iter().all(|k| is_probe_key(k)));
        for (key, prefix) in keys.agent.iter().zip(PROBE_PREFIXES) {
            assert!(key.starts_with(prefix));
        }
        assert!(!is_probe_key("packs/ab/cd"));
        assert!(!is_probe_key("config"));
    }

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
