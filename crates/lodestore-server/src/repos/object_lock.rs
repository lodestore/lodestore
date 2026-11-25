use std::time::Duration;

use lodestore_format::store::RepoLocation;
use lodestore_format::store::probe::CheckStatus;
use reqsign_aws_v4::{EMPTY_STRING_SHA256, RequestSigner, StaticCredentialProvider};
use reqsign_core::{Context, OsEnv, Signer};
use reqsign_file_read_tokio::TokioFileRead;

use crate::repos::db::AccessKey;

pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_REGION: &str = "us-east-1";
pub(crate) const MAX_BODY: usize = 64 * 1024;
const MAX_CODE_LEN: usize = 64;

pub(crate) fn opt<'a>(location: &'a RepoLocation, key: &str) -> Option<&'a str> {
    location
        .options
        .get(key)
        .map(String::as_str)
        .filter(|v| !v.is_empty())
}

fn bucket_url(location: &RepoLocation, subresource: &str) -> String {
    let bucket = opt(location, "bucket").unwrap_or_default();
    let region = opt(location, "region").unwrap_or(DEFAULT_REGION);
    let endpoint = opt(location, "endpoint");
    let virtual_host = match opt(location, "path_style") {
        Some(v) => v == "false",
        None => endpoint.is_none(),
    };
    match (endpoint, virtual_host) {
        (Some(endpoint), false) => {
            format!("{}/{bucket}?{subresource}", endpoint.trim_end_matches('/'))
        }
        (Some(endpoint), true) => {
            let (scheme, host) = endpoint.split_once("://").unwrap_or(("https", endpoint));
            format!(
                "{scheme}://{bucket}.{}/?{subresource}",
                host.trim_end_matches('/')
            )
        }
        (None, true) => format!("https://{bucket}.s3.{region}.amazonaws.com/?{subresource}"),
        (None, false) => format!("https://s3.{region}.amazonaws.com/{bucket}?{subresource}"),
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
    subresource: &str,
) -> Result<(u16, String), Box<dyn std::error::Error + Send + Sync>> {
    let url = bucket_url(location, subresource);
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
    match fetch(location, creds, "object-lock").await {
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
    match fetch(location, creds, "object-lock").await {
        Ok((200, body)) => parse_default_retention(&body),
        Ok(_) => None,
        Err(error) => {
            tracing::warn!(%error, "could not read the Object Lock default retention");
            None
        }
    }
}

/// What the bucket's lifecycle rules mean for the repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleVerdict {
    /// No rule, or none that removes objects or earlier versions.
    NoRules,
    /// An enabled rule that removes (or might remove) repository objects.
    Deletes,
    /// The service refuses or cannot say; this will not change by asking again.
    Unverified,
    /// The answer did not arrive (network, timeout, throttling, server error).
    Inconclusive,
}

/// Maps one read of the lifecycle configuration to a verdict. `None` is a
/// failed transfer. A body cut at [`MAX_BODY`] may hide rules, so it counts as
/// deleting.
pub(crate) fn lifecycle_verdict(
    outcome: Option<(u16, &str)>,
    repo_prefix: &str,
) -> LifecycleVerdict {
    let Some((status, body)) = outcome else {
        return LifecycleVerdict::Inconclusive;
    };
    match (status, error_code(body)) {
        (200, _) if root_element(body) != Some("LifecycleConfiguration") => {
            LifecycleVerdict::Unverified
        }
        (200, _) if body.len() >= MAX_BODY || lifecycle_deletes(body, repo_prefix) => {
            LifecycleVerdict::Deletes
        }
        (200, _) | (404, Some("NoSuchLifecycleConfiguration")) => LifecycleVerdict::NoRules,
        (501, _) | (_, Some("NotImplemented")) => LifecycleVerdict::Unverified,
        (408 | 429 | 500..=599, _) | (_, Some("SlowDown" | "RequestTimeout")) => {
            LifecycleVerdict::Inconclusive
        }
        _ => LifecycleVerdict::Unverified,
    }
}

/// Reads the bucket's lifecycle configuration with a signed `GET ?lifecycle`.
pub async fn lifecycle_check(location: &RepoLocation, creds: &AccessKey) -> LifecycleVerdict {
    let prefix = opt(location, "prefix").unwrap_or_default();
    match fetch(location, creds, "lifecycle").await {
        Ok((status, body)) => lifecycle_verdict(Some((status, &body)), prefix),
        Err(error) => {
            tracing::warn!(%error, "could not read the lifecycle rules");
            lifecycle_verdict(None, prefix)
        }
    }
}

enum Tok<'a> {
    Open(&'a str),
    Empty(&'a str),
    Close,
    Text(&'a str),
}

fn tag_end(tag: &str) -> Option<usize> {
    let mut quote = None;
    for (i, b) in tag.bytes().enumerate() {
        match (quote, b) {
            (None, b'"' | b'\'') => quote = Some(b),
            (Some(q), _) if b == q => quote = None,
            (None, b'>') => return Some(i),
            _ => {}
        }
    }
    None
}

/// Splits XML into tags and text, dropping comments, declarations and
/// namespace prefixes. `None` when the markup is cut off.
fn tokens(xml: &str) -> Option<Vec<Tok<'_>>> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find('<') {
        if start > 0 {
            out.push(Tok::Text(&rest[..start]));
        }
        rest = &rest[start..];
        if let Some(r) = rest.strip_prefix("<!--") {
            rest = r.split_once("-->")?.1;
            continue;
        }
        if let Some(r) = rest.strip_prefix("<![CDATA[") {
            let (text, r) = r.split_once("]]>")?;
            out.push(Tok::Text(text));
            rest = r;
            continue;
        }
        let end = tag_end(rest)?;
        let inner = &rest[1..end];
        rest = &rest[end + 1..];
        if inner.starts_with(['?', '!']) {
            continue;
        }
        if inner.starts_with('/') {
            out.push(Tok::Close);
            continue;
        }
        let name = inner
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .and_then(|n| n.rsplit(':').next())
            .unwrap_or_default();
        out.push(if inner.ends_with('/') {
            Tok::Empty(name)
        } else {
            Tok::Open(name)
        });
    }
    if !rest.is_empty() {
        out.push(Tok::Text(rest));
    }
    Some(out)
}

/// The name of the first element of `xml`, past any declaration.
fn root_element(xml: &str) -> Option<&str> {
    let mut rest = xml;
    loop {
        let start = rest.find('<')?;
        rest = &rest[start..];
        let end = tag_end(rest)?;
        let inner = &rest[1..end];
        rest = &rest[end + 1..];
        if inner.starts_with(['?', '!', '/']) {
            continue;
        }
        return inner
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .and_then(|n| n.rsplit(':').next());
    }
}

pub(crate) fn prefixes_overlap(rule: &str, repo: &str) -> bool {
    let overlap = |r: &str| r.starts_with(repo) || repo.starts_with(r);
    rule.contains('&') || overlap(rule) || overlap(rule.trim())
}

/// One lifecycle rule, read element by element. Anything the reader does not
/// recognise sets `unknown`, which makes an enabled rule count as deleting.
#[derive(Default)]
struct RuleScan<'a> {
    depth: usize,
    top: &'a str,
    sub: &'a str,
    statuses: Vec<String>,
    prefixes: Vec<String>,
    unknown: bool,
    filter_unknown: bool,
    action: bool,
    exp_marker: bool,
    exp_other: bool,
}

impl<'a> RuleScan<'a> {
    fn enter(&mut self, name: &'a str) {
        self.depth += 1;
        match self.depth {
            1 => {
                self.top = name;
                match name {
                    "Status" => self.statuses.push(String::new()),
                    "Prefix" => self.prefixes.push(String::new()),
                    "Expiration" => {
                        self.exp_marker = false;
                        self.exp_other = false;
                    }
                    "NoncurrentVersionExpiration" => self.action = true,
                    "Filter"
                    | "ID"
                    | "Transition"
                    | "NoncurrentVersionTransition"
                    | "AbortIncompleteMultipartUpload" => {}
                    _ => self.unknown = true,
                }
            }
            2 => {
                self.sub = name;
                match (self.top, name) {
                    ("Filter", "Prefix") => self.prefixes.push(String::new()),
                    ("Filter", _) => self.filter_unknown = true,
                    ("Expiration", "ExpiredObjectDeleteMarker") => self.exp_marker = true,
                    ("Expiration", "Days" | "Date") => self.exp_other = true,
                    ("Expiration", _) => {
                        self.exp_other = true;
                        self.unknown = true;
                    }
                    (
                        "NoncurrentVersionExpiration",
                        "NoncurrentDays" | "NewerNoncurrentVersions",
                    ) => {}
                    ("NoncurrentVersionExpiration", _) => self.unknown = true,
                    ("Status", _) => {
                        if let Some(s) = self.statuses.last_mut() {
                            s.push('?');
                        }
                    }
                    _ => {}
                }
            }
            _ => match self.top {
                "Filter" => self.filter_unknown = true,
                "Expiration" => self.exp_other = true,
                _ => {}
            },
        }
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
        if self.depth == 0 {
            self.finish_top();
        }
    }

    fn finish_top(&mut self) {
        if self.top == "Expiration" {
            self.action |= self.exp_other || !self.exp_marker;
        }
        self.top = "";
    }

    fn text(&mut self, text: &str) {
        let blank = text.trim().is_empty();
        match (self.depth, self.top, self.sub) {
            (0, _, _) if !blank => self.unknown = true,
            (1, "Status", _) => {
                if let Some(s) = self.statuses.last_mut() {
                    s.push_str(text);
                }
            }
            (1, "Prefix", _) | (2, "Filter", "Prefix") => {
                if let Some(p) = self.prefixes.last_mut() {
                    p.push_str(text);
                }
            }
            (1, "Filter", _) if !blank => self.filter_unknown = true,
            (1, "Expiration", _) if !blank => self.exp_other = true,
            _ => {}
        }
    }

    fn deletes(mut self, repo: &str) -> bool {
        if self.depth > 0 {
            self.finish_top();
        }
        let disabled =
            !self.statuses.is_empty() && self.statuses.iter().all(|s| s.trim() == "Disabled");
        !disabled
            && (self.unknown
                || (self.action
                    && (self.filter_unknown
                        || self.prefixes.is_empty()
                        || self.prefixes.iter().any(|p| prefixes_overlap(p, repo)))))
    }
}

/// Whether the lifecycle configuration `xml` has an enabled rule that may
/// remove objects or earlier versions under `repo_prefix`. Closed on refusal:
/// a rule counts as harmless only when every element of it was understood.
pub(crate) fn lifecycle_deletes(xml: &str, repo_prefix: &str) -> bool {
    let Some(toks) = tokens(xml) else {
        return xml.contains("Expiration");
    };
    let repo = match repo_prefix.trim_matches('/') {
        "" => String::new(),
        p => format!("{p}/"),
    };
    let mut i = 0;
    while let Some(tok) = toks.get(i) {
        i += 1;
        match tok {
            Tok::Open("Rule") => {
                let mut scan = RuleScan::default();
                while let Some(tok) = toks.get(i) {
                    i += 1;
                    match tok {
                        Tok::Open("Rule") | Tok::Empty("Rule") => return true,
                        Tok::Open(name) => scan.enter(name),
                        Tok::Empty(name) => {
                            scan.enter(name);
                            scan.leave();
                        }
                        Tok::Close if scan.depth == 0 => break,
                        Tok::Close => scan.leave(),
                        Tok::Text(text) => scan.text(text),
                    }
                }
                if scan.deletes(&repo) {
                    return true;
                }
            }
            Tok::Open("Expiration" | "NoncurrentVersionExpiration")
            | Tok::Empty("Expiration" | "NoncurrentVersionExpiration") => return true,
            _ => {}
        }
    }
    false
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

    fn config(rules: &str) -> String {
        format!("<LifecycleConfiguration>{rules}</LifecycleConfiguration>")
    }

    fn rule(status: &str, filter: &str, action: &str) -> String {
        format!("<Rule><ID>r</ID>{filter}{status}{action}</Rule>")
    }

    const ENABLED: &str = "<Status>Enabled</Status>";
    const NONCURRENT: &str = "<NoncurrentVersionExpiration><NoncurrentDays>1</NoncurrentDays></NoncurrentVersionExpiration>";

    #[test]
    fn lifecycle_rules_that_delete_under_the_repository_are_found() {
        let none = "<Filter><Prefix></Prefix></Filter>";
        let own = "<Filter><Prefix>repo/packs/</Prefix></Filter>";
        let parent = "<Filter><Prefix>re</Prefix></Filter>";
        for filter in [none, own, parent, "", "<Filter/>", "<Prefix>repo/</Prefix>"] {
            for action in [NONCURRENT, "<Expiration><Days>3</Days></Expiration>"] {
                let xml = config(&rule(ENABLED, filter, action));
                assert!(lifecycle_deletes(&xml, "repo"), "{xml}");
            }
        }
        let date = "<Expiration><Date>2030-01-01T00:00:00Z</Date></Expiration>";
        assert!(lifecycle_deletes(&config(&rule(ENABLED, none, date)), ""));
    }

    #[test]
    fn lifecycle_rules_that_cannot_delete_are_cleared() {
        let other = "<Filter><Prefix>elsewhere/</Prefix></Filter>";
        let abort = "<AbortIncompleteMultipartUpload><DaysAfterInitiation>1</DaysAfterInitiation></AbortIncompleteMultipartUpload>";
        let marker =
            "<Expiration><ExpiredObjectDeleteMarker>true</ExpiredObjectDeleteMarker></Expiration>";
        for xml in [
            config(&rule("<Status>Disabled</Status>", "", NONCURRENT)),
            config(&rule(ENABLED, "", abort)),
            config(&rule(ENABLED, "", marker)),
            config(&rule(ENABLED, other, NONCURRENT)),
            config(&rule(
                ENABLED,
                "<Filter><And><Prefix>x</Prefix><Tag/></And></Filter>",
                abort,
            )),
            "<LifecycleConfiguration/>".to_string(),
            String::new(),
        ] {
            assert!(!lifecycle_deletes(&xml, "repo"), "{xml}");
        }
    }

    #[test]
    fn lifecycle_rules_not_fully_understood_count_as_deleting() {
        let days = "<Expiration><Days>1</Days></Expiration>";
        for filter in [
            "<Filter><And><Prefix>elsewhere/</Prefix><Tag><Key>k</Key><Value>v</Value></Tag></And></Filter>",
            "<Filter><Tag><Key>k</Key><Value>v</Value></Tag></Filter>",
            "<Filter><ObjectSizeGreaterThan>5</ObjectSizeGreaterThan></Filter>",
            "<Filter><Prefix>elsewhere/</Prefix><Mystery/></Filter>",
        ] {
            let xml = config(&rule(ENABLED, filter, days));
            assert!(lifecycle_deletes(&xml, "repo"), "{xml}");
        }
        let odd_expiration = "<Expiration><ExpiredObjectDeleteMarker>true</ExpiredObjectDeleteMarker><Mystery>1</Mystery></Expiration>";
        for xml in [
            config(&rule(ENABLED, "", odd_expiration)),
            config(&rule("", "<Filter><Prefix>repo/</Prefix></Filter>", days)),
            config(&rule(
                "<Status>Mystery</Status>",
                "<Filter><Prefix>repo/</Prefix></Filter>",
                NONCURRENT,
            )),
            "<LifecycleConfiguration><Expiration><Days>1</Days></Expiration></LifecycleConfiguration>"
                .to_string(),
            "<Rule><Expiration><Days>1</Days></Expiration>".to_string(),
            "<Rule><Status>Enabled</Status><Expiration><Days>1</Days><!--".to_string(),
            config(&rule(ENABLED, "", "<Mystery/>")),
        ] {
            assert!(lifecycle_deletes(&xml, "repo"), "{xml}");
        }
    }

    #[test]
    fn lifecycle_verdict_separates_refusal_from_a_bad_moment() {
        use LifecycleVerdict::*;
        let code = |c: &str| format!("<Error><Code>{c}</Code></Error>");
        let v = |status, body: &str| lifecycle_verdict(Some((status, body)), "repo");
        assert_eq!(v(404, &code("NoSuchLifecycleConfiguration")), NoRules);
        assert_eq!(v(200, &config(&rule(ENABLED, "", NONCURRENT))), Deletes);
        assert_eq!(v(200, &config("")), NoRules);
        assert_eq!(
            v(200, &format!("<?xml version=\"1.0\"?>{}", config(""))),
            NoRules
        );
        assert_eq!(
            v(
                200,
                &format!("<LifecycleConfiguration>{}", "x".repeat(MAX_BODY))
            ),
            Deletes
        );
        for (status, body) in [
            (404, String::new()),
            (404, code("NoSuchBucket")),
            (404, code("NoSuchKey")),
            (200, String::new()),
            (
                200,
                "<ListBucketResult><Name>repo</Name></ListBucketResult>".into(),
            ),
            (200, "<?xml version=\"1.0\"?><ListBucketResult/>".into()),
            (403, code("AccessDenied")),
            (405, String::new()),
            (501, code("NotImplemented")),
            (400, String::new()),
            (409, String::new()),
        ] {
            assert_eq!(v(status, &body), Unverified, "{status}");
        }
        for (status, body) in [
            (429, String::new()),
            (500, String::new()),
            (502, String::new()),
            (503, code("SlowDown")),
            (504, String::new()),
            (503, code("ServiceUnavailable")),
        ] {
            assert_eq!(v(status, &body), Inconclusive, "{status}");
        }
        assert_eq!(lifecycle_verdict(None, "repo"), Inconclusive);
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        const NS: [&str; 3] = ["", "s3:", "ns1:"];
        const WS: [&str; 4] = ["", " ", "\n  ", "\t"];
        const ATTR: [&str; 3] = [
            "",
            " xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"",
            " a='b'",
        ];
        const COMMENT: [&str; 2] = ["", "<!-- c -->"];

        fn el(style: &(usize, usize, usize, usize), name: &str, inner: &str) -> String {
            let (ns, ws, attr, comment) = *style;
            let (ns, ws, attr, comment) = (NS[ns], WS[ws], ATTR[attr], COMMENT[comment]);
            format!("{ws}<{ns}{name}{attr}>{comment}{inner}{ws}</{ns}{name}>{ws}")
        }

        fn style() -> impl Strategy<Value = (usize, usize, usize, usize)> {
            (0..NS.len(), 0..WS.len(), 0..ATTR.len(), 0..COMMENT.len())
        }

        fn deleting_rule(
            action: usize,
            filter: usize,
            status: usize,
            s: &(usize, usize, usize, usize),
        ) -> String {
            let days = el(s, "Days", "1");
            let action = match action {
                0 => el(s, "Expiration", &days),
                1 => el(s, "Expiration", &el(s, "Date", "2030-01-01T00:00:00Z")),
                2 => el(s, "Expiration", &el(s, "NewerNoncurrentVersions", "2")),
                3 => el(s, "Expiration", &el(s, "Mystery", "1")),
                4 => el(
                    s,
                    "NoncurrentVersionExpiration",
                    &el(s, "NoncurrentDays", "1"),
                ),
                _ => el(
                    s,
                    "Expiration",
                    &format!(
                        "{}{}",
                        el(s, "ExpiredObjectDeleteMarker", "true"),
                        el(s, "Mystery", "x")
                    ),
                ),
            };
            let tag = el(
                s,
                "Tag",
                &format!("{}{}", el(s, "Key", "k"), el(s, "Value", "v")),
            );
            let filter = match filter {
                0 => String::new(),
                1 => el(s, "Filter", ""),
                2 => el(s, "Filter", &el(s, "Prefix", "repo/packs/")),
                3 => el(s, "Filter", &el(s, "Prefix", "")),
                4 => el(
                    s,
                    "Filter",
                    &el(s, "And", &format!("{}{tag}", el(s, "Prefix", "repo/"))),
                ),
                5 => el(s, "Filter", &tag),
                6 => el(s, "Filter", &el(s, "ObjectSizeGreaterThan", "5")),
                7 => el(s, "Filter", &el(s, "ObjectSizeLessThan", "5")),
                _ => el(s, "Filter", &el(s, "Mystery", "x")),
            };
            let status = match status {
                0 => el(s, "Status", "Enabled"),
                1 => String::new(),
                _ => el(s, "Status", "Mystery"),
            };
            el(s, "Rule", &format!("{filter}{status}{action}"))
        }

        fn harmless_rule(s: &(usize, usize, usize, usize)) -> String {
            let abort = el(
                s,
                "AbortIncompleteMultipartUpload",
                &el(s, "DaysAfterInitiation", "1"),
            );
            el(s, "Rule", &format!("{}{abort}", el(s, "Status", "Enabled")))
        }

        proptest! {
            #[test]
            fn lifecycle_deletes_never_panics(xml in ".{0,200}") {
                let _ = lifecycle_deletes(&xml, "repo");
                let _ = lifecycle_deletes(&xml, "");
            }

            #[test]
            fn lifecycle_deletes_never_panics_on_markup(
                xml in "(<[/!?]?[A-Za-z:]{0,12}[ =\"'/-]{0,4}>?|[a-z ]{0,5}){0,40}"
            ) {
                let _ = lifecycle_deletes(&xml, "repo");
            }

            #[test]
            fn lifecycle_deletes_closed_on_refusal(
                action in 0usize..6,
                filter in 0usize..9,
                status in 0usize..3,
                s in style(),
                before in 0usize..3,
                after in 0usize..3,
            ) {
                let harmless = harmless_rule(&s);
                let rules = format!(
                    "{}{}{}",
                    harmless.repeat(before),
                    deleting_rule(action, filter, status, &s),
                    harmless.repeat(after),
                );
                let xml = format!("<?xml version=\"1.0\"?>{}", el(&s, "LifecycleConfiguration", &rules));
                prop_assert!(lifecycle_deletes(&xml, "repo"), "{xml}");
            }
        }
    }
}
