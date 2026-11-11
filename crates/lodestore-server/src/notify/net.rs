use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use super::DeliveryError;
use crate::config::NotificationsConfig;

const TIMEOUT: Duration = Duration::from_secs(10);
const BODY_LIMIT: usize = 200;

fn always_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.octets()[0] == 0
                || v4.octets()[0] >= 240
                || v4 == Ipv4Addr::new(100, 100, 100, 200)
                || v4 == Ipv4Addr::new(192, 0, 0, 192)
        }
        IpAddr::V6(v6) => {
            v6.is_unicast_link_local()
                || v6.is_unspecified()
                || v6.is_multicast()
                || v6 == Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x254)
        }
    }
}

fn private_range(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || (o[0] == 100 && o[1] & 0xc0 == 64)
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || (o[0] == 198 && o[1] & 0xfe == 18)
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            v6.is_loopback()
                || v6.is_unique_local()
                || s[0] & 0xffc0 == 0xfec0
                || (s[0] == 0x64 && s[1] == 0xff9b && s[2] == 1)
        }
    }
}

fn embedded_v4(v6: Ipv6Addr) -> Vec<Ipv4Addr> {
    let s = v6.segments();
    let low = Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8);
    let high_zero = s[..5].iter().all(|&x| x == 0);
    let mut found = Vec::new();
    if high_zero && s[5] == 0xffff {
        found.push(low);
    }
    if high_zero && s[5] == 0 && u32::from(low) >= 2 {
        found.push(low);
    }
    if s[0] == 0x64 && s[1] == 0xff9b && s[2..6].iter().all(|&x| x == 0) {
        found.push(low);
    }
    if s[0] == 0x64 && s[1] == 0xff9b && s[2] == 1 {
        found.push(low);
    }
    if s[..4].iter().all(|&x| x == 0) && s[4] == 0xffff && s[5] == 0 {
        found.push(low);
    }
    if s[0] == 0x2002 {
        found.push(Ipv4Addr::new(
            (s[1] >> 8) as u8,
            s[1] as u8,
            (s[2] >> 8) as u8,
            s[2] as u8,
        ));
    }
    if s[0] == 0x2001 && s[1] == 0 {
        found.push(Ipv4Addr::new(
            (s[2] >> 8) as u8,
            s[2] as u8,
            (s[3] >> 8) as u8,
            s[3] as u8,
        ));
        found.push(Ipv4Addr::from(!u32::from(low)));
    }
    found
}

fn blocked(ip: IpAddr, allow_private: bool) -> bool {
    always_blocked(ip)
        || (!allow_private && private_range(ip))
        || match ip {
            IpAddr::V4(_) => false,
            IpAddr::V6(v6) => embedded_v4(v6)
                .into_iter()
                .any(|v4| blocked(IpAddr::V4(v4), allow_private)),
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

fn build(config: &NotificationsConfig) -> reqwest::Client {
    let builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT);
    let builder = match &config.proxy {
        Some(proxy) => builder
            .proxy(reqwest::Proxy::all(proxy.expose()).expect("notifications.proxy was validated")),
        None => builder.dns_resolver(Arc::new(Screened {
            allow_private: config.allow_private_targets,
        })),
    };
    builder
        .build()
        .expect("building the notification HTTP client")
}

pub fn client(config: &NotificationsConfig) -> reqwest::Client {
    // shortcut: the client is built from the config on first use, so the proxy is read once per
    // process; the server has one config per process, upgrade when config can reload.
    static STRICT: OnceLock<reqwest::Client> = OnceLock::new();
    static LAN: OnceLock<reqwest::Client> = OnceLock::new();
    let cell = if config.allow_private_targets {
        &LAN
    } else {
        &STRICT
    };
    cell.get_or_init(|| build(config)).clone()
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

pub async fn resolve_allowed(
    host: &str,
    port: u16,
    allow_private: bool,
) -> Result<SocketAddr, DeliveryError> {
    tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| DeliveryError::new(format!("The host {host} cannot be resolved: {e}")))?
        .find(|addr| !blocked(addr.ip(), allow_private))
        .ok_or_else(|| not_allowed(host))
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
    config: &NotificationsConfig,
    url: &str,
    body: Vec<u8>,
    bearer: Option<&str>,
) -> Result<reqwest::Response, DeliveryError> {
    check_target(url, config.allow_private_targets)?;
    let mut request = client(config)
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
    fn blocked_ranges_table() {
        let teredo = |server: Ipv4Addr, client: Ipv4Addr| {
            let (s, c) = (server.octets(), (!u32::from(client)).to_be_bytes());
            IpAddr::V6(Ipv6Addr::new(
                0x2001,
                0,
                u16::from_be_bytes([s[0], s[1]]),
                u16::from_be_bytes([s[2], s[3]]),
                0x1234,
                0x5678,
                u16::from_be_bytes([c[0], c[1]]),
                u16::from_be_bytes([c[2], c[3]]),
            ))
        };
        let public = Ipv4Addr::new(8, 8, 8, 8);
        let table: Vec<(IpAddr, bool, bool)> = [
            ("0.1.2.3", true, true),
            ("192.0.0.8", true, false),
            ("198.18.0.1", true, false),
            ("198.19.255.255", true, false),
            ("198.20.0.1", false, false),
            ("240.0.0.1", true, true),
            ("100.100.100.200", true, true),
            ("192.0.0.192", true, true),
            ("100.64.0.1", true, false),
            ("8.8.8.8", false, false),
            ("127.0.0.1", true, false),
            ("169.254.169.254", true, true),
            ("::ffff:127.0.0.1", true, false),
            ("::127.0.0.1", true, false),
            ("::1", true, false),
            ("::", true, true),
            ("64:ff9b::a9fe:a9fe", true, true),
            ("64:ff9b::808:808", false, false),
            ("64:ff9b:1::1", true, true),
            ("64:ff9b:1::a9fe:a9fe", true, true),
            ("64:ff9b:1::7f00:1", true, false),
            ("64:ff9b:1::808:808", true, false),
            ("::ffff:0:169.254.169.254", true, true),
            ("::ffff:0:127.0.0.1", true, false),
            ("::ffff:0:8.8.8.8", false, false),
            ("::ffff:0:192.0.0.192", true, true),
            ("2002:7f00:1::", true, false),
            ("2002:808:808::", false, false),
            ("fec0::1", true, false),
            ("fd00:ec2::254", true, true),
            ("fd00::1", true, false),
            ("fe80::1", true, true),
            ("ff02::1", true, true),
            ("2606:4700::1111", false, false),
        ]
        .into_iter()
        .map(|(text, strict, lan)| (ip(text), strict, lan))
        .chain([
            (teredo(public, Ipv4Addr::LOCALHOST), true, false),
            (teredo(Ipv4Addr::LOCALHOST, public), true, false),
            (
                teredo(Ipv4Addr::new(169, 254, 169, 254), public),
                true,
                true,
            ),
            (teredo(public, public), false, false),
        ])
        .collect();
        for (addr, strict, lan) in table {
            assert_eq!(blocked(addr, false), strict, "{addr} without the opt-in");
            assert_eq!(blocked(addr, true), lan, "{addr} with the opt-in");
        }
    }

    #[tokio::test]
    async fn resolved_host_is_filtered() {
        let addr = resolve_allowed("localhost", 2525, true).await.unwrap();
        assert!(addr.ip().is_loopback());
        assert_eq!(addr.port(), 2525);
        assert!(resolve_allowed("localhost", 2525, false).await.is_err());
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
