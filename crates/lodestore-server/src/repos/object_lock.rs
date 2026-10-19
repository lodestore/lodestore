use std::time::Duration;

use lodestore_format::store::RepoLocation;
use lodestore_format::store::probe::CheckStatus;
use reqsign_aws_v4::{EMPTY_STRING_SHA256, RequestSigner, StaticCredentialProvider};
use reqsign_core::{Context, OsEnv, Signer};
use reqsign_file_read_tokio::TokioFileRead;

use crate::repos::db::AccessKey;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_REGION: &str = "us-east-1";
const MAX_BODY: usize = 64 * 1024;
const MAX_CODE_LEN: usize = 64;

fn opt<'a>(location: &'a RepoLocation, key: &str) -> Option<&'a str> {
    location
        .options
        .get(key)
        .map(String::as_str)
        .filter(|v| !v.is_empty())
}

fn object_lock_url(location: &RepoLocation) -> String {
    let bucket = opt(location, "bucket").unwrap_or_default();
    let region = opt(location, "region").unwrap_or(DEFAULT_REGION);
    let endpoint = opt(location, "endpoint");
    let virtual_host = match opt(location, "path_style") {
        Some(v) => v == "false",
        None => endpoint.is_none(),
    };
    match (endpoint, virtual_host) {
        (Some(endpoint), false) => {
            format!("{}/{bucket}?object-lock", endpoint.trim_end_matches('/'))
        }
        (Some(endpoint), true) => {
            let (scheme, host) = endpoint.split_once("://").unwrap_or(("https", endpoint));
            format!(
                "{scheme}://{bucket}.{}/?object-lock",
                host.trim_end_matches('/')
            )
        }
        (None, true) => format!("https://{bucket}.s3.{region}.amazonaws.com/?object-lock"),
        (None, false) => format!("https://s3.{region}.amazonaws.com/{bucket}?object-lock"),
    }
}

fn error_code(body: &str) -> Option<&str> {
    let rest = body.split_once("<Code>")?.1;
    let code = rest.split_once("</Code>")?.0;
    (code.len() <= MAX_CODE_LEN
        && !code.is_empty()
        && code.bytes().all(|b| b.is_ascii_alphanumeric()))
    .then_some(code)
}

async fn fetch(
    location: &RepoLocation,
    creds: &AccessKey,
) -> Result<(u16, String), Box<dyn std::error::Error + Send + Sync>> {
    let url = object_lock_url(location);
    let region = opt(location, "region").unwrap_or(DEFAULT_REGION);
    let signer = Signer::new(
        Context::new().with_file_read(TokioFileRead).with_env(OsEnv),
        StaticCredentialProvider::new(&creds.access_key_id, &creds.secret_access_key),
        RequestSigner::new("s3", region),
    );
    let (mut parts, ()) = http::Request::get(&url)
        .header("x-amz-content-sha256", EMPTY_STRING_SHA256)
        .body(())?
        .into_parts();
    signer.sign(&mut parts, None).await?;
    let mut response = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()?
        .get(&url)
        .headers(parts.headers)
        .send()
        .await?;
    let status = response.status().as_u16();
    let mut body = Vec::new();
    while body.len() < MAX_BODY {
        match response.chunk().await? {
            Some(chunk) => body.extend_from_slice(&chunk),
            None => break,
        }
    }
    body.truncate(MAX_BODY);
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

fn verdict(status: u16, body: &str) -> (CheckStatus, Option<String>) {
    match status {
        200 if body.contains("<ObjectLockEnabled>Enabled") => (CheckStatus::Pass, None),
        200 => (CheckStatus::Neutral, None),
        404 if body.contains("ObjectLockConfigurationNotFoundError") => {
            (CheckStatus::Neutral, None)
        }
        405 | 501 => (CheckStatus::Neutral, Some("not_supported".into())),
        403 => (
            CheckStatus::Warn,
            Some("could not check: access denied".into()),
        ),
        _ => (
            CheckStatus::Warn,
            Some(match error_code(body) {
                Some(code) => format!("could not check: HTTP {status} {code}"),
                None => format!("could not check: HTTP {status}"),
            }),
        ),
    }
}

/// Reads the bucket's Object Lock configuration with a signed
/// `GET ?object-lock`. Only a clear "enabled" passes; a missing configuration
/// is neutral, so is a storage that does not implement the call, and an answer
/// that cannot be read is a warning. It never fails the wizard.
pub async fn check(location: &RepoLocation, creds: &AccessKey) -> (CheckStatus, Option<String>) {
    match fetch(location, creds).await {
        Ok((status, body)) => verdict(status, &body),
        Err(error) => {
            tracing::warn!(%error, "could not read the Object Lock setting");
            (
                CheckStatus::Warn,
                Some("could not check: the storage did not answer".into()),
            )
        }
    }
}

fn tag_number(text: &str, tag: &str) -> Option<u64> {
    text.split_once(&format!("<{tag}>"))?
        .1
        .split_once(&format!("</{tag}>"))?
        .0
        .trim()
        .parse()
        .ok()
}

fn parse_default_retention(body: &str) -> Option<Duration> {
    if !body.contains("<ObjectLockEnabled>Enabled") {
        return None;
    }
    let rule = body
        .split_once("<DefaultRetention>")?
        .1
        .split_once("</DefaultRetention>")?
        .0;
    let days = match (tag_number(rule, "Days"), tag_number(rule, "Years")) {
        (Some(days), None) => days,
        (None, Some(years)) => years.checked_mul(365)?,
        _ => return None,
    };
    Some(Duration::from_secs(days.checked_mul(86_400)?))
}

/// The retention the bucket gives every new object, when Object Lock is
/// enabled and a default is set. Any other answer is `None`.
pub async fn default_retention(location: &RepoLocation, creds: &AccessKey) -> Option<Duration> {
    match fetch(location, creds).await {
        Ok((200, body)) => parse_default_retention(&body),
        Ok(_) => None,
        Err(error) => {
            tracing::warn!(%error, "could not read the Object Lock default retention");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_passes() {
        let body = "<ObjectLockConfiguration><ObjectLockEnabled>Enabled</ObjectLockEnabled></ObjectLockConfiguration>";
        assert_eq!(verdict(200, body), (CheckStatus::Pass, None));
    }

    #[test]
    fn missing_configuration_is_neutral() {
        assert_eq!(
            verdict(200, "<ObjectLockConfiguration/>"),
            (CheckStatus::Neutral, None)
        );
        let body = "<Error><Code>ObjectLockConfigurationNotFoundError</Code></Error>";
        assert_eq!(verdict(404, body), (CheckStatus::Neutral, None));
    }

    #[test]
    fn unimplemented_call_is_not_supported() {
        let note = Some("not_supported".to_string());
        assert_eq!(verdict(501, ""), (CheckStatus::Neutral, note.clone()));
        assert_eq!(verdict(405, "<Error/>"), (CheckStatus::Neutral, note));
    }

    #[test]
    fn odd_error_codes_are_not_echoed() {
        let long = format!("<Error><Code>{}</Code></Error>", "A".repeat(65));
        assert_eq!(
            verdict(500, &long),
            (CheckStatus::Warn, Some("could not check: HTTP 500".into()))
        );
        let odd = "<Error><Code>bad code\n<b>x</b></Code></Error>";
        assert_eq!(
            verdict(500, odd),
            (CheckStatus::Warn, Some("could not check: HTTP 500".into()))
        );
    }

    #[test]
    fn access_denied_warns() {
        let body = "<Error><Code>AccessDenied</Code></Error>";
        assert_eq!(
            verdict(403, body),
            (
                CheckStatus::Warn,
                Some("could not check: access denied".into())
            )
        );
    }

    #[test]
    fn other_statuses_warn_with_code() {
        let body = "<Error><Code>InternalError</Code></Error>";
        assert_eq!(
            verdict(500, body),
            (
                CheckStatus::Warn,
                Some("could not check: HTTP 500 InternalError".into())
            )
        );
        assert_eq!(
            verdict(500, ""),
            (CheckStatus::Warn, Some("could not check: HTTP 500".into()))
        );
    }

    #[test]
    fn default_retention_reads_days_and_years() {
        let rule = |inner: &str| {
            format!(
                "<ObjectLockConfiguration><ObjectLockEnabled>Enabled</ObjectLockEnabled>\
                 <Rule><DefaultRetention><Mode>COMPLIANCE</Mode>{inner}</DefaultRetention></Rule>\
                 </ObjectLockConfiguration>"
            )
        };
        let day = Duration::from_secs(86_400);
        assert_eq!(parse_default_retention(&rule("<Days>1</Days>")), Some(day));
        assert_eq!(
            parse_default_retention(&rule("<Years>2</Years>")),
            Some(day * 730)
        );
        assert_eq!(parse_default_retention(&rule("")), None);
        assert_eq!(
            parse_default_retention(&rule("<Days>1</Days><Years>1</Years>")),
            None
        );
        assert_eq!(parse_default_retention(&rule("<Days>x</Days>")), None);
        assert_eq!(parse_default_retention("<ObjectLockConfiguration/>"), None);
    }
}
