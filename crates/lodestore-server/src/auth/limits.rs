use std::collections::{HashMap, VecDeque};
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
pub const DEMO_PER_MINUTE: u32 = 30;
const ENROLL_PER_MINUTE: u32 = 20;
const WINDOW: Duration = Duration::from_secs(60);
const LOGIN_PER_NAME_PER_HOUR: u32 = 60;
const NAME_WINDOW: Duration = Duration::from_secs(3600);
const KNOWN_BUCKETS_PER_NAME: usize = 16;
const STEP_UP_FAILURES_PER_SESSION: u32 = 5;
const PRUNE_ABOVE: usize = 4096;
const MAX_KEY_USERNAME_CHARS: usize = 64;

struct Attempts<K> {
    limit: u32,
    window: Duration,
    seen: Mutex<HashMap<K, (Instant, u32)>>,
}

impl<K: Hash + Eq + Clone> Attempts<K> {
    fn new(limit: u32, window: Duration) -> Self {
        Attempts {
            limit,
            window,
            seen: Mutex::new(HashMap::new()),
        }
    }

    fn hit(&self, key: &K) -> Result<(), u64> {
        let now = Instant::now();
        let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
        if seen.len() > PRUNE_ABOVE {
            seen.retain(|_, (start, _)| now.duration_since(*start) < self.window);
        }
        let entry = seen.entry(key.clone()).or_insert((now, 0));
        if now.duration_since(entry.0) >= self.window {
            *entry = (now, 0);
        }
        if entry.1 >= self.limit {
            let wait = self.window.saturating_sub(now.duration_since(entry.0));
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
    login_name: Arc<Attempts<String>>,
    known: Arc<Mutex<HashMap<String, VecDeque<IpAddr>>>>,
    step_up_failures: Arc<Mutex<HashMap<Vec<u8>, u32>>>,
    setup: Arc<Attempts<IpAddr>>,
    demo: Arc<Attempts<IpAddr>>,
    enroll: Arc<DefaultKeyedRateLimiter<IpAddr>>,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            login_user: Arc::new(Attempts::new(LOGIN_PER_USER_PER_MINUTE, WINDOW)),
            login_client: Arc::new(Attempts::new(LOGIN_PER_CLIENT_PER_MINUTE, WINDOW)),
            login_name: Arc::new(Attempts::new(LOGIN_PER_NAME_PER_HOUR, NAME_WINDOW)),
            known: Arc::default(),
            step_up_failures: Arc::default(),
            setup: Arc::new(Attempts::new(SETUP_PER_MINUTE, WINDOW)),
            demo: Arc::new(Attempts::new(DEMO_PER_MINUTE, WINDOW)),
            enroll: Arc::new(RateLimiter::keyed(Quota::per_minute(
                NonZeroU32::new(ENROLL_PER_MINUTE).expect("a non-zero quota"),
            ))),
        }
    }
}

/// IPv6 clients are keyed by their /56, the block providers commonly hand a customer.
fn bucket(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & (u128::MAX << 72))),
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

    /// Counts a failed-login budget for an existing account across every
    /// address, so rotating clients cannot guess one password forever. An
    /// address bucket that already signed in as this account (or a request
    /// carrying its live session) is neither counted nor blocked, so an
    /// attacker cannot lock the administrator out. Returns whether an attempt
    /// was counted; pass that to `name_succeeded` or `name_released`.
    pub fn check_login_name(
        &self,
        ip: IpAddr,
        user_id: &str,
        has_session: bool,
    ) -> Result<bool, GuardError> {
        if has_session || self.is_known(ip, user_id) {
            return Ok(false);
        }
        self.login_name
            .hit(&user_id.to_owned())
            .map_err(GuardError::RateLimited)?;
        Ok(true)
    }

    fn is_known(&self, ip: IpAddr, user_id: &str) -> bool {
        let known = self.known.lock().unwrap_or_else(|p| p.into_inner());
        known
            .get(user_id)
            .is_some_and(|buckets| buckets.contains(&bucket(ip)))
    }

    /// Gives back what `check_login` and `check_login_name` took when no
    /// password was judged, for instance because the server was busy.
    pub fn login_released(&self, ip: IpAddr, username: &str) {
        let key = user_key(ip, username);
        self.login_user.refund(&key);
        self.login_client.refund(&key.0);
    }

    pub fn name_released(&self, user_id: &str, counted: bool) {
        if counted {
            self.login_name.refund(&user_id.to_owned());
        }
    }

    pub fn login_succeeded(&self, ip: IpAddr, username: &str) {
        self.login_released(ip, username);
    }

    /// Known addresses live in memory only and are forgotten on restart.
    pub fn name_succeeded(&self, ip: IpAddr, user_id: &str, counted: bool) {
        self.name_released(user_id, counted);
        let mut known = self.known.lock().unwrap_or_else(|p| p.into_inner());
        let buckets = known.entry(user_id.to_owned()).or_default();
        let bucket = bucket(ip);
        if !buckets.contains(&bucket) {
            if buckets.len() >= KNOWN_BUCKETS_PER_NAME {
                buckets.pop_front();
            }
            buckets.push_back(bucket);
        }
    }

    /// Counts a wrong password at step-up against the session. Returns true on
    /// the fifth in a row: the caller ends the session, so a stolen cookie
    /// cannot keep guessing the password through step-up. Memory only; a
    /// restart forgets the count.
    pub fn step_up_failed(&self, session: &[u8]) -> bool {
        let mut failures = self
            .step_up_failures
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if failures.len() > PRUNE_ABOVE {
            failures.clear();
        }
        let count = failures.entry(session.to_vec()).or_insert(0);
        *count += 1;
        if *count < STEP_UP_FAILURES_PER_SESSION {
            return false;
        }
        failures.remove(session);
        true
    }

    pub fn step_up_succeeded(&self, session: &[u8]) {
        let mut failures = self
            .step_up_failures
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        failures.remove(session);
    }

    pub fn check_setup(&self, ip: IpAddr) -> Result<(), GuardError> {
        self.setup.hit(&bucket(ip)).map_err(GuardError::RateLimited)
    }

    pub fn setup_succeeded(&self, ip: IpAddr) {
        self.setup_released(ip);
    }

    /// Gives the attempt back when no setup was judged, for instance because
    /// the server was busy.
    pub fn setup_released(&self, ip: IpAddr) {
        self.setup.refund(&bucket(ip));
    }

    pub fn check_demo(&self, ip: IpAddr) -> Result<(), GuardError> {
        self.demo.hit(&bucket(ip)).map_err(GuardError::RateLimited)
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

/// Rate-limited and busy responses carry a Retry-After header.
#[derive(Debug)]
pub enum GuardError {
    Api(ApiError),
    RateLimited(u64),
    Busy(u64),
}

impl<E: Into<ApiError>> From<E> for GuardError {
    fn from(e: E) -> Self {
        GuardError::Api(e.into())
    }
}

fn retry_response(status: StatusCode, code: &str, message: &str, secs: u64) -> Response {
    let mut response = (
        status,
        Json(ErrorBody {
            code: code.to_string(),
            message: message.to_string(),
        }),
    )
        .into_response();
    response
        .headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from(secs));
    response
}

impl IntoResponse for GuardError {
    fn into_response(self) -> Response {
        match self {
            GuardError::Api(e) => e.into_response(),
            GuardError::RateLimited(secs) => retry_response(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "too many requests",
                secs,
            ),
            GuardError::Busy(secs) => retry_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "busy",
                "the server is busy, try again shortly",
                secs,
            ),
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
    fn ipv6_clients_share_a_slash_56() {
        assert_eq!(
            bucket(ip("2001:db8:1:2:aaaa::1")),
            bucket(ip("2001:db8:1:2:bbbb::9"))
        );
        assert_eq!(
            bucket(ip("2001:db8:0:ab::1")),
            bucket(ip("2001:db8:0:ff::2"))
        );
        assert_ne!(
            bucket(ip("2001:db8:0:100::1")),
            bucket(ip("2001:db8:0:ff::1"))
        );
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

    #[test]
    fn login_name_limit_spares_known_buckets() {
        let limits = Limits::default();
        let id = "user-1";
        let known = ip("203.0.113.1");
        assert!(limits.check_login_name(known, id, false).unwrap());
        limits.name_succeeded(known, id, true);
        assert_eq!(limits.login_name.seen.lock().unwrap().len(), 0);

        for n in 0..60u8 {
            let from = ip(&format!("10.0.0.{n}"));
            assert!(limits.check_login_name(from, id, false).unwrap());
        }
        assert!(matches!(
            limits.check_login_name(ip("10.0.1.1"), id, false),
            Err(GuardError::RateLimited(secs)) if (1..=3600).contains(&secs)
        ));
        let before = limits.login_name.seen.lock().unwrap()[id].1;
        assert!(!limits.check_login_name(known, id, false).unwrap());
        assert!(!limits.check_login_name(ip("10.0.1.1"), id, true).unwrap());
        assert_eq!(limits.login_name.seen.lock().unwrap()[id].1, before);

        let v6 = ip("2001:db8:0:ab::1");
        limits.name_succeeded(v6, "user-2", false);
        assert!(
            !limits
                .check_login_name(ip("2001:db8:0:ff::2"), "user-2", false)
                .unwrap()
        );

        for n in 0..40u8 {
            limits.name_succeeded(ip(&format!("192.0.2.{n}")), "user-3", false);
        }
        let known = limits.known.lock().unwrap();
        assert_eq!(known["user-3"].len(), KNOWN_BUCKETS_PER_NAME);
        assert!(!known["user-3"].contains(&ip("192.0.2.0")));
    }

    #[test]
    fn name_attempts_are_refunded_on_success_and_when_released() {
        let limits = Limits::default();
        let id = "user-1";
        for n in 0..100u8 {
            let from = ip(&format!("10.1.0.{n}"));
            let counted = limits.check_login_name(from, id, false).unwrap();
            if n % 2 == 0 {
                limits.name_released(id, counted);
            } else {
                limits.name_succeeded(from, id, counted);
            }
        }
    }

    #[test]
    fn names_that_do_not_exist_never_reach_the_name_limit() {
        let limits = Limits::default();
        for n in 0..10_000u32 {
            let _ = limits.check_login(ip("198.51.100.30"), &format!("ghost{n}"));
            limits.login_released(ip("198.51.100.30"), &format!("ghost{n}"));
        }
        assert!(limits.login_name.seen.lock().unwrap().is_empty());
    }

    #[test]
    fn busy_is_503_with_retry_after() {
        let response = GuardError::Busy(2).into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[RETRY_AFTER], "2");
    }

    #[test]
    fn demo_sign_ins_are_limited_per_client_and_never_refunded() {
        let limits = Limits::default();
        let visitor = ip("198.51.100.20");
        for _ in 0..DEMO_PER_MINUTE {
            limits.check_demo(visitor).unwrap();
        }
        assert!(matches!(
            limits.check_demo(visitor),
            Err(GuardError::RateLimited(secs)) if (1..=60).contains(&secs)
        ));
        limits.check_demo(ip("203.0.113.20")).unwrap();
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
