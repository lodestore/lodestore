use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use super::DeliveryError;

const TIMEOUT: Duration = Duration::from_secs(10);
const BODY_LIMIT: usize = 200;

fn blocked(ip: IpAddr, allow_private: bool) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || (!allow_private
                    && (v4.is_loopback()
                        || v4.is_private()
                        || (v4.octets()[0] == 100 && v4.octets()[1] & 0xc0 == 64)))
        }
        IpAddr::V6(v6) => {
            v6.is_unicast_link_local()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (!allow_private && (v6.is_loopback() || v6.is_unique_local()))
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| blocked(IpAddr::V4(v4), allow_private))
        }
    }
}

fn not_allowed(host: &str) -> DeliveryError {
    DeliveryError::new(format!("The address {host} is not allowed."))
}

struct Screened {
    allow_private: bool,
}

impl Resolve for Screened {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_private = self.allow_private;
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let allowed: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|addr| !blocked(addr.ip(), allow_private))
                .collect();
            if allowed.is_empty() {
                return Err(not_allowed(&host).to_string().into());
            }
            Ok(Box::new(allowed.into_iter()) as Addrs)
        })
    }
}

fn build(allow_private: bool) -> reqwest::Client {
    reqwest::Client::builder()
        .dns_resolver(Arc::new(Screened { allow_private }))
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        .build()
        .expect("building the notification HTTP client")
}

pub fn client(allow_private: bool) -> reqwest::Client {
    static STRICT: OnceLock<reqwest::Client> = OnceLock::new();
    static LAN: OnceLock<reqwest::Client> = OnceLock::new();
    let cell = if allow_private { &LAN } else { &STRICT };
    cell.get_or_init(|| build(allow_private)).clone()
}

pub fn check_target(url: &str, allow_private: bool) -> Result<(), DeliveryError> {
    let parsed = reqwest::Url::parse(url).map_err(|_| DeliveryError::new("The URL is invalid."))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| DeliveryError::new("The URL has no host."))?;
    match host.trim_matches(['[', ']']).parse::<IpAddr>() {
        Ok(ip) if blocked(ip, allow_private) => Err(not_allowed(host)),
        _ => Ok(()),
    }
}

/// The SMTP client resolves the host itself, so the addresses are checked
/// just before it connects; a record changed in between is not caught.
pub async fn check_host(host: &str, port: u16, allow_private: bool) -> Result<(), DeliveryError> {
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| DeliveryError::new(format!("The host {host} cannot be resolved: {e}")))?;
    if addrs.into_iter().any(|a| blocked(a.ip(), allow_private)) {
        return Err(not_allowed(host));
    }
    Ok(())
}

pub fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

pub fn transport_error(error: reqwest::Error) -> DeliveryError {
    DeliveryError::new(error_chain(&error.without_url()))
}

pub fn clip(text: &str) -> String {
    text.trim().chars().take(BODY_LIMIT).collect()
}

pub async fn post_json(
    allow_private: bool,
    url: &str,
    body: Vec<u8>,
    bearer: Option<&str>,
) -> Result<reqwest::Response, DeliveryError> {
    check_target(url, allow_private)?;
    let mut request = client(allow_private)
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body);
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    request.send().await.map_err(transport_error)
}

pub async fn expect_success(response: reqwest::Response) -> Result<(), DeliveryError> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    Err(DeliveryError::new(format!("HTTP {}", status.as_u16())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn private_ranges_need_the_opt_in() {
        for text in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "100.64.0.1",
            "::1",
            "fd00::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(blocked(ip(text), false), "{text}");
            assert!(!blocked(ip(text), true), "{text}");
        }
        for text in [
            "169.254.169.254",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "fe80::1",
            "::",
            "ff02::1",
            "::ffff:169.254.169.254",
        ] {
            assert!(blocked(ip(text), false), "{text}");
            assert!(blocked(ip(text), true), "{text}");
        }
        for text in ["93.184.216.34", "2606:4700::1111", "100.128.0.1"] {
            assert!(!blocked(ip(text), false), "{text}");
        }
    }

    #[test]
    fn targets_are_checked_by_literal() {
        assert!(check_target("http://127.0.0.1:8080/x", false).is_err());
        assert!(check_target("http://[::1]/x", false).is_err());
        assert!(check_target("http://127.0.0.1:8080/x", true).is_ok());
        assert!(check_target("http://169.254.169.254/x", true).is_err());
        assert!(check_target("https://hooks.example.com/x", false).is_ok());
    }
}
