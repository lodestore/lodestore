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
/// transfer.
pub(crate) fn gcs_lifecycle_verdict(
    outcome: Option<(u16, &str)>,
    repo_prefix: &str,
) -> LifecycleVerdict {
    let Some((status, body)) = outcome else {
        return LifecycleVerdict::Inconclusive;
    };
    match status {
        200 => match serde_json::from_str::<Value>(body) {
            Ok(Value::Object(resource)) => {
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

fn gcs_rules_delete(resource: &serde_json::Map<String, Value>, repo_prefix: &str) -> bool {
    let rules = resource
        .get("lifecycle")
        .and_then(|l| l.get("rule"))
        .and_then(Value::as_array);
    rules.is_some_and(|rules| {
        rules.iter().any(|rule| {
            let action = rule.pointer("/action/type").and_then(Value::as_str);
            let prefixes = rule
                .pointer("/condition/matchesPrefix")
                .and_then(Value::as_array);
            action == Some("Delete")
                && prefixes.is_none_or(|prefixes| {
                    prefixes
                        .iter()
                        .any(|p| p.as_str().is_none_or(|p| prefixes_overlap(p, repo_prefix)))
                })
        })
    })
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
}
