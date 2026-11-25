//! Container SAS for an Azure agent, signed locally with the account key.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use data_encoding::BASE64;
use reqsign_azure_storage::{ServiceSasResource, ServiceSharedAccessSignature};
use reqsign_core::time::Timestamp;
use zeroize::Zeroizing;

use crate::error::{Error, Result};

pub const AGENT_SAS_PERMISSIONS: &str = "rcl";
pub const AGENT_SAS_VERSION: &str = "2022-11-02";
pub const AGENT_SAS_LIFETIME: Duration = Duration::from_secs(24 * 3600);
pub const AGENT_SAS_START_SKEW: Duration = Duration::from_secs(15 * 60);

pub struct SignedSas {
    pub token: Zeroizing<String>,
    pub expires_at: SystemTime,
}

impl fmt::Debug for SignedSas {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignedSas")
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// `https` only for an empty or `https://` endpoint, so an emulator on plain
/// http still gets a usable token.
pub fn sas_https_only(endpoint: &str) -> bool {
    endpoint.is_empty()
        || endpoint
            .get(..8)
            .is_some_and(|p| p.eq_ignore_ascii_case("https://"))
}

fn invalid(what: &str) -> Error {
    Error::InvalidInput(format!("azure SAS: {what}"))
}

fn check_account(name: &str) -> Result<()> {
    let ok = (3..=24).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    ok.then_some(())
        .ok_or_else(|| invalid("the account name must be 3-24 lowercase letters or digits"))
}

fn check_container(name: &str) -> Result<()> {
    let b = name.as_bytes();
    let ok = (3..=63).contains(&b.len())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b.first().is_some_and(u8::is_ascii_alphanumeric)
        && b.last().is_some_and(u8::is_ascii_alphanumeric)
        && !name.contains("--");
    ok.then_some(())
        .ok_or_else(|| invalid("the container name is not a valid Azure container name"))
}

fn check_key(key: &str) -> Result<()> {
    let ok = BASE64
        .decode(key.as_bytes())
        .is_ok_and(|raw| Zeroizing::new(raw).len() == 64);
    ok.then_some(())
        .ok_or_else(|| invalid("the account key is not a base64 value of 64 bytes"))
}

fn timestamp(secs: Option<i64>) -> Result<Timestamp> {
    secs.and_then(|s| Timestamp::from_second(s).ok())
        .ok_or_else(|| invalid("the time is out of range"))
}

fn encode(value: &str) -> String {
    value
        .replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

/// Signs a `rcl` container SAS valid for [`AGENT_SAS_LIFETIME`] from
/// `now - AGENT_SAS_START_SKEW`. No network call is made.
pub fn sign_container_sas(
    account_name: &str,
    account_key: &str,
    container: &str,
    now: SystemTime,
    https_only: bool,
) -> Result<SignedSas> {
    check_account(account_name)?;
    check_container(container)?;
    check_key(account_key)?;

    let now_secs = now
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok());
    let start = now_secs.and_then(|s| s.checked_sub(AGENT_SAS_START_SKEW.as_secs() as i64));
    let expiry = start.and_then(|s| s.checked_add(AGENT_SAS_LIFETIME.as_secs() as i64));
    let (start, expiry) = (timestamp(start)?, timestamp(expiry)?);

    let signer = ServiceSharedAccessSignature::new(
        account_name.to_owned(),
        account_key.to_owned(),
        ServiceSasResource::Container {
            container: container.to_owned(),
        },
        AGENT_SAS_PERMISSIONS.to_owned(),
        expiry,
    )
    .with_start(start)
    .with_protocol(if https_only { "https" } else { "https,http" })
    .with_version(AGENT_SAS_VERSION);
    let pairs = signer
        .token()
        .map_err(|_| invalid("the token could not be signed"))?;
    drop(signer);

    let token = Zeroizing::new(
        pairs
            .iter()
            .map(|(k, v)| format!("{k}={}", encode(v)))
            .collect::<Vec<_>>()
            .join("&"),
    );
    Ok(SignedSas {
        token,
        expires_at: expiry.as_system_time(),
    })
}
