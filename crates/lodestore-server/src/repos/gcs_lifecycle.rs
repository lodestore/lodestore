use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use lodestore_format::store::RepoLocation;
use reqsign_core::{Context, HttpSend, Signer};
use reqsign_file_read_tokio::TokioFileRead;
use reqsign_google::{FileCredentialProvider, RequestSigner, StaticCredentialProvider};
use serde_json::Value;

use crate::repos::object_lock::{
    LifecycleVerdict, MAX_BODY, REQUEST_TIMEOUT, opt, prefixes_overlap,
};

const SCOPE: &str = "https://www.googleapis.com/auth/devstorage.read_only";
const DEFAULT_ENDPOINT: &str = "https://storage.googleapis.com";

/// [`HttpSend`] over reqwest. A body is read only up to [`MAX_BODY`]; errors
/// carry no request or response text, since both can hold credentials.
#[derive(Debug, Clone)]
pub(crate) struct ReqwestSend(reqwest::Client);

impl ReqwestSend {
    pub(crate) fn new() -> Self {
        Self(
            reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_default(),
        )
    }
}

impl HttpSend for ReqwestSend {
    async fn http_send(
        &self,
        req: http::Request<Bytes>,
    ) -> reqsign_core::Result<http::Response<Bytes>> {
        let failed = |what| reqsign_core::Error::unexpected(what);
        let (parts, body) = req.into_parts();
        let mut response = self
            .0
            .request(parts.method, parts.uri.to_string())
            .headers(parts.headers)
            .body(body)
            .send()
            .await
            .map_err(|_| failed("the request did not complete"))?;
        let status = response.status();
        let mut body = Vec::new();
        while body.len() < MAX_BODY {
            match response
                .chunk()
                .await
                .map_err(|_| failed("the response was cut off"))?
            {
                Some(chunk) => body.extend_from_slice(&chunk),
                None => break,
            }
        }
        body.truncate(MAX_BODY);
        http::Response::builder()
            .status(status)
            .body(Bytes::from(body))
            .map_err(|_| failed("the response could not be built"))
    }
}

/// Remembers whether the transport itself failed, so that a refused token
/// (permanent) can be told from an unreachable endpoint (temporary).
#[derive(Debug)]
struct Tracked<H> {
    inner: H,
    failed: AtomicBool,
}

impl<H: HttpSend> HttpSend for Tracked<H> {
    async fn http_send(
        &self,
        req: http::Request<Bytes>,
    ) -> reqsign_core::Result<http::Response<Bytes>> {
        let result = self.inner.http_send(req).await;
        if result.is_err() {
            self.failed.store(true, Ordering::Relaxed);
        }
        result
    }
}

fn bucket_url(location: &RepoLocation) -> String {
    let endpoint = opt(location, "endpoint").unwrap_or(DEFAULT_ENDPOINT);
    let bucket = opt(location, "bucket").unwrap_or_default();
    format!(
        "{}/storage/v1/b/{bucket}?fields=lifecycle",
        endpoint.trim_end_matches('/')
    )
}

fn repo_prefix(location: &RepoLocation) -> &str {
    opt(location, "root").unwrap_or_default().trim_matches('/')
}

enum Auth {
    Bearer(String),
    Signer(Signer<reqsign_google::Credential>),
    Anonymous,
}

fn auth(location: &RepoLocation, ctx: &Context) -> Option<Auth> {
    if let Some(token) = opt(location, "token") {
        return Some(Auth::Bearer(token.to_string()));
    }
    let signer = RequestSigner::new("storage").with_scope(SCOPE);
    if let Some(credential) = opt(location, "credential") {
        let provider = StaticCredentialProvider::from_base64(credential)
            .ok()?
            .with_scope(SCOPE);
        return Some(Auth::Signer(Signer::new(ctx.clone(), provider, signer)));
    }
    if let Some(path) = opt(location, "credential_path") {
        let provider = FileCredentialProvider::new(path).with_scope(SCOPE);
        return Some(Auth::Signer(Signer::new(ctx.clone(), provider, signer)));
    }
    let on = |name| opt(location, name) == Some("true");
    (on("skip_signature") || on("allow_anonymous")).then_some(Auth::Anonymous)
}

/// Reads the bucket's lifecycle rules with the server's own credentials.
pub(crate) async fn check(location: &RepoLocation) -> LifecycleVerdict {
    check_with(location, ReqwestSend::new()).await
}

pub(crate) async fn check_with(location: &RepoLocation, http: impl HttpSend) -> LifecycleVerdict {
    let http = Arc::new(Tracked {
        inner: http,
        failed: AtomicBool::new(false),
    });
    let ctx = Context::new()
        .with_file_read(TokioFileRead)
        .with_http_send(http.clone());
    let Some(auth) = auth(location, &ctx) else {
        return LifecycleVerdict::Unverified;
    };
    let Ok((mut parts, ())) = http::Request::get(bucket_url(location))
        .body(())
        .map(http::Request::into_parts)
    else {
        return LifecycleVerdict::Unverified;
    };
    match auth {
        Auth::Anonymous => {}
        Auth::Bearer(token) => match http::HeaderValue::try_from(format!("Bearer {token}")) {
            Ok(mut value) => {
                value.set_sensitive(true);
                parts.headers.insert(http::header::AUTHORIZATION, value);
            }
            Err(_) => return LifecycleVerdict::Unverified,
        },
        Auth::Signer(signer) => {
            if signer.sign(&mut parts, None).await.is_err() {
                return if http.failed.load(Ordering::Relaxed) {
                    LifecycleVerdict::Inconclusive
                } else {
                    LifecycleVerdict::Unverified
                };
            }
        }
    }
    let prefix = repo_prefix(location);
    match http
        .http_send(http::Request::from_parts(parts, Bytes::new()))
        .await
    {
        Ok(response) => gcs_lifecycle_verdict(
            Some((
                response.status().as_u16(),
                &String::from_utf8_lossy(response.body()),
            )),
            prefix,
        ),
        Err(_) => {
            tracing::warn!("could not read the lifecycle rules of the bucket");
            gcs_lifecycle_verdict(None, prefix)
        }
    }
}

/// Maps one read of the bucket resource to a verdict. `None` is a failed
/// transfer. A body at [`MAX_BODY`] was cut and may hide rules, so it deletes.
pub(crate) fn gcs_lifecycle_verdict(
    outcome: Option<(u16, &str)>,
    repo_prefix: &str,
) -> LifecycleVerdict {
    let Some((status, body)) = outcome else {
        return LifecycleVerdict::Inconclusive;
    };
    match status {
        200 if body.len() >= MAX_BODY => LifecycleVerdict::Deletes,
        200 => match serde_json::from_str::<Value>(body) {
            Ok(resource @ Value::Object(_)) => {
                if gcs_rules_delete(&resource, repo_prefix) {
                    LifecycleVerdict::Deletes
                } else {
                    LifecycleVerdict::NoRules
                }
            }
            _ => LifecycleVerdict::Unverified,
        },
        408 | 429 | 500..=599 => LifecycleVerdict::Inconclusive,
        _ => LifecycleVerdict::Unverified,
    }
}

const CONDITION_KEYS: [&str; 13] = [
    "age",
    "createdBefore",
    "customTimeBefore",
    "daysSinceCustomTime",
    "daysSinceNoncurrentTime",
    "isLive",
    "matchesStorageClass",
    "matchesPrefix",
    "matchesSuffix",
    "noncurrentTimeBefore",
    "numNewerVersions",
    "sizeAboveBytes",
    "sizeBelowBytes",
];

fn only_keys(object: &serde_json::Map<String, Value>, allowed: &[&str]) -> bool {
    object.keys().all(|k| allowed.contains(&k.as_str()))
}

/// Whether the bucket resource holds a lifecycle rule that may remove objects
/// under `repo_prefix`. A rule counts as harmless only when every element of
/// it is understood: whatever is not, deletes.
pub(crate) fn gcs_rules_delete(resource: &Value, repo_prefix: &str) -> bool {
    let Some(resource) = resource.as_object() else {
        return true;
    };
    let Some(lifecycle) = resource.get("lifecycle") else {
        return false;
    };
    let Some(lifecycle) = lifecycle.as_object().filter(|l| only_keys(l, &["rule"])) else {
        return true;
    };
    match lifecycle.get("rule") {
        None => false,
        Some(Value::Array(rules)) => rules.iter().any(|r| rule_deletes(r, repo_prefix)),
        Some(_) => true,
    }
}

fn rule_deletes(rule: &Value, repo_prefix: &str) -> bool {
    let Some(rule) = rule
        .as_object()
        .filter(|r| only_keys(r, &["action", "condition"]))
    else {
        return true;
    };
    let Some(action) = rule
        .get("action")
        .and_then(Value::as_object)
        .filter(|a| only_keys(a, &["type", "storageClass"]))
    else {
        return true;
    };
    let condition = match rule.get("condition") {
        None => None,
        Some(Value::Object(c)) if only_keys(c, &CONDITION_KEYS) => Some(c),
        Some(_) => return true,
    };
    match action.get("type").and_then(Value::as_str) {
        Some("SetStorageClass" | "AbortIncompleteMultipartUpload") => false,
        Some("Delete") => match condition
            .and_then(|c| c.get("matchesPrefix"))
            .and_then(Value::as_array)
        {
            Some(prefixes) if !prefixes.is_empty() => prefixes
                .iter()
                .any(|p| p.as_str().is_none_or(|p| prefixes_overlap(p, repo_prefix))),
            _ => true,
        },
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use lodestore_format::store::probe::{GuaranteeLevel, Weakness};

    use super::*;
    use crate::repos::checks::{apply_lifecycle, timed_out_report};

    #[derive(Debug, Default)]
    pub(super) struct Fake {
        pub(super) requests: Mutex<Vec<http::Request<Bytes>>>,
        pub(super) answers: Mutex<VecDeque<Option<(u16, String)>>>,
    }

    impl Fake {
        pub(super) fn answering(
            answers: impl IntoIterator<Item = Option<(u16, &'static str)>>,
        ) -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::default(),
                answers: Mutex::new(
                    answers
                        .into_iter()
                        .map(|a| a.map(|(s, b)| (s, b.to_string())))
                        .collect(),
                ),
            })
        }
    }

    impl HttpSend for Fake {
        async fn http_send(
            &self,
            req: http::Request<Bytes>,
        ) -> reqsign_core::Result<http::Response<Bytes>> {
            self.requests.lock().unwrap().push(req);
            match self.answers.lock().unwrap().pop_front().flatten() {
                Some((status, body)) => Ok(http::Response::builder()
                    .status(status)
                    .body(Bytes::from(body))
                    .unwrap()),
                None => Err(reqsign_core::Error::unexpected("no answer")),
            }
        }
    }

    fn location(extra: &[(&str, &str)]) -> RepoLocation {
        let mut options = vec![("bucket", "b"), ("root", "/repo")];
        options.extend_from_slice(extra);
        RepoLocation::new("gcs", options)
    }

    #[tokio::test]
    async fn gcs_lifecycle_reads_a_deleting_rule_end_to_end() {
        let fake = Fake::answering([Some((
            200,
            r#"{"lifecycle":{"rule":[{"action":{"type":"Delete"},"condition":{"age":30,"matchesPrefix":["repo/"]}}]}}"#,
        ))]);
        let verdict = check_with(&location(&[("token", "ya29.test")]), fake.clone()).await;
        assert_eq!(verdict, LifecycleVerdict::Deletes);

        let requests = fake.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let uri = requests[0].uri();
        assert_eq!(uri.path(), "/storage/v1/b/b");
        assert_eq!(uri.query(), Some("fields=lifecycle"));
        assert_eq!(requests[0].headers()["authorization"], "Bearer ya29.test");

        let mut report = timed_out_report();
        report.guarantee = GuaranteeLevel::CreateOnlyEnforced;
        apply_lifecycle(&mut report, verdict);
        assert_eq!(report.weaknesses, vec![Weakness::LifecycleDeletes]);
    }

    use LifecycleVerdict::{Deletes, Inconclusive, NoRules, Unverified};
    use serde_json::json;

    fn verdict(status: u16, body: &str) -> LifecycleVerdict {
        gcs_lifecycle_verdict(Some((status, body)), "repo")
    }

    fn lifecycle_with(rule: Value) -> String {
        json!({ "lifecycle": { "rule": [rule] } }).to_string()
    }

    #[test]
    fn gcs_lifecycle_verdict_follows_the_status_table() {
        let huge = format!(r#"{{"pad":"{}"}}"#, "a".repeat(MAX_BODY));
        for (body, expected) in [
            ("{}", NoRules),
            (r#"{"name":"b","location":"EU"}"#, NoRules),
            (r#"{"lifecycle":{}}"#, NoRules),
            (r#"{"lifecycle":{"rule":[]}}"#, NoRules),
            (r#"{"lifecycle":null}"#, Deletes),
            (r#"{"lifecycle":[]}"#, Deletes),
            (r#"{"lifecycle":"x"}"#, Deletes),
            (r#"{"lifecycle":7}"#, Deletes),
            ("not json", Unverified),
            ("[]", Unverified),
            ("null", Unverified),
            ("", Unverified),
            (huge.as_str(), Deletes),
        ] {
            assert_eq!(verdict(200, body), expected, "{body:.60}");
        }
        for status in [201, 204, 301, 400, 401, 403, 404, 405, 409, 412] {
            assert_eq!(verdict(status, "{}"), Unverified, "{status}");
        }
        for status in [408, 429, 500, 501, 502, 503, 504, 599] {
            assert_eq!(verdict(status, "{}"), Inconclusive, "{status}");
        }
        assert_eq!(gcs_lifecycle_verdict(None, "repo"), Inconclusive);
    }

    #[test]
    fn gcs_lifecycle_rules_that_cannot_delete_are_cleared() {
        for rule in [
            json!({"action":{"type":"SetStorageClass","storageClass":"COLDLINE"},"condition":{"age":30}}),
            json!({"action":{"type":"SetStorageClass","storageClass":"ARCHIVE"}}),
            json!({"action":{"type":"AbortIncompleteMultipartUpload"},"condition":{"age":7,"matchesPrefix":["repo/"],"isLive":true}}),
            json!({"action":{"type":"Delete"},"condition":{"age":1,"matchesPrefix":["other/"]}}),
            json!({"action":{"type":"Delete"},"condition":{"matchesPrefix":["a/","b/"],"matchesSuffix":[".tmp"]}}),
        ] {
            assert_eq!(
                verdict(200, &lifecycle_with(rule.clone())),
                NoRules,
                "{rule}"
            );
        }
    }

    #[test]
    fn gcs_lifecycle_rules_not_fully_understood_count_as_deleting() {
        let delete = json!({"type":"Delete"});
        for rule in [
            json!({"action":{"type":"Rename"},"condition":{"age":1}}),
            json!({"action":{"type":"SetStorageClass"},"condition":{"mystery":1}}),
            json!({"action":delete,"condition":{"matchesPrefix":["other/"],"mystery":1}}),
            json!({"action":{"type":"SetStorageClass"},"mystery":1}),
            json!({"action":{"type":"SetStorageClass","mystery":1}}),
            json!({"action":delete}),
            json!({"action":delete,"condition":{"age":1}}),
            json!({"action":delete,"condition":{"matchesPrefix":[]}}),
            json!({"action":delete,"condition":{"matchesPrefix":"other/"}}),
            json!({"action":delete,"condition":{"matchesPrefix":["other/",7]}}),
            json!({"action":delete,"condition":{"matchesPrefix":["re"]}}),
            json!({"action":delete,"condition":{"matchesPrefix":["repo/x"]}}),
            json!({"action":delete,"condition":{"matchesPrefix":[""]}}),
            json!({"action":delete,"condition":[]}),
            json!({"action":"Delete"}),
            json!({"action":[]}),
            json!({"condition":{"age":1}}),
            json!("Delete"),
            json!(null),
        ] {
            assert_eq!(
                verdict(200, &lifecycle_with(rule.clone())),
                Deletes,
                "{rule}"
            );
        }
        for body in [
            r#"{"lifecycle":{"rule":{}}}"#,
            r#"{"lifecycle":{"rule":null}}"#,
            r#"{"lifecycle":{"rule":[],"mystery":1}}"#,
            r#"{"lifecycle":{"mystery":[]}}"#,
        ] {
            assert_eq!(verdict(200, body), Deletes, "{body}");
        }
        let other = lifecycle_with(
            json!({"action":{"type":"Delete"},"condition":{"matchesPrefix":["other/"]}}),
        );
        assert_eq!(
            gcs_lifecycle_verdict(Some((200, &other)), ""),
            Deletes,
            "an empty repository prefix overlaps every rule prefix"
        );
    }

    mod generated {
        use proptest::prelude::*;

        use super::*;

        fn known_conditions() -> impl Strategy<Value = Value> {
            let keys: Vec<&str> = CONDITION_KEYS
                .iter()
                .copied()
                .filter(|k| *k != "matchesPrefix")
                .collect();
            prop::collection::btree_map(prop::sample::select(keys), 0u32..100, 0..4)
                .prop_map(|m| json!(m))
        }

        fn harmless_rule() -> impl Strategy<Value = Value> {
            prop_oneof![
                (
                    prop_oneof![
                        Just("SetStorageClass"),
                        Just("AbortIncompleteMultipartUpload")
                    ],
                    known_conditions()
                )
                    .prop_map(
                        |(kind, condition)| json!({"action":{"type":kind},"condition":condition})
                    ),
                (known_conditions(), "other/[a-z]{0,5}").prop_map(|(mut condition, prefix)| {
                    condition["matchesPrefix"] = json!([prefix]);
                    json!({"action":{"type":"Delete"},"condition":condition})
                }),
            ]
        }

        fn deleting_rule() -> impl Strategy<Value = Value> {
            prop_oneof![
                known_conditions().prop_map(|c| json!({"action":{"type":"Delete"},"condition":c})),
                "repo/[a-z]{0,5}".prop_map(
                    |p| json!({"action":{"type":"Delete"},"condition":{"matchesPrefix":[p]}})
                ),
                "[a-z]{1,6}_x".prop_map(|k| json!({"action":{"type":"SetStorageClass"},k:1})),
                "[a-z]{1,6}_x".prop_map(|k| json!({"action":{"type":"SetStorageClass",k:1}})),
                "[a-z]{1,6}_x"
                    .prop_map(|k| json!({"action":{"type":"SetStorageClass"},"condition":{k:1}})),
                "[A-Za-z]{1,6}_x".prop_map(|t| json!({"action":{"type":t}})),
            ]
        }

        fn deleting_lifecycle() -> impl Strategy<Value = Value> {
            let harmless = || prop::collection::vec(harmless_rule(), 0..4);
            prop_oneof![
                (harmless(), deleting_rule(), harmless()).prop_map(|(before, deleting, after)| {
                    let rules: Vec<Value> =
                        before.into_iter().chain([deleting]).chain(after).collect();
                    json!({"rule": rules})
                }),
                (harmless(), "[a-z]{1,6}_x")
                    .prop_map(|(rules, key)| json!({"rule": rules, key: 1})),
                Just(Value::Null),
                Just(json!([])),
                Just(json!("rule")),
                Just(json!(0)),
            ]
        }

        fn json_tree() -> impl Strategy<Value = Value> {
            let leaf = prop_oneof![
                Just(Value::Null),
                any::<bool>().prop_map(Value::from),
                any::<i64>().prop_map(Value::from),
                "[a-zA-Z/]{0,6}".prop_map(Value::from),
            ];
            leaf.prop_recursive(4, 64, 6, |inner| {
                prop_oneof![
                    prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
                    prop::collection::btree_map("[a-zA-Z]{0,8}", inner, 0..4)
                        .prop_map(|m| Value::Object(m.into_iter().collect())),
                ]
            })
        }

        proptest! {
            #[test]
            fn gcs_lifecycle_closed_on_refusal(lifecycle in deleting_lifecycle(), first in any::<bool>()) {
                let resource = if first {
                    json!({"lifecycle": lifecycle, "name": "b"})
                } else {
                    json!({"name": "b", "lifecycle": lifecycle})
                };
                prop_assert_eq!(verdict(200, &resource.to_string()), Deletes);
            }

            #[test]
            fn gcs_lifecycle_unknown_shapes_never_clear(
                rules in prop::collection::vec(harmless_rule(), 1..4),
                at in any::<prop::sample::Index>(),
                level in 0usize..3,
                key in "[a-z]{1,8}_x",
            ) {
                let mut lifecycle = json!({"rule": rules});
                let i = at.index(lifecycle["rule"].as_array().unwrap().len());
                let target = match level {
                    0 => &mut lifecycle,
                    1 => &mut lifecycle["rule"][i],
                    _ => &mut lifecycle["rule"][i]["action"],
                };
                target[key] = json!(1);
                let body = json!({"lifecycle": lifecycle}).to_string();
                prop_assert_ne!(verdict(200, &body), NoRules);
            }

            #[test]
            fn gcs_lifecycle_never_panics(text in ".{0,200}", tree in json_tree(), status in 0u16..600) {
                let _ = verdict(200, &text);
                let _ = verdict(status, &tree.to_string());
                let _ = verdict(200, &json!({"lifecycle": tree}).to_string());
                let _ = gcs_rules_delete(&tree, "repo");
            }
        }
    }

    fn service_account_json() -> String {
        static KEY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let key = KEY.get_or_init(|| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("sa");
            let status = std::process::Command::new("ssh-keygen")
                .args([
                    "-q", "-t", "rsa", "-b", "2048", "-m", "PKCS8", "-N", "", "-f",
                ])
                .arg(&path)
                .status()
                .expect("ssh-keygen must be installed to run the GCS lifecycle tests");
            assert!(status.success(), "ssh-keygen failed");
            std::fs::read_to_string(&path).unwrap()
        });
        json!({
            "type": "service_account",
            "private_key": key,
            "client_email": "backup@project.iam.gserviceaccount.com",
        })
        .to_string()
    }

    const MINTED: &str = r#"{"access_token":"ya29.minted","expires_in":3600}"#;

    fn assert_minted_token_reads_the_bucket(fake: &Fake) {
        let requests = fake.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method(), http::Method::POST);
        assert_eq!(requests[0].uri(), "https://oauth2.googleapis.com/token");
        assert_eq!(requests[1].uri().path(), "/storage/v1/b/b");
        assert_eq!(requests[1].headers()["authorization"], "Bearer ya29.minted");
    }

    #[tokio::test]
    async fn gcs_lifecycle_service_account_key_exchanges_a_token() {
        let credential = data_encoding::BASE64.encode(service_account_json().as_bytes());
        let fake = Fake::answering([Some((200, MINTED)), Some((200, "{}"))]);
        let verdict = check_with(&location(&[("credential", &credential)]), fake.clone()).await;
        assert_eq!(verdict, NoRules);
        assert_minted_token_reads_the_bucket(&fake);
    }

    #[tokio::test]
    async fn gcs_lifecycle_key_file_exchanges_a_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sa.json");
        std::fs::write(&path, service_account_json()).unwrap();
        let fake = Fake::answering([Some((200, MINTED)), Some((200, "{}"))]);
        let at = path.to_str().unwrap();
        let verdict = check_with(&location(&[("credential_path", at)]), fake.clone()).await;
        assert_eq!(verdict, NoRules);
        assert_minted_token_reads_the_bucket(&fake);
        let missing = dir.path().join("none.json");
        let verdict = check_with(
            &location(&[("credential_path", missing.to_str().unwrap())]),
            Fake::answering([]),
        )
        .await;
        assert_eq!(verdict, Unverified);
    }

    #[tokio::test]
    async fn gcs_lifecycle_token_refusal_is_unverified() {
        let credential = data_encoding::BASE64.encode(service_account_json().as_bytes());
        let at = location(&[("credential", &credential)]);
        for status in [400, 401, 403] {
            let fake = Fake::answering([Some((status, r#"{"error":"invalid_grant"}"#))]);
            assert_eq!(check_with(&at, fake.clone()).await, Unverified, "{status}");
            assert_eq!(fake.requests.lock().unwrap().len(), 1, "{status}");
        }
        assert_eq!(check_with(&at, Fake::answering([None])).await, Inconclusive);
        let broken = location(&[("credential", "!!not base64!!")]);
        assert_eq!(check_with(&broken, Fake::answering([])).await, Unverified);
    }

    #[tokio::test]
    async fn gcs_lifecycle_without_credentials_is_unverified() {
        let fake = Fake::answering([Some((200, "{}"))]);
        assert_eq!(check_with(&location(&[]), fake.clone()).await, Unverified);
        assert!(fake.requests.lock().unwrap().is_empty());
        let fake = Fake::answering([Some((200, "{}"))]);
        let off = location(&[("skip_signature", "false")]);
        assert_eq!(check_with(&off, fake.clone()).await, Unverified);
        assert!(fake.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn gcs_lifecycle_anonymous_sends_no_authorization() {
        for flag in ["skip_signature", "allow_anonymous"] {
            let fake = Fake::answering([Some((200, "{}"))]);
            let verdict = check_with(&location(&[(flag, "true")]), fake.clone()).await;
            assert_eq!(verdict, NoRules, "{flag}");
            let requests = fake.requests.lock().unwrap();
            assert!(
                !requests[0].headers().contains_key("authorization"),
                "{flag}"
            );
        }
    }
}
