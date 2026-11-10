use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use clap::Args;
use serde::Deserialize;

use crate::agent_update::api::ROLLBACK_AFTER_SECS;
use crate::alerts::model::AlertType;

pub const DEFAULT_CONFIG_FILE: &str = "/etc/lodestore/server.yaml";

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Secret(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GrpcConfig {
    pub listen: SocketAddr,
    pub public_address: String,
    pub extra_sans: Vec<String>,
    pub tls_cert_file: Option<PathBuf>,
    pub tls_key_file: Option<PathBuf>,
    pub cert_reload_secs: u64,
}

pub const MAX_JOIN_TOKEN_TTL_SECS: i64 = 7 * 86_400;
pub const MAX_AGENT_CERT_LIFETIME_SECS: i64 = 825 * 86_400;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub listen: SocketAddr,
    pub public_url: String,
    pub trusted_proxies: Vec<String>,
    pub grpc: GrpcConfig,
    pub data_dir: PathBuf,
    pub database_url: String,
    pub unlock_key: Option<Secret>,
    pub unlock_key_file: Option<PathBuf>,
    pub admin_username: String,
    pub admin_password: Option<Secret>,
    pub agent_dir: PathBuf,
    pub join_token_ttl_secs: i64,
    pub agent_cert_lifetime_secs: i64,
    pub schedule_tick_secs: u64,
    pub metrics_listen: Option<SocketAddr>,
    pub demo_mode: bool,
    #[serde(skip)]
    pub demo: DemoConfig,
    #[serde(skip)]
    pub find: FindConfig,
    #[serde(skip)]
    pub agent_updates: AgentUpdateConfig,
    #[serde(skip)]
    pub grpc_public_address_defaulted: bool,
    #[serde(skip)]
    pub notifications: NotificationsConfig,
}

#[derive(Debug, Clone)]
pub struct DemoConfig {
    pub download_timeout: Duration,
    pub max_downloads: i64,
}

impl Default for DemoConfig {
    fn default() -> Self {
        DemoConfig {
            download_timeout: Duration::from_secs(120),
            max_downloads: 2,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FindConfig {
    pub max_matches: u64,
    pub timeout: Duration,
    pub cache_max_bytes: u64,
}

impl Default for FindConfig {
    fn default() -> Self {
        FindConfig {
            max_matches: 1000,
            timeout: Duration::from_secs(600),
            cache_max_bytes: 1 << 30,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentUpdateConfig {
    pub canary: u32,
    pub wave: u32,
    pub pause_after: u32,
    pub health_timeout: Duration,
}

impl Default for AgentUpdateConfig {
    fn default() -> Self {
        AgentUpdateConfig {
            canary: 2,
            wave: 10,
            pause_after: 1,
            health_timeout: Duration::from_secs(150),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct NotificationsConfig {
    pub channels: Vec<ConfigChannel>,
    pub allow_private_targets: bool,
}

#[derive(Debug, Clone)]
pub struct ConfigChannel {
    pub name: String,
    pub enabled: bool,
    pub alerts: Vec<AlertType>,
    pub kind: ChannelSpec,
}

#[derive(Debug, Clone)]
pub enum ChannelSpec {
    Telegram {
        bot_token: Secret,
        chat_id: String,
    },
    Webhook {
        url: String,
        secret: Option<Secret>,
    },
    Slack {
        webhook_url: Secret,
    },
    Email {
        smtp_host: String,
        port: u16,
        security: SmtpSecurity,
        username: Option<String>,
        password: Option<Secret>,
        from: String,
        to: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SmtpSecurity {
    Starttls,
    Tls,
}

fn smtp_port() -> u16 {
    587
}

const ALL_ALERT_TYPES: [AlertType; 10] = [
    AlertType::BackupFailed,
    AlertType::NoRecentBackup,
    AlertType::AgentOffline,
    AlertType::CheckFailed,
    AlertType::CertificateExpiring,
    AlertType::AgentUpdateFailed,
    AlertType::SnapshotQuarantined,
    AlertType::RepositoryUnavailable,
    AlertType::AgentSessionConflict,
    AlertType::AgentCertificateRejected,
];

fn nodename() -> String {
    rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned()
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 8080))
}

fn default_grpc_listen() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 8443))
}

fn default_database_url(data_dir: &std::path::Path) -> String {
    format!("sqlite://{}/lodestore.db?mode=rwc", data_dir.display())
}

impl Default for GrpcConfig {
    fn default() -> Self {
        GrpcConfig {
            listen: default_grpc_listen(),
            public_address: format!("{}:{}", nodename(), default_grpc_listen().port()),
            extra_sans: Vec::new(),
            tls_cert_file: None,
            tls_key_file: None,
            cert_reload_secs: 60,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        let data_dir = PathBuf::from("./data");
        Config {
            listen: default_listen(),
            public_url: format!("http://{}:{}", nodename(), default_listen().port()),
            trusted_proxies: Vec::new(),
            grpc: GrpcConfig::default(),
            database_url: default_database_url(&data_dir),
            data_dir,
            unlock_key: None,
            unlock_key_file: None,
            admin_username: "admin".to_string(),
            admin_password: None,
            agent_dir: PathBuf::from("/usr/share/lodestore/agents"),
            join_token_ttl_secs: 3600,
            agent_cert_lifetime_secs: 7_776_000,
            schedule_tick_secs: crate::schedule::SCHEDULE_TICK.as_secs(),
            metrics_listen: None,
            demo_mode: false,
            demo: DemoConfig::default(),
            find: FindConfig::default(),
            agent_updates: AgentUpdateConfig::default(),
            grpc_public_address_defaulted: true,
            notifications: NotificationsConfig::default(),
        }
    }
}

#[derive(Args, Debug, Default, Clone)]
pub struct ConfigArgs {
    #[arg(long, env = "LODESTORE_CONFIG")]
    pub config: Option<PathBuf>,
    #[arg(long, env = "LODESTORE_LISTEN")]
    pub listen: Option<SocketAddr>,
    #[arg(long, env = "LODESTORE_PUBLIC_URL")]
    pub public_url: Option<String>,
    #[arg(long, env = "LODESTORE_TRUSTED_PROXIES", value_delimiter = ',')]
    pub trusted_proxies: Vec<String>,
    #[arg(long, env = "LODESTORE_GRPC_LISTEN")]
    pub grpc_listen: Option<SocketAddr>,
    #[arg(long, env = "LODESTORE_GRPC_PUBLIC_ADDRESS")]
    pub grpc_public_address: Option<String>,
    #[arg(long, env = "LODESTORE_GRPC_EXTRA_SANS", value_delimiter = ',')]
    pub grpc_extra_sans: Vec<String>,
    #[arg(long, env = "LODESTORE_GRPC_TLS_CERT_FILE")]
    pub grpc_tls_cert_file: Option<PathBuf>,
    #[arg(long, env = "LODESTORE_GRPC_TLS_KEY_FILE")]
    pub grpc_tls_key_file: Option<PathBuf>,
    #[arg(long, env = "LODESTORE_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
    #[arg(long, env = "LODESTORE_DATABASE_URL", hide_env_values = true)]
    pub database_url: Option<String>,
    #[arg(long, env = "LODESTORE_DATABASE_URL_FILE")]
    pub database_url_file: Option<PathBuf>,
    #[arg(long, env = "LODESTORE_UNLOCK_KEY", hide_env_values = true)]
    pub unlock_key: Option<String>,
    #[arg(long, env = "LODESTORE_UNLOCK_KEY_FILE")]
    pub unlock_key_file: Option<PathBuf>,
    #[arg(long, env = "LODESTORE_ADMIN_USERNAME")]
    pub admin_username: Option<String>,
    #[arg(long, env = "LODESTORE_ADMIN_PASSWORD", hide_env_values = true)]
    pub admin_password: Option<String>,
    #[arg(long, env = "LODESTORE_ADMIN_PASSWORD_FILE")]
    pub admin_password_file: Option<PathBuf>,
    #[arg(long, env = "LODESTORE_AGENT_DIR")]
    pub agent_dir: Option<PathBuf>,
    #[arg(long, env = "LODESTORE_JOIN_TOKEN_TTL")]
    pub join_token_ttl_secs: Option<i64>,
    #[arg(long, env = "LODESTORE_AGENT_CERT_LIFETIME", hide = true)]
    pub agent_cert_lifetime_secs: Option<i64>,
    #[arg(long, env = "LODESTORE_METRICS_LISTEN")]
    pub metrics_listen: Option<SocketAddr>,
    #[arg(long, env = "LODESTORE_DEMO_MODE", value_parser = clap::builder::BoolishValueParser::new())]
    pub demo_mode: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileGrpc {
    listen: Option<SocketAddr>,
    public_address: Option<String>,
    extra_sans: Option<Vec<String>>,
    tls_cert_file: Option<PathBuf>,
    tls_key_file: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    listen: Option<SocketAddr>,
    public_url: Option<String>,
    trusted_proxies: Option<Vec<String>>,
    grpc: FileGrpc,
    data_dir: Option<PathBuf>,
    database_url: Option<String>,
    admin_username: Option<String>,
    agent_dir: Option<PathBuf>,
    join_token_ttl_secs: Option<i64>,
    agent_cert_lifetime_secs: Option<i64>,
    metrics_listen: Option<SocketAddr>,
    demo_mode: Option<bool>,
    notifications: FileNotifications,
    find: FileFind,
    agent_updates: FileAgentUpdates,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileAgentUpdates {
    canary: Option<i64>,
    wave: Option<i64>,
    pause_after: Option<i64>,
    health_timeout_secs: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileFind {
    max_matches: Option<i64>,
    timeout_secs: Option<i64>,
    cache_max_mb: Option<i64>,
}

fn positive_in(section: &str, key: &str, value: i64) -> anyhow::Result<u64> {
    u64::try_from(value)
        .ok()
        .filter(|v| *v > 0)
        .with_context(|| format!("{section}.{key} must be greater than zero"))
}

fn positive(key: &str, value: i64) -> anyhow::Result<u64> {
    positive_in("find", key, value)
}

fn resolve_agent_updates(file: FileAgentUpdates) -> anyhow::Result<AgentUpdateConfig> {
    let count = |key: &str, value: i64| -> anyhow::Result<u32> {
        u32::try_from(positive_in("agent_updates", key, value)?)
            .with_context(|| format!("agent_updates.{key} is too large"))
    };
    let mut updates = AgentUpdateConfig::default();
    if let Some(v) = file.canary {
        updates.canary = count("canary", v)?;
    }
    if let Some(v) = file.wave {
        updates.wave = count("wave", v)?;
    }
    if let Some(v) = file.pause_after {
        updates.pause_after = count("pause_after", v)?;
    }
    if let Some(v) = file.health_timeout_secs {
        let secs = positive_in("agent_updates", "health_timeout_secs", v)?;
        anyhow::ensure!(
            secs >= ROLLBACK_AFTER_SECS,
            "agent_updates.health_timeout_secs must be at least {ROLLBACK_AFTER_SECS}, \
             the time an agent has before it rolls itself back"
        );
        updates.health_timeout = Duration::from_secs(secs);
    }
    Ok(updates)
}

fn resolve_find(file: FileFind) -> anyhow::Result<FindConfig> {
    let mut find = FindConfig::default();
    if let Some(v) = file.max_matches {
        find.max_matches = positive("max_matches", v)?;
    }
    if let Some(v) = file.timeout_secs {
        find.timeout = Duration::from_secs(positive("timeout_secs", v)?);
    }
    if let Some(v) = file.cache_max_mb {
        find.cache_max_bytes = positive("cache_max_mb", v)?
            .checked_mul(1 << 20)
            .context("find.cache_max_mb is too large")?;
    }
    Ok(find)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileNotifications {
    channels: Vec<FileChannel>,
    allow_private_targets: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum FileChannel {
    Telegram {
        name: String,
        #[serde(default = "yes")]
        enabled: bool,
        alerts: Option<Vec<AlertType>>,
        bot_token_file: PathBuf,
        chat_id: String,
    },
    Webhook {
        name: String,
        #[serde(default = "yes")]
        enabled: bool,
        alerts: Option<Vec<AlertType>>,
        url: String,
        secret_file: Option<PathBuf>,
    },
    Slack {
        name: String,
        #[serde(default = "yes")]
        enabled: bool,
        alerts: Option<Vec<AlertType>>,
        webhook_url_file: PathBuf,
    },
    Email {
        name: String,
        #[serde(default = "yes")]
        enabled: bool,
        alerts: Option<Vec<AlertType>>,
        smtp_host: String,
        #[serde(default = "smtp_port")]
        port: u16,
        #[serde(default = "default_security")]
        security: SmtpSecurity,
        username: Option<String>,
        password_file: Option<PathBuf>,
        from: String,
        #[serde(default)]
        to: Vec<String>,
    },
}

fn default_security() -> SmtpSecurity {
    SmtpSecurity::Starttls
}

fn read_secret_file(channel: &str, field: &str, path: &Path) -> anyhow::Result<Secret> {
    let text = std::fs::read_to_string(path).with_context(|| {
        format!(
            "notification channel {channel}: reading {field} {}",
            path.display()
        )
    })?;
    let text = text.trim_end();
    anyhow::ensure!(
        !text.is_empty(),
        "notification channel {channel}: {field} {} is empty",
        path.display()
    );
    Ok(Secret::new(text))
}

fn resolve_channels(file: FileNotifications) -> anyhow::Result<Vec<ConfigChannel>> {
    file.channels
        .into_iter()
        .map(|channel| {
            Ok(match channel {
                FileChannel::Telegram {
                    name,
                    enabled,
                    alerts,
                    bot_token_file,
                    chat_id,
                } => ConfigChannel {
                    kind: ChannelSpec::Telegram {
                        bot_token: read_secret_file(&name, "bot_token_file", &bot_token_file)?,
                        chat_id,
                    },
                    name,
                    enabled,
                    alerts: alerts.unwrap_or_else(|| ALL_ALERT_TYPES.to_vec()),
                },
                FileChannel::Webhook {
                    name,
                    enabled,
                    alerts,
                    url,
                    secret_file,
                } => ConfigChannel {
                    kind: ChannelSpec::Webhook {
                        url,
                        secret: secret_file
                            .map(|path| read_secret_file(&name, "secret_file", &path))
                            .transpose()?,
                    },
                    name,
                    enabled,
                    alerts: alerts.unwrap_or_else(|| ALL_ALERT_TYPES.to_vec()),
                },
                FileChannel::Slack {
                    name,
                    enabled,
                    alerts,
                    webhook_url_file,
                } => ConfigChannel {
                    kind: ChannelSpec::Slack {
                        webhook_url: read_secret_file(
                            &name,
                            "webhook_url_file",
                            &webhook_url_file,
                        )?,
                    },
                    name,
                    enabled,
                    alerts: alerts.unwrap_or_else(|| ALL_ALERT_TYPES.to_vec()),
                },
                FileChannel::Email {
                    name,
                    enabled,
                    alerts,
                    smtp_host,
                    port,
                    security,
                    username,
                    password_file,
                    from,
                    to,
                } => ConfigChannel {
                    kind: ChannelSpec::Email {
                        smtp_host,
                        port,
                        security,
                        username,
                        password: password_file
                            .map(|path| read_secret_file(&name, "password_file", &path))
                            .transpose()?,
                        from,
                        to,
                    },
                    name,
                    enabled,
                    alerts: alerts.unwrap_or_else(|| ALL_ALERT_TYPES.to_vec()),
                },
            })
        })
        .collect()
}

fn load_file(explicit: Option<&Path>) -> anyhow::Result<FileConfig> {
    let path = match explicit {
        Some(path) => path,
        None if Path::new(DEFAULT_CONFIG_FILE).is_file() => Path::new(DEFAULT_CONFIG_FILE),
        None => return Ok(FileConfig::default()),
    };
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the config file {}", path.display()))?;
    yaml_serde::from_str(&text)
        .with_context(|| format!("parsing the config file {}", path.display()))
}

fn secret_value(
    name: &str,
    value: Option<String>,
    file: Option<&Path>,
) -> anyhow::Result<Option<String>> {
    match (value, file) {
        (Some(_), Some(_)) => anyhow::bail!("{name} and {name}_FILE are both set; use one of them"),
        (Some(value), None) => Ok(Some(value)),
        (None, Some(path)) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {name}_FILE {}", path.display()))?;
            Ok(Some(text.trim_end().to_string()))
        }
        (None, None) => Ok(None),
    }
}

impl Config {
    pub fn from_args(args: ConfigArgs) -> anyhow::Result<Config> {
        let file = load_file(args.config.as_deref())?;
        let mut c = Config::default();
        if let Some(v) = args.listen.or(file.listen) {
            c.listen = v;
        }
        if let Some(v) = args.grpc_listen.or(file.grpc.listen) {
            c.grpc.listen = v;
        }
        if let Some(v) = args.data_dir.or(file.data_dir) {
            c.data_dir = v;
        }
        c.public_url = args
            .public_url
            .or(file.public_url)
            .unwrap_or_else(|| format!("http://{}:{}", nodename(), c.listen.port()))
            .trim_end_matches('/')
            .to_string();
        c.trusted_proxies = if args.trusted_proxies.is_empty() {
            file.trusted_proxies.unwrap_or_default()
        } else {
            args.trusted_proxies
        };
        match args.grpc_public_address.or(file.grpc.public_address) {
            Some(v) => {
                c.grpc.public_address = v;
                c.grpc_public_address_defaulted = false;
            }
            None => {
                c.grpc.public_address = format!("{}:{}", nodename(), c.grpc.listen.port());
            }
        }
        c.grpc.extra_sans = if args.grpc_extra_sans.is_empty() {
            file.grpc.extra_sans.unwrap_or_default()
        } else {
            args.grpc_extra_sans
        };
        c.grpc.tls_cert_file = args.grpc_tls_cert_file.or(file.grpc.tls_cert_file);
        c.grpc.tls_key_file = args.grpc_tls_key_file.or(file.grpc.tls_key_file);
        c.database_url = secret_value(
            "LODESTORE_DATABASE_URL",
            args.database_url,
            args.database_url_file.as_deref(),
        )?
        .or(file.database_url)
        .unwrap_or_else(|| default_database_url(&c.data_dir));
        anyhow::ensure!(
            args.unlock_key.is_none() || args.unlock_key_file.is_none(),
            "LODESTORE_UNLOCK_KEY and LODESTORE_UNLOCK_KEY_FILE are both set; use one of them"
        );
        c.unlock_key = args.unlock_key.map(Secret::new);
        c.unlock_key_file = args.unlock_key_file;
        if let Some(v) = args.admin_username.or(file.admin_username) {
            c.admin_username = v;
        }
        c.admin_password = secret_value(
            "LODESTORE_ADMIN_PASSWORD",
            args.admin_password,
            args.admin_password_file.as_deref(),
        )?
        .map(Secret::new);
        if let Some(v) = args.agent_dir.or(file.agent_dir) {
            c.agent_dir = v;
        }
        if let Some(v) = args.join_token_ttl_secs.or(file.join_token_ttl_secs) {
            c.join_token_ttl_secs = v;
        }
        if let Some(v) = args
            .agent_cert_lifetime_secs
            .or(file.agent_cert_lifetime_secs)
        {
            c.agent_cert_lifetime_secs = v;
        }
        c.metrics_listen = args.metrics_listen.or(file.metrics_listen);
        c.demo_mode = args.demo_mode.or(file.demo_mode).unwrap_or(false);
        c.find = resolve_find(file.find)?;
        c.agent_updates = resolve_agent_updates(file.agent_updates)?;
        c.notifications.allow_private_targets = file.notifications.allow_private_targets;
        c.notifications.channels = resolve_channels(file.notifications)?;
        c.validate().context("invalid configuration")?;
        Ok(c)
    }

    fn validate(&self) -> anyhow::Result<()> {
        validate_public_url(&self.public_url)?;
        parse_proxies(&self.trusted_proxies)?;
        if let Err(message) = crate::auth::check_username(&self.admin_username) {
            anyhow::bail!("admin username: {message}");
        }
        if let Some(password) = &self.admin_password
            && let Err(message) = crate::auth::check_password(password.expose())
        {
            anyhow::bail!("admin password: {message}");
        }
        anyhow::ensure!(
            (1..=MAX_JOIN_TOKEN_TTL_SECS).contains(&self.join_token_ttl_secs),
            "join token TTL must be between 1 and {MAX_JOIN_TOKEN_TTL_SECS} seconds"
        );
        anyhow::ensure!(
            (1..=MAX_AGENT_CERT_LIFETIME_SECS).contains(&self.agent_cert_lifetime_secs),
            "agent certificate lifetime must be between 1 and {MAX_AGENT_CERT_LIFETIME_SECS} seconds"
        );
        anyhow::ensure!(
            self.grpc.tls_cert_file.is_some() == self.grpc.tls_key_file.is_some(),
            "grpc tls_cert_file and tls_key_file must be set together"
        );
        validate_channels(&self.notifications.channels)
    }
}

fn validate_channels(channels: &[ConfigChannel]) -> anyhow::Result<()> {
    let mut seen = std::collections::HashSet::new();
    for (i, channel) in channels.iter().enumerate() {
        let name = channel.name.as_str();
        anyhow::ensure!(
            !name.trim().is_empty(),
            "notification channel #{}: name must not be empty",
            i + 1
        );
        anyhow::ensure!(
            seen.insert(name),
            "notification channel name {name} is used twice"
        );
        anyhow::ensure!(
            !channel.alerts.is_empty(),
            "notification channel {name}: alerts must not be empty"
        );
        match &channel.kind {
            ChannelSpec::Webhook { url, .. } => anyhow::ensure!(
                url.starts_with("http://") || url.starts_with("https://"),
                "notification channel {name}: url must start with http:// or https://"
            ),
            ChannelSpec::Telegram { chat_id, .. } => anyhow::ensure!(
                !chat_id.trim().is_empty(),
                "notification channel {name}: chat_id must not be empty"
            ),
            ChannelSpec::Slack { webhook_url } => anyhow::ensure!(
                webhook_url.expose().starts_with("https://"),
                "notification channel {name}: webhook URL must start with https://"
            ),
            ChannelSpec::Email {
                smtp_host,
                from,
                to,
                ..
            } => {
                anyhow::ensure!(
                    !smtp_host.trim().is_empty(),
                    "notification channel {name}: smtp_host must not be empty"
                );
                from.parse::<lettre::Address>().map_err(|e| {
                    anyhow::anyhow!(
                        "notification channel {name}: from {from:?} is not an email address: {e}"
                    )
                })?;
                for address in to {
                    address.parse::<lettre::Address>().map_err(|e| {
                        anyhow::anyhow!("notification channel {name}: to {address:?} is not an email address: {e}")
                    })?;
                }
            }
        }
    }
    Ok(())
}

fn validate_public_url(url: &str) -> anyhow::Result<()> {
    let uri: axum::http::Uri = url
        .parse()
        .with_context(|| format!("public_url {url:?} is not a valid URL"))?;
    anyhow::ensure!(
        matches!(uri.scheme_str(), Some("http" | "https")) && uri.authority().is_some(),
        "public_url {url:?} must look like https://lodestore.example.com"
    );
    anyhow::ensure!(
        matches!(uri.path(), "" | "/") && uri.query().is_none() && !url.contains('#'),
        "public_url {url:?} must not contain a path, query or fragment"
    );
    let host_ok = uri.authority().is_some_and(|a| {
        a.as_str()
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '-' | '_' | '[' | ']'))
    });
    anyhow::ensure!(
        host_ok && !url.contains('\''),
        "public_url {url:?} must have a plain host name or address and no quotes"
    );
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct Cidr {
    addr: IpAddr,
    bits: u8,
}

impl Cidr {
    fn parse(text: &str) -> anyhow::Result<Cidr> {
        let (addr, bits) = match text.split_once('/') {
            Some((addr, bits)) => (addr, Some(bits)),
            None => (text, None),
        };
        let addr: IpAddr = addr.trim().parse().with_context(|| {
            format!("trusted_proxies entry {text:?} is not an IP address or CIDR")
        })?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let bits = match bits {
            Some(bits) => bits
                .parse::<u8>()
                .ok()
                .filter(|b| *b <= max)
                .with_context(|| {
                    format!("trusted_proxies entry {text:?} has a bad prefix length")
                })?,
            None => max,
        };
        Ok(Cidr { addr, bits })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        fn prefix_eq(a: &[u8], b: &[u8], bits: u8) -> bool {
            let (full, rest) = (usize::from(bits / 8), bits % 8);
            a[..full] == b[..full] && (rest == 0 || (a[full] ^ b[full]) >> (8 - rest) == 0)
        }
        match (self.addr.to_canonical(), ip.to_canonical()) {
            (IpAddr::V4(a), IpAddr::V4(b)) => prefix_eq(&a.octets(), &b.octets(), self.bits),
            (IpAddr::V6(a), IpAddr::V6(b)) => prefix_eq(&a.octets(), &b.octets(), self.bits),
            _ => false,
        }
    }
}

pub fn parse_proxies(entries: &[String]) -> anyhow::Result<Vec<Cidr>> {
    entries.iter().map(|e| Cidr::parse(e)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_matching() {
        let nets = parse_proxies(&["10.0.0.0/8".into(), "::1".into(), "fd00::/16".into()]).unwrap();
        let hit = |ip: &str| nets.iter().any(|n| n.contains(ip.parse().unwrap()));
        assert!(hit("10.9.8.7") && hit("::1") && hit("fd00::5") && hit("::ffff:10.1.1.1"));
        assert!(!hit("11.0.0.1") && !hit("::2") && !hit("fe80::1"));
        assert!(parse_proxies(&["10.0.0.0/33".into()]).is_err());
        assert!(parse_proxies(&["nope".into()]).is_err());
    }

    #[test]
    fn public_url_rules() {
        for ok in [
            "http://host:8080",
            "https://lodestore.example.com",
            "https://h/",
            "http://[::1]:8080",
            "http://192.168.0.5:8080",
            "http://lode_server:8080",
        ] {
            validate_public_url(ok).unwrap();
        }
        for bad in [
            "lodestore.example.com",
            "ftp://h",
            "https://h/sub",
            "https://h?x=1",
            "https://",
            "https://h'x",
            "https://h$(id)",
            "https://h`id`",
            "https://h;id",
            "https://u:p@h",
            "https://h:80'",
        ] {
            assert!(validate_public_url(bad).is_err(), "{bad}");
        }
    }

    fn load(yaml: &str, files: &[(&str, &str)]) -> anyhow::Result<Config> {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in files {
            std::fs::write(dir.path().join(name), body).unwrap();
        }
        let path = dir.path().join("server.yaml");
        std::fs::write(&path, yaml.replace("{dir}", dir.path().to_str().unwrap())).unwrap();
        Config::from_args(ConfigArgs {
            config: Some(path),
            ..ConfigArgs::default()
        })
    }

    fn chain(result: anyhow::Result<Config>) -> String {
        format!("{:#}", result.unwrap_err())
    }

    #[test]
    fn metrics_listen_from_yaml_and_env() {
        assert_eq!(load("", &[]).unwrap().metrics_listen, None);
        let yaml = load("metrics_listen: \"0.0.0.0:9464\"", &[]).unwrap();
        assert_eq!(yaml.metrics_listen, Some("0.0.0.0:9464".parse().unwrap()));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.yaml");
        std::fs::write(&path, "metrics_listen: \"0.0.0.0:9464\"").unwrap();
        let env = Config::from_args(ConfigArgs {
            config: Some(path),
            metrics_listen: Some("127.0.0.1:9100".parse().unwrap()),
            ..ConfigArgs::default()
        })
        .unwrap();
        assert_eq!(env.metrics_listen, Some("127.0.0.1:9100".parse().unwrap()));

        let err = chain(load("metrics_listen: not-an-address", &[]));
        assert!(
            err.contains("metrics_listen") || err.contains("invalid socket address"),
            "{err}"
        );
    }

    #[test]
    fn demo_mode_defaults_off_and_follows_args_over_file() {
        assert!(!load("", &[]).unwrap().demo_mode);
        assert!(load("demo_mode: true", &[]).unwrap().demo_mode);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.yaml");
        std::fs::write(&path, "demo_mode: true").unwrap();
        let off = Config::from_args(ConfigArgs {
            config: Some(path),
            demo_mode: Some(false),
            ..ConfigArgs::default()
        })
        .unwrap();
        assert!(!off.demo_mode);
        let on = Config::from_args(ConfigArgs {
            demo_mode: Some(true),
            ..ConfigArgs::default()
        })
        .unwrap();
        assert!(on.demo_mode);
    }

    #[test]
    fn find_section_parses_and_rejects_zero() {
        let defaults = load("", &[]).unwrap().find;
        assert_eq!(defaults.max_matches, 1000);
        assert_eq!(defaults.timeout, Duration::from_secs(600));
        assert_eq!(defaults.cache_max_bytes, 1 << 30);

        let c = load(
            "find:\n  max_matches: 50\n  timeout_secs: 30\n  cache_max_mb: 64",
            &[],
        )
        .unwrap();
        assert_eq!(c.find.max_matches, 50);
        assert_eq!(c.find.timeout, Duration::from_secs(30));
        assert_eq!(c.find.cache_max_bytes, 64 << 20);

        for key in ["max_matches", "timeout_secs", "cache_max_mb"] {
            for value in ["0", "-1"] {
                let err = chain(load(&format!("find:\n  {key}: {value}"), &[]));
                assert!(err.contains(key), "{key}: {err}");
            }
        }
    }

    #[test]
    fn agent_updates_section_parses() {
        let defaults = load("", &[]).unwrap().agent_updates;
        assert_eq!(
            (defaults.canary, defaults.wave, defaults.pause_after),
            (2, 10, 1)
        );
        assert_eq!(defaults.health_timeout, Duration::from_secs(150));

        let c = load(
            "agent_updates:\n  canary: 3\n  wave: 5\n  pause_after: 1\n  health_timeout_secs: 120",
            &[],
        )
        .unwrap()
        .agent_updates;
        assert_eq!((c.canary, c.wave, c.pause_after), (3, 5, 1));
        assert_eq!(c.health_timeout, Duration::from_secs(120));
    }

    #[test]
    fn agent_updates_health_timeout_outlasts_the_agent_timer() {
        let err = chain(load("agent_updates:\n  health_timeout_secs: 119", &[]));
        assert!(err.contains("health_timeout_secs"), "{err}");
        assert!(err.contains("120"), "{err}");
    }

    #[test]
    fn agent_updates_section_rejects_zero() {
        for key in ["canary", "wave", "pause_after", "health_timeout_secs"] {
            for value in ["0", "-1"] {
                let err = chain(load(&format!("agent_updates:\n  {key}: {value}"), &[]));
                assert!(err.contains(key), "{key}: {err}");
            }
        }
    }

    #[test]
    fn notifications_yaml_reads_secret_files() {
        let c = load(
            "notifications:
  channels:
    - {name: ops-hook, type: webhook, url: 'https://hooks.example.com/x', secret_file: '{dir}/hook'}
    - {name: ops-tg, type: telegram, bot_token_file: '{dir}/tg', chat_id: '-100'}",
            &[("hook", "hook-secret\n"), ("tg", "123:abc\n")],
        )
        .unwrap();
        let [hook, tg] = c.notifications.channels.as_slice() else {
            panic!("expected two channels");
        };
        assert_eq!((hook.name.as_str(), hook.enabled), ("ops-hook", true));
        assert_eq!(hook.alerts, ALL_ALERT_TYPES);
        assert_eq!(tg.alerts, ALL_ALERT_TYPES);
        assert!(hook.alerts.contains(&AlertType::SnapshotQuarantined));
        assert!(hook.alerts.contains(&AlertType::RepositoryUnavailable));
        let ChannelSpec::Webhook { url, secret } = &hook.kind else {
            panic!("webhook expected");
        };
        assert_eq!(url, "https://hooks.example.com/x");
        assert_eq!(secret.as_ref().unwrap().expose(), "hook-secret");
        let ChannelSpec::Telegram { bot_token, chat_id } = &tg.kind else {
            panic!("telegram expected");
        };
        assert_eq!((bot_token.expose(), chat_id.as_str()), ("123:abc", "-100"));
    }

    #[test]
    fn notifications_private_targets_off_by_default() {
        assert!(!load("", &[]).unwrap().notifications.allow_private_targets);
        let c = load("notifications:\n  allow_private_targets: true", &[]).unwrap();
        assert!(c.notifications.allow_private_targets);
    }

    #[test]
    fn notifications_inline_secret_rejected() {
        let err = chain(load(
            "notifications:
  channels:
    - {name: ops-tg, type: telegram, bot_token: '123:abc', chat_id: '-100'}",
            &[],
        ));
        assert!(err.contains("unknown field `bot_token`"), "{err}");
    }

    #[test]
    fn notifications_duplicate_names_rejected() {
        let err = chain(load(
            "notifications:
  channels:
    - {name: ops-hook, type: webhook, url: 'https://a.example/x'}
    - {name: ops-hook, type: webhook, url: 'https://b.example/x'}",
            &[],
        ));
        assert!(
            err.contains("notification channel name ops-hook is used twice"),
            "{err}"
        );
        let err = chain(load(
            "notifications:
  channels:
    - {name: '', type: webhook, url: 'https://a.example/x'}",
            &[],
        ));
        assert!(err.contains("name must not be empty"), "{err}");
        let err = chain(load(
            "notifications:
  channels:
    - {name: quiet, type: webhook, url: 'https://a.example/x', alerts: []}",
            &[],
        ));
        assert!(err.contains("quiet: alerts must not be empty"), "{err}");
    }

    #[test]
    fn notifications_bad_webhook_url_rejected() {
        let err = chain(load(
            "notifications:
  channels:
    - {name: ops-hook, type: webhook, url: 'ftp://example.com/x'}",
            &[],
        ));
        assert!(
            err.contains("ops-hook: url must start with http:// or https://"),
            "{err}"
        );
        let err = chain(load(
            "notifications:
  channels:
    - {name: ops-hook, type: webhook, url: 'https://a.example/x', secret_file: '{dir}/missing'}",
            &[],
        ));
        assert!(
            err.contains("secret_file") && err.contains("missing"),
            "{err}"
        );
    }

    #[test]
    fn notifications_slack_requires_https() {
        let yaml = "notifications:
  channels:
    - {name: ops-slack, type: slack, webhook_url_file: '{dir}/slack'}";
        let err = chain(load(
            yaml,
            &[("slack", "http://hooks.slack.com/services/T/B/X\n")],
        ));
        assert!(
            err.contains("ops-slack: webhook URL must start with https://"),
            "{err}"
        );
        let c = load(
            yaml,
            &[("slack", "https://hooks.slack.com/services/T/B/X\n")],
        )
        .unwrap();
        let ChannelSpec::Slack { webhook_url } = &c.notifications.channels[0].kind else {
            panic!("slack expected");
        };
        assert_eq!(
            webhook_url.expose(),
            "https://hooks.slack.com/services/T/B/X"
        );
    }

    #[test]
    fn notifications_email_defaults() {
        let c = load(
            "notifications:
  channels:
    - {name: mail, type: email, smtp_host: smtp.example.com, from: 'lodestore@example.com'}
    - {name: mail-tls, type: email, smtp_host: smtp.example.com, port: 465, security: tls, username: u, password_file: '{dir}/pw', from: 'lodestore@example.com', to: ['ops@example.com']}",
            &[("pw", "smtp-pass\n")],
        )
        .unwrap();
        let [mail, tls] = c.notifications.channels.as_slice() else {
            panic!("expected two channels");
        };
        let ChannelSpec::Email {
            port,
            security,
            password,
            to,
            ..
        } = &mail.kind
        else {
            panic!("email expected");
        };
        assert_eq!(
            (*port, *security, password.is_none()),
            (587, SmtpSecurity::Starttls, true)
        );
        assert!(to.is_empty());
        let ChannelSpec::Email {
            port,
            security,
            password,
            username,
            to,
            ..
        } = &tls.kind
        else {
            panic!("email expected");
        };
        assert_eq!((*port, *security), (465, SmtpSecurity::Tls));
        assert_eq!(password.as_ref().unwrap().expose(), "smtp-pass");
        assert_eq!((username.as_deref(), to.len()), (Some("u"), 1));
        assert!(!format!("{c:?}").contains("smtp-pass"));
    }

    #[test]
    fn notifications_email_address_rejected() {
        let err = chain(load(
            "notifications:
  channels:
    - {name: mail, type: email, smtp_host: smtp.example.com, from: nope}",
            &[],
        ));
        assert!(err.contains("mail: from"), "{err}");
        let err = chain(load(
            "notifications:
  channels:
    - {name: mail, type: email, smtp_host: smtp.example.com, from: 'a@example.com', to: ['ops@']}",
            &[],
        ));
        assert!(err.contains("mail: to"), "{err}");
    }

    #[test]
    fn notifications_plain_smtp_rejected() {
        let err = chain(load(
            "notifications:
  channels:
    - {name: mail, type: email, smtp_host: smtp.example.com, from: 'a@example.com', security: none}",
            &[],
        ));
        assert!(err.contains("unknown variant `none`"), "{err}");
    }

    #[test]
    fn notifications_debug_redacts_secrets() {
        let c = load(
            "notifications:
  channels:
    - {name: ops-hook, type: webhook, url: 'https://a.example/x', secret_file: '{dir}/hook'}
    - {name: ops-tg, type: telegram, bot_token_file: '{dir}/tg', chat_id: '-100'}",
            &[("hook", "hook-secret"), ("tg", "123:abc")],
        )
        .unwrap();
        let debug = format!("{c:?}");
        assert!(
            !debug.contains("hook-secret") && !debug.contains("123:abc"),
            "{debug}"
        );
    }
}
