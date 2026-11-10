pub mod account;
pub mod limits;
pub mod setup;
pub mod tokens;

use std::net::SocketAddr;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use argon2::password_hash::PasswordVerifier;
use argon2::{Argon2, PasswordHasher};
use axum::Json;
use axum::extract::{ConnectInfo, FromRequestParts, State};
use axum::http::StatusCode;
use axum::http::header::{AUTHORIZATION, HOST, ORIGIN};
use axum::http::request::Parts;
use axum::http::uri::Authority;
use axum::http::{HeaderMap, Method, Uri};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use data_encoding::BASE64URL_NOPAD;
use lodestore_format::crypto::Entropy;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use self::limits::GuardError;
use crate::db::now_ms;
use crate::error::{ApiError, ErrorBody};
use crate::state::AppState;

pub const SESSION_COOKIE: &str = "lodestore_session";
pub const CSRF_HEADER: &str = "x-csrf-token";
pub const MIN_PASSWORD_CHARS: usize = 12;
pub const MAX_PASSWORD_BYTES: usize = 1024;
/// A password set under the earlier limit of 1024 characters is at most 4096
/// bytes of UTF-8, so anything longer cannot be a password that signs in.
pub const MAX_SUBMITTED_PASSWORD_BYTES: usize = 4096;
const ARGON_WAIT: Duration = Duration::from_secs(2);
const ARGON_RETRY_AFTER_S: u64 = 2;
pub const MAX_USERNAME_CHARS: usize = 64;
const SESSION_DAYS: i64 = 7;
const SESSION_LIFETIME: Duration = Duration::from_secs(7 * 86_400);
const SESSION_MAX_DAYS: i64 = 30;
const DAY_MS: i64 = 86_400_000;
const SESSION_TOUCH_MS: i64 = 3_600_000;

#[derive(Clone, Default)]
pub struct AuthState {
    pub setup: setup::SetupState,
    pub limits: limits::Limits,
    pub argon: ArgonGate,
}

/// Caps how many Argon2 hashes run at once; each holds about 19 MiB. A request
/// that cannot get a slot within two seconds is answered 503.
#[derive(Clone)]
pub struct ArgonGate(Arc<Semaphore>);

impl Default for ArgonGate {
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        ArgonGate::with_permits((2 * cores).clamp(2, 32))
    }
}

impl ArgonGate {
    pub(crate) fn with_permits(permits: usize) -> Self {
        ArgonGate(Arc::new(Semaphore::new(permits)))
    }

    pub fn available(&self) -> usize {
        self.0.available_permits()
    }

    pub async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, GuardError> {
        self.run_within(ARGON_WAIT, work).await
    }

    /// The permit moves into the blocking closure: a client that hangs up
    /// drops this future, but the hash keeps running and keeps its slot until
    /// it ends, so abandoned requests cannot pile up past the cap.
    async fn run_within<T: Send + 'static>(
        &self,
        wait: Duration,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, GuardError> {
        let permit = tokio::time::timeout(wait, self.0.clone().acquire_owned())
            .await
            .map_err(|_| GuardError::Busy(ARGON_RETRY_AFTER_S))?
            .map_err(|e| ApiError::Internal(e.into()))?;
        tokio::task::spawn_blocking(move || {
            let result = work();
            drop(permit);
            result
        })
        .await
        .map_err(|e| ApiError::Internal(e.into()).into())
    }
}

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub user_id: String,
    pub username: String,
    pub csrf_token: String,
}

#[derive(Deserialize, ToSchema)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Serialize, ToSchema)]
pub struct SessionInfo {
    pub username: String,
    pub csrf_token: String,
}

pub fn check_username(username: &str) -> Result<(), &'static str> {
    match username.chars().count() {
        1..=MAX_USERNAME_CHARS => Ok(()),
        _ => Err("Username must be 1 to 64 characters."),
    }
}

pub fn check_password(password: &str) -> Result<(), &'static str> {
    match password.chars().count() {
        n if n < MIN_PASSWORD_CHARS => Err("At least 12 characters."),
        _ if password.len() > MAX_PASSWORD_BYTES => Err("At most 1024 bytes."),
        _ => Ok(()),
    }
}

static DUMMY_HASH: LazyLock<String> = LazyLock::new(|| {
    hash_password("lodestore-dummy-password").expect("hashing a constant password")
});

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    Ok(Argon2::default()
        .hash_password(password.as_bytes())
        .map_err(|e| anyhow::anyhow!("hashing the password: {e}"))?
        .to_string())
}

fn verify_password(password: &str, hash: &str) -> bool {
    Argon2::default()
        .verify_password(password.as_bytes(), hash)
        .is_ok()
}

fn session_hash(cookie_value: &str) -> Vec<u8> {
    Sha256::digest(cookie_value.as_bytes()).to_vec()
}

pub(crate) fn random_token() -> String {
    BASE64URL_NOPAD.encode(&Entropy::os().bytes::<32>())
}

pub(crate) async fn admin_count(state: &AppState) -> Result<i64, sqlx::Error> {
    sqlx::query("SELECT COUNT(*) AS n FROM admin_users")
        .fetch_one(&state.db.pool)
        .await?
        .try_get("n")
}

pub async fn purge_expired_sessions(state: &AppState) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query("DELETE FROM sessions WHERE expires_at < $1")
        .bind(now_ms())
        .execute(&state.db.pool)
        .await?
        .rows_affected())
}

pub async fn session_janitor(state: AppState, stop: CancellationToken) -> anyhow::Result<()> {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    loop {
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            _ = tick.tick() => {}
        }
        match purge_expired_sessions(&state).await {
            Ok(0) => {}
            Ok(n) => tracing::debug!(removed = n, "expired sessions removed"),
            Err(error) => tracing::warn!(%error, "removing expired sessions failed"),
        }
        match crate::download::reap_stale(&state).await {
            Ok(0) => {}
            Ok(n) => tracing::warn!(reaped = n, "stale downloads marked interrupted"),
            Err(error) => tracing::warn!(%error, "reaping stale downloads failed"),
        }
        match crate::download::purge_unused_links(&state).await {
            Ok(0) => {}
            Ok(n) => tracing::debug!(removed = n, "unused download links removed"),
            Err(error) => tracing::warn!(%error, "removing unused download links failed"),
        }
    }
}

pub async fn bootstrap_admin(state: &AppState) -> anyhow::Result<()> {
    if admin_count(state).await? > 0 {
        return Ok(());
    }
    let Some(password) = &state.config.admin_password else {
        setup::announce(state);
        return Ok(());
    };
    check_username(&state.config.admin_username).map_err(anyhow::Error::msg)?;
    check_password(password.expose()).map_err(anyhow::Error::msg)?;
    let pw = password.expose().to_owned();
    let hash = state
        .auth
        .argon
        .run(move || hash_password(&pw))
        .await
        .map_err(|e| anyhow::anyhow!("hashing the admin password: {e:?}"))??;
    sqlx::query(
        "INSERT INTO admin_users (id, username, password_hash, created_at) VALUES ($1, $2, $3, $4)",
    )
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(&state.config.admin_username)
    .bind(hash)
    .bind(now_ms())
    .execute(&state.db.pool)
    .await?;
    tracing::info!(username = %state.config.admin_username, "admin account created");
    Ok(())
}

pub(crate) fn bearer_secret(headers: &HeaderMap) -> Option<&str> {
    let (scheme, secret) = headers.get(AUTHORIZATION)?.to_str().ok()?.split_once(' ')?;
    scheme.eq_ignore_ascii_case("Bearer").then_some(secret)
}

async fn unauthenticated(state: &AppState) -> ApiError {
    match admin_count(state).await {
        Ok(0) => ApiError::SetupRequired,
        Ok(_) => ApiError::Unauthorized,
        Err(e) => e.into(),
    }
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        if bearer_secret(&parts.headers).is_some() {
            return Err(unauthenticated(state).await);
        }
        let jar = CookieJar::from_headers(&parts.headers);
        let Some(cookie) = jar.get(SESSION_COOKIE) else {
            return Err(unauthenticated(state).await);
        };
        let id_hash = session_hash(cookie.value());
        let row = sqlx::query(
            "SELECT s.user_id, s.csrf_token, s.created_at, s.expires_at, s.last_seen_at, u.username \
             FROM sessions s JOIN admin_users u ON u.id = s.user_id WHERE s.id_hash = $1",
        )
        .bind(&id_hash)
        .fetch_optional(&state.db.pool)
        .await?;
        let Some(row) = row else {
            return Err(unauthenticated(state).await);
        };
        let now = now_ms();
        let expires_at: i64 = row.try_get("expires_at")?;
        if expires_at <= now {
            return Err(ApiError::Unauthorized);
        }
        let user = AuthUser {
            user_id: row.try_get("user_id")?,
            username: row.try_get("username")?,
            csrf_token: row.try_get("csrf_token")?,
        };
        if !matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS) {
            check_csrf(parts, &user, &state.config.public_url)?;
        }
        let last_seen_at: i64 = row.try_get("last_seen_at")?;
        if now - last_seen_at >= SESSION_TOUCH_MS {
            let created_at: i64 = row.try_get("created_at")?;
            let expires = (now + SESSION_DAYS * DAY_MS).min(created_at + SESSION_MAX_DAYS * DAY_MS);
            sqlx::query(
                "UPDATE sessions SET last_seen_at = $1, expires_at = $2 WHERE id_hash = $3",
            )
            .bind(now)
            .bind(expires)
            .bind(&id_hash)
            .execute(&state.db.pool)
            .await?;
        }
        Ok(user)
    }
}

fn check_csrf(parts: &Parts, user: &AuthUser, public_url: &str) -> Result<(), ApiError> {
    let token = parts
        .headers
        .get(CSRF_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or(ApiError::Csrf)?;
    if token != user.csrf_token {
        return Err(ApiError::Csrf);
    }
    if let Some(origin) = parts.headers.get(ORIGIN) {
        let origin = origin.to_str().map_err(|_| ApiError::Csrf)?;
        let host = parts.headers.get(HOST).and_then(|v| v.to_str().ok());
        if !origin_allowed(origin, host, public_url) {
            return Err(ApiError::Csrf);
        }
    }
    Ok(())
}

fn origin_key(scheme: &str, authority: &Authority) -> (String, String, u16) {
    let scheme = scheme.to_ascii_lowercase();
    let port = authority
        .port_u16()
        .unwrap_or(if scheme == "https" { 443 } else { 80 });
    (scheme, authority.host().to_ascii_lowercase(), port)
}

fn uri_key(text: &str) -> Option<(String, String, u16)> {
    let uri = text.parse::<Uri>().ok()?;
    Some(origin_key(uri.scheme_str()?, uri.authority()?))
}

/// An Origin is accepted when it is the configured `public_url` (the address
/// users see, whatever a reverse proxy puts in `Host`) or when it names the
/// host the request was addressed to.
fn origin_allowed(origin: &str, host: Option<&str>, public_url: &str) -> bool {
    let Some(wanted) = uri_key(origin) else {
        return false;
    };
    if uri_key(public_url).as_ref() == Some(&wanted) {
        return true;
    }
    host.and_then(|h| h.parse::<Authority>().ok())
        .is_some_and(|authority| origin_key(&wanted.0, &authority) == wanted)
}

/// Records a new session for `user_id` and returns the cookie value.
/// `None` when the account no longer has `password_hash`, so a sign-in that
/// verified a password which was changed meanwhile gets no session.
pub async fn insert_session(
    state: &AppState,
    user_id: &str,
    password_hash: &str,
) -> Result<Option<String>, ApiError> {
    insert_session_for(state, user_id, password_hash, SESSION_LIFETIME).await
}

pub(crate) async fn insert_session_for(
    state: &AppState,
    user_id: &str,
    password_hash: &str,
    lifetime: Duration,
) -> Result<Option<String>, ApiError> {
    let cookie_value = random_token();
    let now = now_ms();
    let inserted = sqlx::query(
        "INSERT INTO sessions (id_hash, user_id, csrf_token, created_at, expires_at, last_seen_at) \
         SELECT $1, $2, $3, $4, $5, $4 FROM admin_users WHERE id = $2 AND password_hash = $6",
    )
    .bind(session_hash(&cookie_value))
    .bind(user_id)
    .bind(random_token())
    .bind(now)
    .bind(now + i64::try_from(lifetime.as_millis()).unwrap_or(i64::MAX / 2))
    .bind(password_hash)
    .execute(&state.db.pool)
    .await?;
    Ok((inserted.rows_affected() > 0).then_some(cookie_value))
}

pub(crate) async fn start_session(
    state: &AppState,
    jar: CookieJar,
    user_id: &str,
    password_hash: &str,
) -> Result<CookieJar, ApiError> {
    start_session_for(state, jar, user_id, password_hash, SESSION_LIFETIME).await
}

pub(crate) async fn start_session_for(
    state: &AppState,
    jar: CookieJar,
    user_id: &str,
    password_hash: &str,
    lifetime: Duration,
) -> Result<CookieJar, ApiError> {
    let cookie_value = insert_session_for(state, user_id, password_hash, lifetime)
        .await?
        .ok_or(ApiError::Unauthorized)?;
    let cookie = Cookie::build((SESSION_COOKIE, cookie_value))
        .http_only(true)
        .same_site(SameSite::Strict)
        .secure(state.config.public_url.starts_with("https://"))
        .path("/")
        .max_age(time::Duration::seconds(
            i64::try_from(lifetime.as_secs()).unwrap_or(i64::MAX / 2),
        ))
        .build();
    Ok(jar.add(cookie))
}

async fn has_live_session(
    state: &AppState,
    jar: &CookieJar,
    user_id: &str,
) -> Result<bool, sqlx::Error> {
    let Some(cookie) = jar.get(SESSION_COOKIE) else {
        return Ok(false);
    };
    Ok(sqlx::query(
        "SELECT 1 FROM sessions WHERE id_hash = $1 AND user_id = $2 AND expires_at > $3",
    )
    .bind(session_hash(cookie.value()))
    .bind(user_id)
    .bind(now_ms())
    .fetch_optional(&state.db.pool)
    .await?
    .is_some())
}

#[utoipa::path(
    post,
    tag = "Auth",
    summary = "Sign in",
    security(()),
    path = "/api/v1/auth/login",
    request_body = LoginRequest,
    responses(
        (status = 204, description = "Signed in; the session cookie is set"),
        (status = 401, description = "Wrong username or password", body = ErrorBody),
        (status = 409, description = "No administrator exists yet", body = ErrorBody),
        (status = 429, description = "Too many attempts", body = ErrorBody),
    )
)]
async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    jar: CookieJar,
    Json(req): Json<LoginRequest>,
) -> Result<(CookieJar, StatusCode), GuardError> {
    if req.password.len() > MAX_SUBMITTED_PASSWORD_BYTES {
        return Err(ApiError::Unauthorized.into());
    }
    let client = limits::client_ip(&state.config, peer, &headers);
    state.auth.limits.check_login(client, &req.username)?;
    if admin_count(&state).await? == 0 {
        return Err(ApiError::SetupRequired.into());
    }
    let user = sqlx::query("SELECT id, password_hash FROM admin_users WHERE username = $1")
        .bind(&req.username)
        .fetch_optional(&state.db.pool)
        .await?;
    let (user_id, stored) = match user {
        Some(row) => (
            Some(row.try_get::<String, _>("id")?),
            Some(row.try_get::<String, _>("password_hash")?),
        ),
        None => (None, None),
    };
    let mut name_counted = false;
    if let Some(id) = &user_id {
        let has_session = has_live_session(&state, &jar, id).await?;
        name_counted = state
            .auth
            .limits
            .check_login_name(client, id, has_session)?;
    }
    let password = req.password;
    let verified = stored.clone();
    let judged = state
        .auth
        .argon
        .run(move || match verified {
            Some(hash) => verify_password(&password, &hash),
            None => verify_password(&password, &DUMMY_HASH),
        })
        .await;
    let ok = match judged {
        Ok(ok) => ok,
        Err(busy) => {
            state.auth.limits.login_released(client, &req.username);
            if let Some(id) = &user_id {
                state.auth.limits.name_released(id, name_counted);
            }
            return Err(busy);
        }
    };
    let (user_id, hash) = match (ok, user_id, stored) {
        (true, Some(id), Some(hash)) => (id, hash),
        _ => return Err(ApiError::Unauthorized.into()),
    };
    state.auth.limits.login_succeeded(client, &req.username);
    state
        .auth
        .limits
        .name_succeeded(client, &user_id, name_counted);
    let jar = start_session(&state, jar, &user_id, &hash).await?;
    Ok((jar, StatusCode::NO_CONTENT))
}

#[utoipa::path(
    post,
    tag = "Auth",
    summary = "Sign out",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/auth/logout",
    responses(
        (status = 204, description = "The session is ended and the cookie cleared"),
        (status = 401, description = "Not signed in", body = ErrorBody),
    )
)]
async fn logout(
    State(state): State<AppState>,
    _user: AuthUser,
    jar: CookieJar,
) -> Result<(CookieJar, StatusCode), ApiError> {
    if let Some(cookie) = jar.get(SESSION_COOKIE) {
        sqlx::query("DELETE FROM sessions WHERE id_hash = $1")
            .bind(session_hash(cookie.value()))
            .execute(&state.db.pool)
            .await?;
    }
    let jar = jar.remove(Cookie::build((SESSION_COOKIE, "")).path("/"));
    Ok((jar, StatusCode::NO_CONTENT))
}

#[utoipa::path(
    get,
    tag = "Auth",
    summary = "Get the current session and CSRF token",
    path = "/api/v1/auth/session",
    responses(
        (status = 200, description = "The signed-in user", body = SessionInfo),
        (status = 401, description = "Not signed in", body = ErrorBody),
    )
)]
async fn session(user: AuthUser) -> Json<SessionInfo> {
    Json(SessionInfo {
        username: user.username,
        csrf_token: user.csrf_token,
    })
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(login))
        .routes(routes!(logout))
        .routes(routes!(session))
        .merge(setup::routes())
        .merge(account::routes())
        .merge(tokens::routes())
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    const SHORT: Duration = Duration::from_millis(100);

    #[tokio::test]
    async fn argon_gate_returns_busy_when_full() {
        let gate = ArgonGate::with_permits(1);
        let (release, held) = mpsc::channel::<()>();
        let first = tokio::spawn({
            let gate = gate.clone();
            async move { gate.run(move || held.recv().is_ok()).await }
        });
        while gate.available() > 0 {
            tokio::task::yield_now().await;
        }
        let second = gate.run_within(SHORT, || 1).await;
        assert!(matches!(second, Err(GuardError::Busy(ARGON_RETRY_AFTER_S))));
        release.send(()).unwrap();
        assert!(first.await.unwrap().unwrap());
        assert_eq!(gate.run_within(SHORT, || 2).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn argon_gate_holds_permit_until_work_ends() {
        let gate = ArgonGate::with_permits(1);
        let (release, held) = mpsc::channel::<()>();
        let abandoned = tokio::time::timeout(
            Duration::from_millis(50),
            gate.run(move || held.recv().is_ok()),
        )
        .await;
        assert!(abandoned.is_err());
        assert!(matches!(
            gate.run_within(SHORT, || 1).await,
            Err(GuardError::Busy(_))
        ));
        release.send(()).unwrap();
        assert_eq!(
            gate.run_within(Duration::from_secs(5), || 2).await.unwrap(),
            2
        );
    }

    #[test]
    fn password_limits_count_bytes() {
        assert!(check_password(&"a".repeat(1024)).is_ok());
        assert_eq!(
            check_password(&"a".repeat(1025)),
            Err("At most 1024 bytes.")
        );
        assert_eq!(check_password(&"ж".repeat(513)), Err("At most 1024 bytes."));
        assert_eq!(
            check_password(&"ж".repeat(11)),
            Err("At least 12 characters.")
        );
    }

    use super::origin_allowed;

    #[test]
    fn origin_is_checked_against_public_url_and_host() {
        let public = "https://lode.example.com";
        let behind_proxy = Some("127.0.0.1:8080");
        assert!(origin_allowed(
            "https://lode.example.com",
            behind_proxy,
            public
        ));
        assert!(origin_allowed(
            "https://LODE.example.com:443",
            behind_proxy,
            public
        ));
        assert!(!origin_allowed(
            "http://lode.example.com",
            behind_proxy,
            public
        ));
        assert!(!origin_allowed(
            "https://evil.example",
            behind_proxy,
            public
        ));
        assert!(!origin_allowed("null", behind_proxy, public));
        assert!(origin_allowed(
            "http://127.0.0.1:8080",
            behind_proxy,
            public
        ));
        assert!(origin_allowed(
            "http://[::1]:8080",
            Some("[::1]:8080"),
            "http://nas:8080"
        ));
        assert!(!origin_allowed("http://127.0.0.1:9", behind_proxy, public));
        assert!(!origin_allowed("http://127.0.0.1:8080", None, public));
    }
}
