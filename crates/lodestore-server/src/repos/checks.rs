use std::collections::BTreeMap;
use std::fmt::Display;
use std::time::{Duration, SystemTime};

use zeroize::Zeroizing;

use lodestore_format::store::azure_sas::{sas_https_only, sign_container_sas};
use lodestore_format::store::location::secret_option_names;
use lodestore_format::store::probe::{
    AgentKeyState, AgentProbe, CHECK_ORDER, Check, CheckId, CheckStatus, GuaranteeLevel, ProbeKeys,
    ProbeReport, SKIPPED_NOTE, Weakness, probe_keep, remove_probe_objects,
};
use lodestore_format::store::{RepoLocation, StaticCredentials, Store};

use crate::error::ApiError;
use crate::repos::db::{self, AccessKey, AgentOptions};
use crate::repos::gcs_lifecycle;
use crate::repos::object_lock::{self, LifecycleVerdict};
use crate::repos::{CheckDto, CheckResult, LocationDto, RepositoryDraft};
use crate::state::AppState;

pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
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

/// The options an agent may bring of its own, by backend: credentials only.
/// An Azure account key is not among them, since it opens the whole account,
/// deletion included.
pub fn agent_option_names(backend: &str) -> &'static [&'static str] {
    match backend {
        "azblob" => &["sas_token"],
        "gcs" => &["credential"],
        "sftp" => &["user", "key"],
        _ => &[],
    }
}

/// The server's credential options that an agent's own take the place of.
pub fn server_credential_names(backend: &str) -> &'static [&'static str] {
    match backend {
        "azblob" => &["account_key", "sas_token"],
        "gcs" => &["credential", "token"],
        "sftp" => &["key"],
        _ => &[],
    }
}

pub fn check_agent_options(
    backend: &str,
    options: &BTreeMap<String, String>,
) -> Result<(), ApiError> {
    if options.is_empty() {
        return Ok(());
    }
    let names = agent_option_names(backend);
    if names.is_empty() {
        return Err(ApiError::Validation(
            "agent_options are for azblob, gcs and sftp; an S3 repository takes agent_credentials"
                .into(),
        ));
    }
    if let Some(name) = options
        .iter()
        .find_map(|(k, v)| (!names.contains(&k.as_str()) || v.is_empty()).then_some(k))
    {
        return Err(ApiError::Validation(format!(
            "{backend} agent_options support only: {}; `{name}` is not accepted or is empty",
            names.join(", ")
        )));
    }
    Ok(())
}

fn agent_overlay(location: &RepoLocation, agent: &AgentOptions) -> RepoLocation {
    let mut overlaid = location.clone();
    if agent.is_empty() {
        return overlaid;
    }
    for name in server_credential_names(&location.backend) {
        overlaid.options.remove(*name);
    }
    overlaid
        .options
        .extend(agent.iter().map(|(k, v)| (k.clone(), v.to_string())));
    overlaid
}

/// The location an agent opens: the server's own credentials give way to the
/// agent's when it has them, and `for_agent` closes the ambient ones. The probe
/// and the material sent to the agent both come from here.
pub fn agent_location(location: &RepoLocation, agent: &AgentOptions) -> RepoLocation {
    agent_overlay(location, agent).for_agent()
}

/// Whether the server signs a SAS for the agents of this location: an Azure
/// repository with the account key and no agent options of its own.
pub fn vends_sas(location: &RepoLocation, agent: &AgentOptions) -> bool {
    let set = |name: &str| location.options.get(name).is_some_and(|v| !v.is_empty());
    location.backend == "azblob" && agent.is_empty() && set("account_name") && set("account_key")
}

/// [`agent_location`], except that a vending repository gets a container SAS
/// signed now in place of the account key; the second element is its expiry.
/// The probe and the agent material both come from here.
pub fn agent_location_with(
    location: &RepoLocation,
    agent: &AgentOptions,
    sas_vending: bool,
) -> lodestore_format::Result<(RepoLocation, Option<SystemTime>)> {
    if !(sas_vending && vends_sas(location, agent)) {
        return Ok((agent_location(location, agent), None));
    }
    let opt = |name: &str| location.options.get(name).map_or("", String::as_str);
    let signed = sign_container_sas(
        opt("account_name"),
        opt("account_key"),
        opt("container"),
        SystemTime::now(),
        sas_https_only(opt("endpoint")),
    )?;
    let mut vended = location.clone();
    for name in server_credential_names("azblob") {
        vended.options.remove(*name);
    }
    vended
        .options
        .insert("sas_token".to_string(), signed.token.to_string());
    Ok((vended.for_agent(), Some(signed.expires_at)))
}

/// A location together with the stores opened from it: the server's own and,
/// when the backend can tell them apart, the agent's.
pub struct Opened {
    pub location: RepoLocation,
    pub server: Store,
    pub agent: Option<Store>,
    pub agent_location: Option<RepoLocation>,
    pub server_creds: Option<AccessKey>,
    pub agent_creds: Option<AccessKey>,
    pub agent_options: AgentOptions,
    pub sas_vending: bool,
}

pub fn open_parts(
    location: RepoLocation,
    server_creds: Option<AccessKey>,
    agent_creds: Option<AccessKey>,
    agent_options: AgentOptions,
) -> Result<Opened, ApiError> {
    open_parts_with(location, server_creds, agent_creds, agent_options, false)
}

pub fn open_parts_with(
    location: RepoLocation,
    server_creds: Option<AccessKey>,
    agent_creds: Option<AccessKey>,
    agent_options: AgentOptions,
    sas_vending: bool,
) -> Result<Opened, ApiError> {
    let sas_vending = sas_vending && vends_sas(&location, &agent_options);
    location.validate().map_err(invalid)?;
    let server = open_store(&location, server_creds.as_ref()).map_err(invalid)?;
    let (agent, agent_at) = match location.backend.as_str() {
        "local" => (None, None),
        "s3" => (
            agent_creds
                .as_ref()
                .map(|k| open_store(&location, Some(k)))
                .transpose()
                .map_err(invalid)?,
            None,
        ),
        _ => {
            if let Some(problem) =
                agent_overlay(&location, &agent_options).agent_credentials_problem()
            {
                return Err(invalid(problem));
            }
            let (agent_at, _) =
                agent_location_with(&location, &agent_options, sas_vending).map_err(invalid)?;
            let store = open_store(&agent_at, None).map_err(invalid)?;
            (Some(store), Some(agent_at))
        }
    };
    Ok(Opened {
        location,
        server,
        agent,
        agent_location: agent_at,
        server_creds,
        agent_creds,
        agent_options,
        sas_vending,
    })
}

pub fn open_draft(draft: &RepositoryDraft) -> Result<Opened, ApiError> {
    open_draft_with(draft, false)
}

pub fn open_draft_with(draft: &RepositoryDraft, allow_vending: bool) -> Result<Opened, ApiError> {
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
    let agent_options = draft.agent_options.clone().unwrap_or_default();
    check_agent_options(&location.backend, &agent_options)?;
    let agent_options = agent_options
        .into_iter()
        .map(|(k, v)| (k, Zeroizing::new(v)))
        .collect();
    open_parts_with(
        location,
        server_creds,
        agent_creds,
        agent_options,
        allow_vending,
    )
}

pub(crate) fn timed_out_report() -> ProbeReport {
    unchecked_report("the storage did not answer within the time allowed for the checks")
}

pub(crate) fn unchecked_report(message: &str) -> ProbeReport {
    let note = |status, error: &str| (status, Some(error.to_string()));
    let skipped = || note(CheckStatus::Neutral, SKIPPED_NOTE);
    let outcomes = [
        note(CheckStatus::Fail, message),
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
        host_key_changed: None,
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
                let error = redact_signatures(&error.to_string());
                tracing::warn!(%key, %error, "could not remove the probe object");
            }
        }
        Err(_) => tracing::warn!("removing the probe objects timed out after {CLEANUP_TIMEOUT:?}"),
    }
    for check in &mut report.checks {
        check.error = check.error.as_deref().map(redact_signatures);
        check.detail = check.detail.as_deref().map(redact_signatures);
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

/// Hides the value of every `sig=` (an Azure SAS signature) in an error text.
fn redact_signatures(text: &str) -> String {
    const MARKS: [&str; 2] = ["sig=", "sig%3d"];
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some((found, mark)) = MARKS
        .iter()
        .filter_map(|mark| lower[at..].find(mark).map(|found| (found, mark.len())))
        .min()
    {
        let start = at + found + mark;
        out.push_str(&text[at..start]);
        out.push_str("<redacted>");
        at = text[start..]
            .find(|c: char| c == '&' || c == '"' || c == '\'' || c.is_whitespace())
            .map_or(text.len(), |end| start + end);
    }
    out.push_str(&text[at..]);
    out
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
    match opened.location.backend.as_str() {
        "gcs" => {
            let verdict = gcs_lifecycle::check(&opened.location).await;
            apply_lifecycle(&mut report, verdict);
        }
        "azblob" => report.lifecycle_unverified(),
        _ => {}
    }
    if !matches!(opened.location.backend.as_str(), "s3" | "local")
        && opened.agent_options.is_empty()
        && !opened.sas_vending
    {
        report.cap_guarantee(Weakness::SharedCredentials);
    }
    report
}

/// True only for a service SAS that grants nothing beyond read, create and
/// list. An account SAS, a missing `sr` or `sp`, any other permission letter
/// or an unreadable token is wide.
pub fn azure_sas_is_narrow(token: &str) -> bool {
    let (mut service, mut permissions) = (false, false);
    for pair in token.trim_start_matches('?').split('&') {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        if name.eq_ignore_ascii_case("ss") || name.eq_ignore_ascii_case("srt") {
            return false;
        }
        if name.eq_ignore_ascii_case("sr") {
            service = true;
        } else if name.eq_ignore_ascii_case("sp") {
            if value.is_empty() || !value.chars().all(|c| matches!(c, 'r' | 'c' | 'l')) {
                return false;
            }
            permissions = true;
        }
    }
    service && permissions
}

/// The Azure account name, lowercase: the `account_name` option, else the
/// first label of a standard Azure Blob host. `None` when it cannot be told.
pub fn azure_account_name(options: &BTreeMap<String, String>) -> Option<String> {
    if let Some(name) = options.get("account_name").map(|n| n.trim())
        && !name.is_empty()
    {
        return Some(name.to_ascii_lowercase());
    }
    let endpoint = options.get("endpoint")?;
    let rest = endpoint
        .split_once("://")
        .map_or(endpoint.as_str(), |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    let host = host
        .split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let name = if let Some(prefix) = host.strip_suffix(".blob.core.windows.net") {
        prefix.strip_suffix(".privatelink").unwrap_or(prefix)
    } else {
        let prefix = host.strip_suffix(".blob.storage.azure.net")?;
        let (name, zone) = prefix.rsplit_once('.')?;
        let digits = zone.strip_prefix('z')?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        name
    };
    (!name.is_empty() && !name.contains('.')).then(|| name.to_string())
}

/// Two accounts are different only when both names are known and differ.
pub fn same_azure_account(a: Option<&str>, b: Option<&str>) -> bool {
    !matches!((a, b), (Some(a), Some(b)) if a != b)
}

/// Whether the agents of a repository hold credentials that reach beyond the
/// repository's own container: the account key, or a SAS that is not narrow.
pub fn agent_creds_wide(location: &RepoLocation, agent: &AgentOptions, sas_vending: bool) -> bool {
    if sas_vending {
        return false;
    }
    if !agent.is_empty() {
        return agent
            .get("sas_token")
            .is_none_or(|sas| !azure_sas_is_narrow(sas));
    }
    if location
        .options
        .get("account_key")
        .is_some_and(|k| !k.is_empty())
    {
        return true;
    }
    location
        .options
        .get("sas_token")
        .is_none_or(|sas| !azure_sas_is_narrow(sas))
}

/// One Azure repository of the catalog as the credentials of its agents make it
/// matter to another.
pub struct AzureNeighbour {
    pub id: String,
    pub account_name: Option<String>,
    pub container: String,
    pub wide: bool,
    /// The agents do not get the server's own credentials.
    pub protected: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Neighbourhood {
    pub same_container: bool,
    pub account_wide: bool,
}

pub fn neighbourhood_of(our: &RepoLocation, neighbours: &[AzureNeighbour]) -> Neighbourhood {
    let name = azure_account_name(&our.options);
    let container = our.options.get("container");
    let mut found = Neighbourhood {
        same_container: false,
        account_wide: false,
    };
    for n in neighbours {
        if !same_azure_account(name.as_deref(), n.account_name.as_deref()) {
            continue;
        }
        found.same_container |= container == Some(&n.container);
        found.account_wide |= n.wide;
    }
    found
}

pub async fn azure_neighbourhood(
    state: &AppState,
    location: &RepoLocation,
    exclude_id: Option<&str>,
) -> Result<Neighbourhood, ApiError> {
    if location.backend != "azblob" {
        return Ok(Neighbourhood {
            same_container: false,
            account_wide: false,
        });
    }
    let neighbours = db::azure_agent_credentials(state, exclude_id).await?;
    Ok(neighbourhood_of(location, &neighbours))
}

/// [`run_probe`], and for an Azure repository whose agents do not get the
/// server's credentials, the cap that other repositories of the same account
/// put on its level.
pub async fn run_probe_in(
    state: &AppState,
    opened: &Opened,
    exclude_id: Option<&str>,
) -> Result<ProbeReport, ApiError> {
    let mut report = run_probe(opened).await;
    if opened.location.backend == "azblob"
        && (opened.sas_vending || !opened.agent_options.is_empty())
    {
        let around = azure_neighbourhood(state, &opened.location, exclude_id).await?;
        if around.same_container || around.account_wide {
            report.cap_guarantee(Weakness::SharedCredentials);
        }
    }
    Ok(report)
}

/// Before a repository takes wide credentials or a container of another one,
/// lowers the stored level of every protected repository of the same account
/// that this puts at risk. Returns the ids it lowered.
pub async fn cap_exposed_neighbours(
    state: &AppState,
    changed: &RepoLocation,
    changed_agent: &AgentOptions,
    changed_vending: bool,
    exclude_id: Option<&str>,
) -> Result<Vec<String>, ApiError> {
    if changed.backend != "azblob" {
        return Ok(Vec::new());
    }
    let wide = agent_creds_wide(changed, changed_agent, changed_vending);
    let name = azure_account_name(&changed.options);
    let container = changed.options.get("container");
    let mut lowered = Vec::new();
    for n in db::azure_agent_credentials(state, exclude_id).await? {
        let exposed = n.protected
            && same_azure_account(name.as_deref(), n.account_name.as_deref())
            && (wide || container == Some(&n.container));
        if exposed && db::cap_guarantee_shared(state, &n.id).await? {
            lowered.push(n.id);
        }
    }
    Ok(lowered)
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
        host_key: None,
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

    fn agent(pairs: &[(&str, &str)]) -> AgentOptions {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), Zeroizing::new(v.to_string())))
            .collect()
    }

    #[test]
    fn agent_location_overlays_only_credentials() {
        let azure = RepoLocation::new(
            "azblob",
            [
                ("container", "c"),
                ("account_name", "n"),
                ("account_key", "SRV"),
                ("encryption_key", "E"),
            ],
        );
        let got = agent_location(&azure, &agent(&[("sas_token", "AGT")]));
        assert_eq!(got.options["sas_token"], "AGT");
        assert_eq!(got.options["encryption_key"], "E");
        assert!(!got.options.contains_key("account_key"));

        let gcs = RepoLocation::new(
            "gcs",
            [
                ("bucket", "b"),
                ("credential", "SRV"),
                ("token", "SRV"),
                ("credential_path", "/x"),
            ],
        );
        let got = agent_location(&gcs, &agent(&[("credential", "AGT")]));
        assert_eq!(got.options["credential"], "AGT");
        assert!(!got.options.contains_key("token"));
        assert!(!got.options.contains_key("credential_path"));
        assert_eq!(got.options["disable_vm_metadata"], "true");
        assert_eq!(got.options["disable_config_load"], "true");

        let sftp = RepoLocation::new(
            "sftp",
            [
                ("endpoint", "h:22"),
                ("user", "srv"),
                ("key", "SRV"),
                ("host_key", "HK"),
                ("root", "/r"),
            ],
        );
        let got = agent_location(&sftp, &agent(&[("user", "agent"), ("key", "AGT")]));
        assert_eq!(got.options["user"], "agent");
        assert_eq!(got.options["key"], "AGT");
        assert_eq!(got.options["host_key"], "HK");
        assert_eq!(got.options["endpoint"], "h:22");
        assert_eq!(got.options["root"], "/r");

        for location in [&azure, &gcs, &sftp] {
            assert_eq!(agent_location(location, &agent(&[])), location.for_agent());
        }
    }

    fn account_key() -> String {
        data_encoding::BASE64.encode(&[7u8; 64])
    }

    fn vending_location(extra: &[(&str, &str)]) -> RepoLocation {
        let mut options = vec![
            ("container", "vended"),
            ("account_name", "lodeacct"),
            ("endpoint", "https://lodeacct.blob.core.windows.net"),
            ("root", "r"),
            ("encryption_key_sha256", "H"),
        ];
        options.extend_from_slice(extra);
        RepoLocation::new("azblob", options)
    }

    #[test]
    fn vends_sas_needs_the_account_key_and_no_agent_options() {
        let key = account_key();
        let with_key = vending_location(&[("account_key", &key)]);
        assert!(vends_sas(&with_key, &agent(&[])));
        assert!(!vends_sas(&with_key, &agent(&[("sas_token", "sv=1")])));
        assert!(!vends_sas(&vending_location(&[]), &agent(&[])));
        assert!(!vends_sas(
            &vending_location(&[("sas_token", "sv=1&sig=x")]),
            &agent(&[])
        ));
        let mut nameless = with_key.clone();
        nameless.options.remove("account_name");
        assert!(!vends_sas(&nameless, &agent(&[])));
        for backend in ["gcs", "sftp", "s3"] {
            let other = RepoLocation::new(
                backend,
                [("account_name", "n"), ("account_key", key.as_str())],
            );
            assert!(!vends_sas(&other, &agent(&[])), "{backend}");
        }
    }

    #[test]
    fn agent_location_with_vends_a_sas_instead_of_the_account_key() {
        let key = account_key();
        let full = vending_location(&[("account_key", &key), ("encryption_key", "E")]);
        let (got, expires) = agent_location_with(&full, &agent(&[]), true).unwrap();
        assert!(!got.options.contains_key("account_key"));
        let sas = &got.options["sas_token"];
        assert!(sas.contains("sp=rcl") && sas.contains("sr=c"), "{sas}");
        for kept in [
            "container",
            "root",
            "endpoint",
            "account_name",
            "encryption_key",
            "encryption_key_sha256",
        ] {
            assert_eq!(got.options[kept], full.options[kept], "{kept}");
        }
        let left = expires
            .unwrap()
            .duration_since(SystemTime::now())
            .unwrap()
            .as_secs();
        assert!(
            (23 * 3600 + 40 * 60..=23 * 3600 + 50 * 60).contains(&left),
            "{left}"
        );

        let (same, none) = agent_location_with(&full, &agent(&[]), false).unwrap();
        assert_eq!(same, agent_location(&full, &agent(&[])));
        assert!(none.is_none());
        let own = agent(&[("sas_token", "AGT")]);
        let (kept, none) = agent_location_with(&full, &own, true).unwrap();
        assert_eq!(kept.options["sas_token"], "AGT");
        assert!(none.is_none());
    }

    #[test]
    fn agent_location_with_refuses_a_bad_account_key() {
        let bad = data_encoding::BASE64.encode(b"too short");
        for value in ["not base64 !!", bad.as_str()] {
            let full = vending_location(&[("account_key", value)]);
            let error = agent_location_with(&full, &agent(&[]), true)
                .unwrap_err()
                .to_string();
            assert!(!error.contains(value), "{error}");
        }
    }

    #[test]
    fn redact_signatures_hides_every_sig() {
        assert_eq!(
            redact_signatures("https://a.blob/c/k?sv=1&sig=AbC%2F&se=2"),
            "https://a.blob/c/k?sv=1&sig=<redacted>&se=2"
        );
        assert_eq!(redact_signatures("SIG=x y"), "SIG=<redacted> y");
        assert_eq!(
            redact_signatures("a?sig=1&b sig=2\""),
            "a?sig=<redacted>&b sig=<redacted>\""
        );
        assert_eq!(redact_signatures("nothing here"), "nothing here");
    }

    #[test]
    fn redact_signatures_hides_a_percent_encoded_sig() {
        assert_eq!(
            redact_signatures("u=https%3A%2F%2Fa%3Fsv%3D1%26sig%3DAbC%252F%26se%3D2 x"),
            "u=https%3A%2F%2Fa%3Fsv%3D1%26sig%3D<redacted> x"
        );
        assert_eq!(
            redact_signatures("Sig%3dabc&SIG=def"),
            "Sig%3d<redacted>&SIG=<redacted>"
        );
    }

    mod allow_list {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn check_agent_options_accepts_exactly_the_allow_list(
                backend in prop::sample::select(vec!["s3", "local", "azblob", "gcs", "sftp"]),
                options in prop::collection::btree_map(
                    prop::sample::select(vec![
                        "sas_token", "credential", "user", "key", "account_key", "endpoint",
                        "bucket", "container", "root", "host_key", "publish",
                        "credential_path", "token", "encryption_key", "sas_key", "Token",
                    ]),
                    prop_oneof![Just(String::new()), "[a-z]{1,6}"],
                    0..5,
                ),
            ) {
                let options: BTreeMap<String, String> =
                    options.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
                let names = agent_option_names(backend);
                let expected = options.is_empty()
                    || (!names.is_empty()
                        && options
                            .iter()
                            .all(|(k, v)| names.contains(&k.as_str()) && !v.is_empty()));
                prop_assert_eq!(check_agent_options(backend, &options).is_ok(), expected);
            }
        }

        #[test]
        fn allow_list_names_match_the_documented_set() {
            assert_eq!(agent_option_names("azblob"), ["sas_token"]);
            assert_eq!(agent_option_names("gcs"), ["credential"]);
            assert_eq!(agent_option_names("sftp"), ["user", "key"]);
            let bad = BTreeMap::from([("account_key".to_string(), "x".to_string())]);
            let error = check_agent_options("azblob", &bad).unwrap_err().to_string();
            assert!(error.contains("sas_token"), "{error}");
        }
    }

    #[test]
    fn azure_sas_is_narrow_follows_the_table() {
        for (token, narrow) in [
            ("sv=2022-11-02&sr=c&sp=rcl&se=2030", true),
            ("?sr=c&sp=r", true),
            ("sr=c&sp=rcl&sp=d", false),
            ("sr=c&sp=rcwl", false),
            ("sr=c&sp=rcla", false),
            ("sr=c&sp=rcld", false),
            ("sr=c&sp=rclt", false),
            ("sr=c&sp=rclx", false),
            ("sr=c&sp=rcly", false),
            ("ss=b&srt=sco&sp=rcl", false),
            ("sr=c&sp=rcl&srt=o", false),
            ("sp=rcl", false),
            ("sr=c", false),
            ("sr=c&sp=", false),
            ("sr=c&sp", false),
            ("", false),
            ("%%%garbage&=&&", false),
        ] {
            assert_eq!(azure_sas_is_narrow(token), narrow, "{token}");
        }
    }

    fn options(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    const SPELLINGS: [&str; 6] = [
        "https://acct.blob.core.windows.net",
        "HTTP://ACCT.blob.core.windows.net",
        "https://acct.blob.core.windows.net:443/",
        "https://acct.privatelink.blob.core.windows.net/",
        "https://acct.z12.blob.storage.azure.net",
        "https://user:pw@acct.blob.core.windows.net:8443/path?x=1",
    ];

    #[test]
    fn azure_account_name_treats_every_spelling_as_one_account() {
        assert_eq!(
            azure_account_name(&options(&[("account_name", "Acct")])),
            Some("acct".to_string())
        );
        for endpoint in SPELLINGS {
            assert_eq!(
                azure_account_name(&options(&[("endpoint", endpoint)])),
                Some("acct".to_string()),
                "{endpoint}"
            );
            assert_eq!(
                azure_account_name(&options(&[
                    ("endpoint", endpoint),
                    ("account_name", "Acct")
                ])),
                Some("acct".to_string()),
                "{endpoint}"
            );
        }
        for endpoint in [
            "https://backups.example.com",
            "https://x.azureedge.net",
            "http://127.0.0.1:10000/devstoreaccount1",
            "https://a.b.blob.core.windows.net",
            "https://acct.zz.blob.storage.azure.net",
            "https://.blob.core.windows.net",
            "[::1]:1",
            "",
            "://",
        ] {
            assert_eq!(
                azure_account_name(&options(&[("endpoint", endpoint)])),
                None,
                "{endpoint}"
            );
        }
        assert_eq!(azure_account_name(&options(&[])), None);
    }

    #[test]
    fn same_azure_account_needs_proof_of_difference() {
        assert!(!same_azure_account(Some("a"), Some("b")));
        assert!(same_azure_account(Some("a"), Some("a")));
        assert!(same_azure_account(None, Some("a")));
        assert!(same_azure_account(Some("a"), None));
        assert!(same_azure_account(None, None));
        for first in SPELLINGS {
            for second in SPELLINGS {
                let name = |e| azure_account_name(&options(&[("endpoint", e)]));
                assert!(
                    same_azure_account(name(first).as_deref(), name(second).as_deref()),
                    "{first} {second}"
                );
            }
        }
    }

    fn azure_at(extra: &[(&str, &str)]) -> RepoLocation {
        let mut all = vec![("container", "c"), ("account_name", "acct")];
        all.extend_from_slice(extra);
        RepoLocation::new("azblob", all)
    }

    #[test]
    fn agent_creds_wide_follows_the_table() {
        let key = account_key();
        let narrow = "sr=c&sp=rcl&sv=1";
        let wide = "sr=c&sp=racwdl&sv=1";
        let keyed = azure_at(&[("account_key", &key)]);
        assert!(!agent_creds_wide(&keyed, &agent(&[]), true));
        assert!(!agent_creds_wide(
            &keyed,
            &agent(&[("sas_token", narrow)]),
            false
        ));
        assert!(agent_creds_wide(
            &keyed,
            &agent(&[("sas_token", wide)]),
            false
        ));
        assert!(agent_creds_wide(&keyed, &agent(&[]), false));
        assert!(!agent_creds_wide(
            &azure_at(&[("sas_token", narrow)]),
            &agent(&[]),
            false
        ));
        assert!(agent_creds_wide(
            &azure_at(&[("sas_token", wide)]),
            &agent(&[]),
            false
        ));
        assert!(agent_creds_wide(&azure_at(&[]), &agent(&[]), false));
    }

    fn neighbour(account: Option<&str>, container: &str, wide: bool) -> AzureNeighbour {
        AzureNeighbour {
            id: "n".into(),
            account_name: account.map(str::to_string),
            container: container.into(),
            wide,
            protected: !wide,
        }
    }

    #[test]
    fn neighbourhood_of_follows_the_table() {
        let ours = azure_at(&[]);
        let around = |neighbours: &[AzureNeighbour]| {
            let n = neighbourhood_of(&ours, neighbours);
            (n.same_container, n.account_wide)
        };
        assert_eq!(around(&[]), (false, false));
        assert_eq!(
            around(&[neighbour(Some("acct"), "c", false)]),
            (true, false)
        );
        assert_eq!(around(&[neighbour(None, "c", false)]), (true, false));
        assert_eq!(around(&[neighbour(Some("acct"), "c", true)]), (true, true));
        assert_eq!(around(&[neighbour(Some("acct"), "d", true)]), (false, true));
        assert_eq!(around(&[neighbour(None, "d", true)]), (false, true));
        assert_eq!(
            around(&[neighbour(Some("acct"), "d", false)]),
            (false, false)
        );
        assert_eq!(
            around(&[neighbour(Some("other"), "c", true)]),
            (false, false)
        );
        assert_eq!(
            around(&[neighbour(Some("other"), "d", false)]),
            (false, false)
        );
        assert_eq!(
            around(&[
                neighbour(Some("acct"), "d", false),
                neighbour(Some("acct"), "e", true)
            ]),
            (false, true)
        );
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
