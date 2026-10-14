use std::collections::HashMap;
use std::hash::Hash;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header::RETRY_AFTER};
use axum::response::{IntoResponse, Response};
use governor::{DefaultKeyedRateLimiter, Quota, RateLimiter};
use tonic::Status;

use crate::config::{Config, parse_proxies};
use crate::error::{ApiError, ErrorBody};

const LOGIN_PER_USER_PER_MINUTE: u32 = 5;
const LOGIN_PER_CLIENT_PER_MINUTE: u32 = 30;
const SETUP_PER_MINUTE: u32 = 10;
const ENROLL_PER_MINUTE: u32 = 20;
const WINDOW: Duration = Duration::from_secs(60);
const PRUNE_ABOVE: usize = 4096;
const MAX_KEY_USERNAME_CHARS: usize = 64;

struct Attempts<K> {
    limit: u32,
    seen: Mutex<HashMap<K, (Instant, u32)>>,
}

impl<K: Hash + Eq + Clone> Attempts<K> {
    fn new(limit: u32) -> Self {
        Attempts {
            limit,
            seen: Mutex::new(HashMap::new()),
        }
    }

    fn hit(&self, key: &K) -> Result<(), u64> {
        let now = Instant::now();
        let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
        if seen.len() > PRUNE_ABOVE {
            seen.retain(|_, (start, _)| now.duration_since(*start) < WINDOW);
        }
        let entry = seen.entry(key.clone()).or_insert((now, 0));
        if now.duration_since(entry.0) >= WINDOW {
            *entry = (now, 0);
        }
        if entry.1 >= self.limit {
            let wait = WINDOW.saturating_sub(now.duration_since(entry.0));
            return Err(wait.as_secs().max(1));
        }
        entry.1 += 1;
        Ok(())
    }

    fn refund(&self, key: &K) {
        let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(entry) = seen.get_mut(key) {
            entry.1 = entry.1.saturating_sub(1);
            if entry.1 == 0 {
                seen.remove(key);
            }
        }
    }
}

#[derive(Clone)]
pub struct Limits {
    login_user: Arc<Attempts<(IpAddr, String)>>,
    login_client: Arc<Attempts<IpAddr>>,
    setup: Arc<Attempts<IpAddr>>,
    enroll: Arc<DefaultKeyedRateLimiter<IpAddr>>,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            login_user: Arc::new(Attempts::new(LOGIN_PER_USER_PER_MINUTE)),
            login_client: Arc::new(Attempts::new(LOGIN_PER_CLIENT_PER_MINUTE)),
            setup: Arc::new(Attempts::new(SETUP_PER_MINUTE)),
            enroll: Arc::new(RateLimiter::keyed(Quota::per_minute(
                NonZeroU32::new(ENROLL_PER_MINUTE).expect("a non-zero quota"),
            ))),
        }
    }
}

/// IPv6 clients are keyed by their /64, the smallest block a provider hands out.
fn bucket(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & (u128::MAX << 64))),
        v4 => v4,
    }
}

fn user_key(ip: IpAddr, username: &str) -> (IpAddr, String) {
    (
        bucket(ip),
        username.chars().take(MAX_KEY_USERNAME_CHARS).collect(),
    )
}

impl Limits {
    /// Counts a login attempt before the password is checked, so parallel
    /// guesses cannot slip past the limit. A success gives the attempt back,
    /// which keeps the administrator signing in even after typos, and a
    /// client or username that fails repeatedly only locks out itself.
    pub fn check_login(&self, ip: IpAddr, username: &str) -> Result<(), GuardError> {
        let key = user_key(ip, username);
        self.login_user.hit(&key).map_err(GuardError::RateLimited)?;
        if let Err(wait) = self.login_client.hit(&key.0) {
            self.login_user.refund(&key);
            return Err(GuardError::RateLimited(wait));
        }
        Ok(())
    }

    pub fn login_succeeded(&self, ip: IpAddr, username: &str) {
        let key = user_key(ip, username);
        self.login_user.refund(&key);
        self.login_client.refund(&key.0);
    }

    pub fn check_setup(&self, ip: IpAddr) -> Result<(), GuardError> {
        self.setup.hit(&bucket(ip)).map_err(GuardError::RateLimited)
    }

    pub fn setup_succeeded(&self, ip: IpAddr) {
        self.setup.refund(&bucket(ip));
    }

    pub fn check_enroll(&self, ip: IpAddr) -> Result<(), Status> {
        let ip = bucket(ip);
        if self.enroll.len() > PRUNE_ABOVE {
            self.enroll.retain_recent();
        }
        self.enroll
            .check_key(&ip)
            .map_err(|_| Status::resource_exhausted("too many enrollment attempts, retry later"))
    }
}

/// The address a request counts against. Behind a reverse proxy listed in
/// `trusted_proxies` this is the closest address in `X-Forwarded-For` that
/// does not belong to a trusted proxy; otherwise it is the TCP peer.
pub fn client_ip(config: &Config, peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
    let peer = peer.ip();
    let Ok(proxies) = parse_proxies(&config.trusted_proxies) else {
        return peer;
    };
    let trusted = |ip: IpAddr| proxies.iter().any(|net| net.contains(ip));
    if proxies.is_empty() || !trusted(peer) {
        return peer;
    }
    headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .rev()
        .filter_map(|s| s.trim().parse::<IpAddr>().ok())
        .find(|ip| !trusted(*ip))
        .unwrap_or(peer)
}

/// Rate-limited responses carry a Retry-After header.
#[derive(Debug)]
pub enum GuardError {
    Api(ApiError),
    RateLimited(u64),
}

impl<E: Into<ApiError>> From<E> for GuardError {
    fn from(e: E) -> Self {
        GuardError::Api(e.into())
    }
}

impl IntoResponse for GuardError {
    fn into_response(self) -> Response {
        match self {
            GuardError::Api(e) => e.into_response(),
            GuardError::RateLimited(secs) => {
                let mut response = (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(ErrorBody {
                        code: "rate_limited".to_string(),
                        message: "too many requests".to_string(),
                    }),
                )
                    .into_response();
                response
                    .headers_mut()
                    .insert(RETRY_AFTER, HeaderValue::from(secs));
                response
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn ipv6_clients_share_a_slash_64() {
        assert_eq!(
            bucket(ip("2001:db8:1:2:aaaa::1")),
            bucket(ip("2001:db8:1:2:bbbb::9"))
        );
        assert_ne!(bucket(ip("2001:db8:1:2::1")), bucket(ip("2001:db8:1:3::1")));
        assert_eq!(bucket(ip("::ffff:192.0.2.1")), ip("192.0.2.1"));
    }

    #[test]
    fn failures_lock_out_only_their_own_client_and_user() {
        let limits = Limits::default();
        let attacker = ip("198.51.100.7");
        for _ in 0..5 {
            limits.check_login(attacker, "admin").unwrap();
        }
        assert!(matches!(
            limits.check_login(attacker, "admin"),
            Err(GuardError::RateLimited(secs)) if (1..=60).contains(&secs)
        ));
        limits.check_login(ip("203.0.113.9"), "admin").unwrap();
        limits.check_login(attacker, "someone-else").unwrap();
    }

    #[test]
    fn rotating_usernames_hits_the_client_limit_and_success_refunds() {
        let limits = Limits::default();
        let client = ip("198.51.100.8");
        for n in 0..30 {
            limits.check_login(client, &format!("user{n}")).unwrap();
        }
        assert!(limits.check_login(client, "user30").is_err());

        let admin = ip("198.51.100.9");
        for _ in 0..20 {
            limits.check_login(admin, "admin").unwrap();
            limits.login_succeeded(admin, "admin");
        }
    }

    fn headers(forwarded: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", forwarded.parse().unwrap());
        h
    }

    #[test]
    fn forwarded_for_is_trusted_only_from_listed_proxies() {
        let config = Config {
            trusted_proxies: vec!["10.0.0.0/8".into()],
            ..Config::default()
        };
        let peer = |text: &str| SocketAddr::new(ip(text), 40000);
        let forged = headers("1.1.1.1, 198.51.100.5, 10.0.0.2");
        assert_eq!(
            client_ip(&config, peer("10.0.0.3"), &forged),
            ip("198.51.100.5")
        );
        assert_eq!(
            client_ip(&config, peer("192.0.2.50"), &forged),
            ip("192.0.2.50")
        );
        assert_eq!(
            client_ip(&Config::default(), peer("10.0.0.3"), &forged),
            ip("10.0.0.3")
        );
    }
}
