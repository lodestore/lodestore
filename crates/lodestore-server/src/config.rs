use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Args;
use serde::Deserialize;

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
    #[serde(skip)]
    pub grpc_public_address_defaulted: bool,
}

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
            grpc_public_address_defaulted: true,
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
        Ok(())
    }
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
        ] {
            validate_public_url(ok).unwrap();
        }
        for bad in [
            "lodestore.example.com",
            "ftp://h",
            "https://h/sub",
            "https://h?x=1",
            "https://",
        ] {
            assert!(validate_public_url(bad).is_err(), "{bad}");
        }
    }
}
