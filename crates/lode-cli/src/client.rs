use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::{Method, StatusCode, Url};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::cli::UsageError;
use crate::profile::Profile;

const MISSING_CONFIG: &str =
    "Set LODESTORE_SERVER and LODESTORE_TOKEN, or add server and token to the profile.";

/// The server answered 401, or 403 for a missing scope; exit code 13.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct TokenRefused(pub String);

/// Any other non-success answer of the server.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
#[allow(dead_code)]
pub struct ApiFailure {
    pub status: u16,
    pub code: String,
    pub message: String,
}

#[derive(serde::Deserialize)]
struct ErrorBody {
    code: String,
    message: String,
}

pub struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn is_loopback(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Percent-encodes one path segment.
pub fn seg(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn refusal(status: StatusCode, body: &[u8]) -> Option<TokenRefused> {
    match status {
        StatusCode::UNAUTHORIZED => Some(TokenRefused(
            "The token was refused: it is unknown, expired or revoked.".into(),
        )),
        StatusCode::FORBIDDEN => {
            let err: ErrorBody = serde_json::from_slice(body).ok()?;
            if err.code != "forbidden" {
                return None;
            }
            let need = err
                .message
                .strip_prefix("The token needs the ")
                .and_then(|rest| rest.strip_suffix('.'));
            Some(TokenRefused(match need {
                Some(need) => format!("The token was refused: it needs the {need}."),
                None => "The token was refused: it may not use this request.".into(),
            }))
        }
        _ => None,
    }
}

pub fn failure(status: StatusCode, body: &[u8]) -> ApiFailure {
    match serde_json::from_slice::<ErrorBody>(body) {
        Ok(err) => ApiFailure {
            status: status.as_u16(),
            code: err.code,
            message: err.message,
        },
        Err(_) => ApiFailure {
            status: status.as_u16(),
            code: String::new(),
            message: format!("The server answered {status}."),
        },
    }
}

impl Client {
    /// `--server`, then `LODESTORE_SERVER`, then the profile's `server`; the
    /// token comes from `LODESTORE_TOKEN`, then the profile's `token`.
    pub fn resolve(server_flag: Option<String>, profile: Option<&Profile>) -> Result<Client> {
        let server = server_flag
            .filter(|v| !v.trim().is_empty())
            .or_else(|| env("LODESTORE_SERVER"))
            .or_else(|| profile.and_then(|p| p.server.clone()));
        let token = env("LODESTORE_TOKEN").or_else(|| profile.and_then(|p| p.token.clone()));
        let (Some(server), Some(token)) = (server, token) else {
            return Err(UsageError(MISSING_CONFIG.into()).into());
        };
        let url = Url::parse(server.trim())
            .map_err(|err| UsageError(format!("invalid server address {server:?}: {err}")))?;
        let host = match (url.scheme(), url.host_str()) {
            ("http" | "https", Some(host)) => host,
            _ => {
                return Err(UsageError(format!(
                    "invalid server address {server:?}: expected http:// or https://"
                ))
                .into());
            }
        };
        if url.scheme() == "http" && !is_loopback(host) {
            eprintln!("warning: The token is sent without TLS to {host}.");
        }
        let http = reqwest::Client::builder()
            .user_agent(concat!("lode/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("cannot start the HTTP client")?;
        Ok(Client {
            http,
            base: url.as_str().trim_end_matches('/').to_owned(),
            token: token.trim().to_owned(),
        })
    }

    async fn exchange(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<serde_json::Value>,
    ) -> Result<(StatusCode, Vec<u8>)> {
        let mut url = Url::parse(&format!("{}{path}", self.base))
            .with_context(|| format!("invalid request path {path}"))?;
        if !query.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(query.iter().map(|(k, v)| (*k, v.as_str())));
        }
        let mut request = self.http.request(method, url).bearer_auth(&self.token);
        if let Some(body) = &body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(reqwest::Error::without_url)
            .with_context(|| format!("cannot reach the server at {}", self.base))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(reqwest::Error::without_url)
            .context("cannot read the server answer")?;
        if let Some(refused) = refusal(status, &bytes) {
            return Err(refused.into());
        }
        Ok((status, bytes.to_vec()))
    }

    async fn checked(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<serde_json::Value>,
    ) -> Result<Vec<u8>> {
        let (status, bytes) = self.exchange(method, path, query, body).await?;
        if status.is_success() {
            Ok(bytes)
        } else {
            Err(failure(status, &bytes).into())
        }
    }

    pub async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T> {
        decode(&self.checked(Method::GET, path, query, None).await?)
    }

    pub async fn post_json<T: DeserializeOwned>(
        &self,
        path: &str,
        body: Option<&impl Serialize>,
    ) -> Result<T> {
        let body = body.map(serde_json::to_value).transpose()?;
        decode(&self.checked(Method::POST, path, &[], body).await?)
    }

    pub async fn put_json<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<T> {
        let body = serde_json::to_value(body)?;
        decode(&self.checked(Method::PUT, path, &[], Some(body)).await?)
    }

    pub async fn delete_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        decode(&self.checked(Method::DELETE, path, &[], None).await?)
    }

    pub async fn delete(&self, path: &str) -> Result<()> {
        self.checked(Method::DELETE, path, &[], None).await?;
        Ok(())
    }

    /// The status and body of a POST whatever the status, for answers that
    /// carry their own error list. A refused token is still an error.
    pub async fn raw_post(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<(StatusCode, Vec<u8>)> {
        let body = serde_json::to_value(body)?;
        self.exchange(Method::POST, path, &[], Some(body)).await
    }
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).context("the server answer is not what this lode expects")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_are_percent_encoded() {
        assert_eq!(seg("etc-and_home.1"), "etc-and_home.1");
        assert_eq!(seg("a/b c"), "a%2Fb%20c");
    }

    #[test]
    fn loopback_hosts() {
        assert!(is_loopback("localhost"));
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("[::1]"));
        assert!(!is_loopback("lode.example.com"));
        assert!(!is_loopback("10.0.0.1"));
    }

    #[test]
    fn forbidden_names_the_scope() {
        let body = br#"{"code":"forbidden","message":"The token needs the jobs:run scope."}"#;
        let refused = refusal(StatusCode::FORBIDDEN, body).unwrap();
        assert_eq!(
            refused.0,
            "The token was refused: it needs the jobs:run scope."
        );
        let body = br#"{"code":"forbidden","message":"The token needs the snapshots:delete scope to set or change retention."}"#;
        assert_eq!(
            refusal(StatusCode::FORBIDDEN, body).unwrap().0,
            "The token was refused: it needs the snapshots:delete scope to set or change retention."
        );
        assert!(refusal(StatusCode::FORBIDDEN, br#"{"code":"csrf","message":"x"}"#).is_none());
        assert!(refusal(StatusCode::NOT_FOUND, b"").is_none());
    }
}
