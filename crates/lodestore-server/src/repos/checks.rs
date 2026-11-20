use std::collections::BTreeMap;
use std::fmt::Display;
use std::time::Duration;

use lodestore_format::store::location::secret_option_names;
use lodestore_format::store::probe::{
    AgentKeyState, AgentProbe, CHECK_ORDER, Check, CheckId, CheckStatus, GuaranteeLevel, ProbeKeys,
    ProbeReport, SKIPPED_NOTE, Weakness, probe_keep, remove_probe_objects,
};
use lodestore_format::store::{RepoLocation, StaticCredentials, Store};

use crate::error::ApiError;
use crate::repos::db::AccessKey;
use crate::repos::object_lock::{self, LifecycleVerdict};
use crate::repos::{CheckDto, CheckResult, LocationDto, RepositoryDraft};

const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn invalid(e: impl Display) -> ApiError {
    ApiError::Validation(e.to_string())
}

pub fn format_location(dto: &LocationDto) -> RepoLocation {
    RepoLocation {
        backend: dto.backend.clone(),
        options: dto.options.clone(),
    }
}

pub fn open_store(
    location: &RepoLocation,
    creds: Option<&AccessKey>,
) -> lodestore_format::Result<Store> {
    location.open(creds.map(|k| StaticCredentials {
        access_key_id: k.access_key_id.clone(),
        secret_access_key: k.secret_access_key.clone(),
    }))
}

/// Splits off the options that carry secrets: they are sealed on their own and
/// never appear in the stored location or in any response.
pub fn split_secret_options(location: &RepoLocation) -> (RepoLocation, BTreeMap<String, String>) {
    let names = secret_option_names(&location.backend);
    let (secret, public): (BTreeMap<_, _>, BTreeMap<_, _>) = location
        .options
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .partition(|(k, _)| names.contains(&k.as_str()));
    (
        RepoLocation {
            backend: location.backend.clone(),
            options: public,
        },
        secret,
    )
}

/// A location together with the stores opened from it: the server's own and,
/// when the backend can tell them apart, the agent's.
pub struct Opened {
    pub location: RepoLocation,
    pub server: Store,
    pub agent: Option<Store>,
    pub server_creds: Option<AccessKey>,
    pub agent_creds: Option<AccessKey>,
}

pub fn open_parts(
    location: RepoLocation,
    server_creds: Option<AccessKey>,
    agent_creds: Option<AccessKey>,
) -> Result<Opened, ApiError> {
    location.validate().map_err(invalid)?;
    let server = open_store(&location, server_creds.as_ref()).map_err(invalid)?;
    let agent = match location.backend.as_str() {
        "local" => None,
        "s3" => agent_creds
            .as_ref()
            .map(|k| open_store(&location, Some(k)))
            .transpose()
            .map_err(invalid)?,
        _ => {
            let for_agent = location.for_agent();
            if let Some(problem) = location.agent_credentials_problem() {
                return Err(invalid(problem));
            }
            Some(open_store(&for_agent, None).map_err(invalid)?)
        }
    };
    Ok(Opened {
        location,
        server,
        agent,
        server_creds,
        agent_creds,
    })
}

pub fn open_draft(draft: &RepositoryDraft) -> Result<Opened, ApiError> {
    let location = format_location(&draft.location);
    location.validate_new().map_err(invalid)?;
    let (server_creds, agent_creds) = if location.backend == "s3" {
        match (&draft.server_credentials, &draft.agent_credentials) {
            (Some(s), Some(a)) => (Some(s.clone().into()), Some(a.clone().into())),
            _ => {
                return Err(ApiError::Validation(
                    "An S3 repository needs both the server key and the agent key.".into(),
                ));
            }
        }
    } else {
        (None, None)
    };
    open_parts(location, server_creds, agent_creds)
}

pub(crate) fn timed_out_report() -> ProbeReport {
    let note = |status, error: &str| (status, Some(error.to_string()));
    let skipped = || note(CheckStatus::Neutral, SKIPPED_NOTE);
    let outcomes = [
        note(
            CheckStatus::Fail,
            "the storage did not answer within the time allowed for the checks",
        ),
        skipped(),
        skipped(),
        skipped(),
        skipped(),
    ];
    ProbeReport {
        existing_repository: false,
        checks: std::array::from_fn(|i| Check {
            id: CHECK_ORDER[i],
            status: outcomes[i].0,
            error: outcomes[i].1.clone(),
            detail: None,
        }),
        agent_key: AgentKeyState::Unchecked,
        guarantee: GuaranteeLevel::Unchecked,
        weaknesses: Vec::new(),
    }
}

/// The write, list, conditional write and agent delete checks, bounded in time.
pub async fn run_storage_probe(opened: &Opened) -> ProbeReport {
    let keys = ProbeKeys::default();
    let probing = async {
        match &opened.agent {
            Some(agent) => probe_keep(&opened.server, AgentProbe::Store(agent), &keys).await,
            None => probe_keep(&opened.server, AgentProbe::NotEnforceable, &keys).await,
        }
    };
    let mut report = match tokio::time::timeout(PROBE_TIMEOUT, probing).await {
        Ok(report) => report,
        Err(_) => {
            tracing::warn!("storage checks timed out after {PROBE_TIMEOUT:?}");
            timed_out_report()
        }
    };
    match tokio::time::timeout(CLEANUP_TIMEOUT, remove_probe_objects(&opened.server, &keys)).await {
        Ok(failed) => {
            for (key, error) in failed {
                tracing::warn!(%key, %error, "could not remove the probe object");
            }
        }
        Err(_) => tracing::warn!("removing the probe objects timed out after {CLEANUP_TIMEOUT:?}"),
    }
    for check in &mut report.checks {
        if let Some(detail) = &check.detail {
            tracing::warn!(
                check = check.id.as_str(),
                status = ?check.status,
                error = %detail,
                "storage check failed"
            );
        }
    }
    report
}

/// A lifecycle rule that removes objects caps the level; rules that cannot be
/// read follow the one D-17 rule; a read that did not happen leaves the level
/// unmeasured instead of lowering it.
pub(crate) fn apply_lifecycle(report: &mut ProbeReport, verdict: LifecycleVerdict) {
    match verdict {
        LifecycleVerdict::NoRules => {}
        LifecycleVerdict::Deletes => report.cap_guarantee(Weakness::LifecycleDeletes),
        LifecycleVerdict::Unverified => report.lifecycle_unverified(),
        LifecycleVerdict::Inconclusive => report.guarantee = GuaranteeLevel::Unchecked,
    }
}

pub async fn run_probe(opened: &Opened) -> ProbeReport {
    let mut report = run_storage_probe(opened).await;
    if report.check(CheckId::Write).status == CheckStatus::Fail {
        return report;
    }
    let (status, error) = match &opened.server_creds {
        Some(creds) if opened.location.backend == "s3" => {
            object_lock::check(&opened.location, creds).await
        }
        _ => (CheckStatus::Neutral, Some("not_applicable".to_string())),
    };
    report.set_object_lock(status, error);
    if let Some(creds) = opened
        .server_creds
        .as_ref()
        .filter(|_| opened.location.backend == "s3")
    {
        let verdict = object_lock::lifecycle_check(&opened.location, creds).await;
        apply_lifecycle(&mut report, verdict);
    }
    if !matches!(opened.location.backend.as_str(), "s3" | "local") {
        report.cap_guarantee(Weakness::SharedCredentials);
    }
    report
}

pub async fn run_checks(draft: &RepositoryDraft) -> Result<ProbeReport, ApiError> {
    let opened = open_draft(draft)?;
    Ok(run_probe(&opened).await)
}

pub fn agent_key_state_name(report: &ProbeReport) -> &'static str {
    match report.agent_key_state() {
        AgentKeyState::Verified => "verified",
        AgentKeyState::CanDelete => "can_delete",
        AgentKeyState::CanOverwrite => "can_overwrite",
        AgentKeyState::Unchecked => "unchecked",
    }
}

pub fn to_result(report: &ProbeReport) -> CheckResult {
    CheckResult {
        existing_repository: report.existing_repository,
        blocking: report.blocking(),
        agent_key_state: agent_key_state_name(report).to_string(),
        guarantee_level: report.guarantee.as_str().to_string(),
        guarantee_notes: report
            .weaknesses
            .iter()
            .map(|w| w.as_str().to_string())
            .collect(),
        checks: report
            .checks
            .iter()
            .map(|c| CheckDto {
                id: c.id.as_str().to_string(),
                status: match c.status {
                    CheckStatus::Pass => "pass",
                    CheckStatus::Warn => "warn",
                    CheckStatus::Fail => "fail",
                    CheckStatus::Neutral => "neutral",
                }
                .to_string(),
                error: c.error.clone(),
            })
            .collect(),
    }
}

/// A new repository on this storage needs the user's explicit agreement: a host
/// can change or delete backups, or the check could not tell.
pub fn needs_consent(result: &CheckResult) -> bool {
    matches!(
        GuaranteeLevel::parse(&result.guarantee_level),
        Some(GuaranteeLevel::HostCanOverwriteOrDelete | GuaranteeLevel::Unchecked)
    )
}

/// The refusal text sent with the `weaker_guarantee` conflict.
pub fn consent_message(result: &CheckResult, verb: &str) -> String {
    if GuaranteeLevel::parse(&result.guarantee_level) == Some(GuaranteeLevel::Unchecked) {
        return format!(
            "The check could not measure what a host can do with this storage. Run the check \
             again, or send accept_weaker_guarantee to {verb} the repository anyway."
        );
    }
    let reasons: Vec<&str> = result
        .guarantee_notes
        .iter()
        .filter_map(|n| Weakness::parse(n))
        .map(Weakness::message)
        .collect();
    format!(
        "This storage does not stop a host from destroying backups. {} Send \
         accept_weaker_guarantee to {verb} the repository anyway.",
        reasons.join(" ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_secret_option_is_split_off() {
        for (backend, secrets, public) in [
            (
                "azblob",
                &["account_key", "sas_token", "encryption_key"][..],
                &["container", "account_name", "encryption_key_sha256"][..],
            ),
            (
                "gcs",
                &["credential", "token"][..],
                &["bucket", "credential_path"][..],
            ),
            ("sftp", &["key"][..], &["user", "host_key"][..]),
        ] {
            let location = RepoLocation::new(
                backend,
                secrets.iter().chain(public).map(|k| (*k, "SECRETVALUE")),
            );
            let (stored, sealed) = split_secret_options(&location);
            let sealed: Vec<&str> = sealed.keys().map(String::as_str).collect();
            let mut expected = secrets.to_vec();
            expected.sort_unstable();
            assert_eq!(sealed, expected, "{backend}");
            assert!(
                stored.options.keys().all(|k| public.contains(&k.as_str())),
                "{backend}: {:?}",
                stored.options.keys()
            );
        }
    }

    fn at(level: GuaranteeLevel) -> ProbeReport {
        ProbeReport {
            guarantee: level,
            ..timed_out_report()
        }
    }

    #[test]
    fn apply_lifecycle_follows_the_table() {
        use GuaranteeLevel::*;
        use LifecycleVerdict::*;
        let case = |level, verdict| {
            let mut report = at(level);
            apply_lifecycle(&mut report, verdict);
            (report.guarantee, report.weaknesses)
        };
        let deletes = vec![Weakness::LifecycleDeletes];
        let unverified = vec![Weakness::LifecycleUnverified];
        for level in [
            CreateOnlyEnforced,
            OverwriteKeepsOldVersion,
            HostCanOverwriteOrDelete,
        ] {
            assert_eq!(case(level, NoRules), (level, vec![]));
            assert_eq!(
                case(level, Deletes),
                (HostCanOverwriteOrDelete, deletes.clone())
            );
            assert_eq!(case(level, Inconclusive), (Unchecked, vec![]));
        }
        assert_eq!(
            case(OverwriteKeepsOldVersion, Unverified),
            (HostCanOverwriteOrDelete, unverified.clone())
        );
        assert_eq!(
            case(CreateOnlyEnforced, Unverified),
            (CreateOnlyEnforced, unverified.clone())
        );
        assert_eq!(
            case(HostCanOverwriteOrDelete, Unverified),
            (HostCanOverwriteOrDelete, unverified)
        );
        assert_eq!(case(Unchecked, Unverified), (Unchecked, vec![]));
        assert_eq!(case(Unchecked, Deletes), (Unchecked, vec![]));
    }
}
