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

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use lodestore_testkit::azurite::{
        ACCOUNT_KEY, ACCOUNT_NAME, container_sas_signature, iso8601_utc,
    };
    use proptest::prelude::*;

    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn fields(token: &str) -> Vec<(String, String)> {
        token
            .split('&')
            .map(|pair| {
                let (k, v) = pair.split_once('=').unwrap();
                let v = v
                    .replace("%2B", "+")
                    .replace("%2F", "/")
                    .replace("%3D", "=");
                (k.to_owned(), v)
            })
            .collect()
    }

    fn field<'a>(f: &'a [(String, String)], name: &str) -> &'a str {
        &f.iter().find(|(k, _)| k == name).unwrap().1
    }

    proptest! {
        #[test]
        fn azure_sas_matches_the_independent_signer(
            account in "[a-z0-9]{3,24}",
            container in "[a-z0-9]([a-z0-9]|-[a-z0-9]){2,30}",
            key in proptest::collection::vec(any::<u8>(), 64),
            secs in 1_577_836_800u64..4_102_444_800,
            https_only in any::<bool>(),
        ) {
            let key = BASE64.encode(&key);
            let signed = sign_container_sas(&account, &key, &container, at(secs), https_only).unwrap();
            let f = fields(&signed.token);
            let st = secs - 15 * 60;
            let se = st + 24 * 3600;
            let spr = if https_only { "https" } else { "https,http" };
            prop_assert_eq!(field(&f, "sp"), "rcl");
            prop_assert_eq!(field(&f, "sr"), "c");
            prop_assert_eq!(field(&f, "sv"), "2022-11-02");
            prop_assert_eq!(field(&f, "spr"), spr);
            prop_assert_eq!(field(&f, "st"), iso8601_utc(st));
            prop_assert_eq!(field(&f, "se"), iso8601_utc(se));
            prop_assert_eq!(signed.expires_at, at(se));
            let oracle = container_sas_signature(
                &account,
                &key,
                &container,
                "rcl",
                Some(&iso8601_utc(st)),
                &iso8601_utc(se),
                Some(spr),
                "2022-11-02",
            );
            prop_assert_eq!(field(&f, "sig"), oracle);
        }
    }

    #[test]
    fn azure_sas_reqsign_golden_vector() {
        let expiry =
            Timestamp::from_str("2022-03-01T08:12:34Z").unwrap() + Duration::from_secs(300);
        let sign = ServiceSharedAccessSignature::new(
            "account".to_owned(),
            BASE64.encode(b"key"),
            ServiceSasResource::Blob {
                container: "container".to_owned(),
                blob: "path/to/blob.txt".to_owned(),
            },
            "r".to_owned(),
            expiry,
        );
        let token = sign
            .token()
            .unwrap()
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        assert_eq!(
            token,
            "sv=2020-12-06&se=2022-03-01T08:17:34Z&sp=r&sr=b&sig=CP9a2LIrR9zeG4I4jZjqPetJSXWJ77QeUA7c3GMypyM="
        );
    }

    #[test]
    fn azure_sas_golden_container_token() {
        let signed =
            sign_container_sas(ACCOUNT_NAME, ACCOUNT_KEY, "golden", at(1_700_000_000), true)
                .unwrap();
        assert_eq!(
            &*signed.token,
            "sv=2022-11-02&se=2023-11-15T21:58:20Z&sp=rcl&sr=c&st=2023-11-14T21:58:20Z&spr=https&sig=qDrWYntJCGJuN0ojzbvrr1wDBEDkbuucMUNooPpfuhA%3D"
        );
        let oracle = container_sas_signature(
            ACCOUNT_NAME,
            ACCOUNT_KEY,
            "golden",
            "rcl",
            Some("2023-11-14T21:58:20Z"),
            "2023-11-15T21:58:20Z",
            Some("https"),
            "2022-11-02",
        );
        assert_eq!(oracle, "qDrWYntJCGJuN0ojzbvrr1wDBEDkbuucMUNooPpfuhA=");
    }

    #[test]
    fn azure_sas_refuses_bad_inputs() {
        let key_text = "super-secret-not-base64-key-value";
        let short_key = BASE64.encode(&[7u8; 32]);
        let now = at(1_700_000_000);
        let cases = [
            ("Account", "cont", ACCOUNT_KEY),
            ("ab", "cont", ACCOUNT_KEY),
            ("a2345678901234567890123456", "cont", ACCOUNT_KEY),
            ("account", "Cont", ACCOUNT_KEY),
            ("account", "my_cont", ACCOUNT_KEY),
            ("account", "my--cont", ACCOUNT_KEY),
            ("account", "-cont", ACCOUNT_KEY),
            ("account", "ab", ACCOUNT_KEY),
            ("account", "cont", key_text),
            ("account", "cont", short_key.as_str()),
        ];
        for (account, container, key) in cases {
            let err = sign_container_sas(account, key, container, now, true).unwrap_err();
            assert!(
                matches!(err, Error::InvalidInput(_)),
                "{account} {container}: {err:?}"
            );
            let text = format!("{err} {err:?}");
            assert!(
                !text.contains(key_text)
                    && !text.contains(&short_key)
                    && !text.contains(ACCOUNT_KEY)
            );
        }
        let before_epoch = UNIX_EPOCH - Duration::from_secs(1);
        assert!(sign_container_sas("account", ACCOUNT_KEY, "cont", before_epoch, true).is_err());
    }

    #[test]
    fn azure_sas_https_only_follows_the_endpoint() {
        assert!(sas_https_only(""));
        assert!(sas_https_only("https://x"));
        assert!(sas_https_only("HTTPS://x"));
        assert!(!sas_https_only("http://127.0.0.1:1/a"));
    }

    #[test]
    fn azure_sas_debug_hides_the_token() {
        let signed =
            sign_container_sas(ACCOUNT_NAME, ACCOUNT_KEY, "golden", at(1_700_000_000), true)
                .unwrap();
        let sig = field(&fields(&signed.token), "sig").to_owned();
        let text = format!("{signed:?}");
        assert!(!text.contains(&*signed.token) && !text.contains(&sig) && !text.contains("sig="));
    }
}
