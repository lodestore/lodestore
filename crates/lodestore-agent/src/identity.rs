use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::AgentError;

#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Paths {
            config_dir: PathBuf::from("/etc/lodestore"),
            state_dir: PathBuf::from("/var/lib/lodestore"),
        }
    }
}

impl Paths {
    pub fn agent_json(&self) -> PathBuf {
        self.config_dir.join("agent.json")
    }
    pub fn agent_yaml(&self) -> PathBuf {
        self.config_dir.join("agent.yaml")
    }
    pub fn hooks_dir(&self) -> PathBuf {
        self.config_dir.join("hooks.d")
    }
    pub fn trusted_keys(&self) -> PathBuf {
        self.config_dir.join("trusted-keys")
    }
    pub fn identity_json(&self) -> PathBuf {
        self.state_dir.join("identity.json")
    }
    pub fn key(&self) -> PathBuf {
        self.state_dir.join("agent.key")
    }
    pub fn cert(&self) -> PathBuf {
        self.state_dir.join("agent.crt")
    }
    pub fn key_new(&self) -> PathBuf {
        self.state_dir.join("agent.key.new")
    }
    pub fn cert_new(&self) -> PathBuf {
        self.state_dir.join("agent.crt.new")
    }
    pub fn ca(&self) -> PathBuf {
        self.state_dir.join("ca.pem")
    }
    pub fn writer_keys(&self) -> PathBuf {
        self.state_dir.join("writer-keys")
    }
    pub fn s3_credentials(&self) -> PathBuf {
        self.state_dir.join("s3-credentials.json")
    }
    pub fn repository_json(&self) -> PathBuf {
        self.state_dir.join("repository.json")
    }
    pub fn jobs_dir(&self) -> PathBuf {
        self.state_dir.join("jobs")
    }
    pub fn cache_dir(&self) -> PathBuf {
        self.state_dir.join("cache")
    }
    pub fn update_dir(&self) -> PathBuf {
        self.state_dir.join("update")
    }
}

pub fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

pub fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("path has no parent directory"))?;
    ensure_dir(dir)?;
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("path has no file name"))?
        .to_string_lossy();
    let tmp = dir.join(format!(".{name}.tmp"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    sync_dir(dir)
}

pub fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

/// Replaces the agent key and certificate so that a crash at any point leaves
/// a usable pair behind: both new files are complete before either name is
/// switched, and `load_pair` finds the matching combination afterwards.
pub fn replace_pair(paths: &Paths, key_pem: &[u8], cert_pem: &[u8]) -> std::io::Result<()> {
    write_private_file(&paths.key_new(), key_pem)?;
    write_private_file(&paths.cert_new(), cert_pem)?;
    std::fs::rename(paths.cert_new(), paths.cert())?;
    sync_dir(&paths.state_dir)?;
    std::fs::rename(paths.key_new(), paths.key())?;
    sync_dir(&paths.state_dir)
}

/// The newest key and certificate that belong together, finishing an
/// interrupted `replace_pair`.
pub fn load_pair(
    paths: &Paths,
    matches: impl Fn(&str, &str) -> bool,
) -> Result<(String, zeroize::Zeroizing<String>), AgentError> {
    let read = |path: PathBuf| std::fs::read_to_string(path).ok();
    for key_path in [paths.key_new(), paths.key()] {
        let Some(key) = read(key_path.clone()).map(zeroize::Zeroizing::new) else {
            continue;
        };
        for cert_path in [paths.cert_new(), paths.cert()] {
            let Some(cert) = read(cert_path.clone()) else {
                continue;
            };
            if !matches(&cert, &key) {
                continue;
            }
            if key_path == paths.key_new() || cert_path == paths.cert_new() {
                tracing::warn!("finishing an interrupted certificate renewal");
                if cert_path == paths.cert_new() {
                    std::fs::rename(paths.cert_new(), paths.cert())?;
                }
                if key_path == paths.key_new() {
                    std::fs::rename(paths.key_new(), paths.key())?;
                }
                sync_dir(&paths.state_dir)?;
            }
            return Ok((cert, key));
        }
    }
    Err(AgentError::Other(anyhow::anyhow!(
        "no agent key and certificate in {} belong together; re-enroll with install.sh --force",
        paths.state_dir.display()
    )))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustMode {
    Pinned,
    System,
    Bundle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trust {
    pub mode: TrustMode,
    pub ca_sha256: Option<String>,
    pub ca_bundle: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub grpc_address: String,
    pub server_name: String,
    pub trust: Trust,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub agent_id: String,
    pub repository_id: String,
    pub issued_at: i64,
    pub expires_at: i64,
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, AgentError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).map_err(|e| {
            AgentError::Other(anyhow::anyhow!("{}: {e}", path.display()))
        })?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), AgentError> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| AgentError::Other(e.into()))?;
    Ok(write_private_file(path, &bytes)?)
}

impl AgentConfig {
    pub fn load(paths: &Paths) -> Result<Option<AgentConfig>, AgentError> {
        read_json(&paths.agent_json())
    }

    pub fn save(&self, paths: &Paths) -> Result<(), AgentError> {
        write_json(&paths.agent_json(), self)
    }
}

impl Identity {
    pub fn load(paths: &Paths) -> Result<Option<Identity>, AgentError> {
        read_json(&paths.identity_json())
    }

    pub fn save(&self, paths: &Paths) -> Result<(), AgentError> {
        write_json(&paths.identity_json(), self)
    }
}

pub fn write_json_file<T: Serialize>(path: &Path, value: &T) -> Result<(), AgentError> {
    write_json(path, value)
}

#[cfg(test)]
mod tests {
    use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};

    use super::*;
    use crate::tls::pair_matches;

    fn pair() -> (String, String) {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let cert = CertificateParams::new(vec!["agent".to_string()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        (cert.pem(), key.serialize_pem())
    }

    fn paths(dir: &Path) -> Paths {
        Paths {
            config_dir: dir.join("etc"),
            state_dir: dir.join("var"),
        }
    }

    #[test]
    fn renewal_survives_a_crash_between_the_two_renames() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let (old_cert, old_key) = pair();
        let (new_cert, new_key) = pair();
        write_private_file(&paths.cert(), old_cert.as_bytes()).unwrap();
        write_private_file(&paths.key(), old_key.as_bytes()).unwrap();

        write_private_file(&paths.key_new(), new_key.as_bytes()).unwrap();
        let (cert, key) = load_pair(&paths, pair_matches).unwrap();
        assert_eq!(
            (cert.as_str(), key.as_str()),
            (old_cert.as_str(), old_key.as_str())
        );

        write_private_file(&paths.cert_new(), new_cert.as_bytes()).unwrap();
        std::fs::rename(paths.cert_new(), paths.cert()).unwrap();
        let (cert, key) = load_pair(&paths, pair_matches).unwrap();
        assert_eq!(
            (cert.as_str(), key.as_str()),
            (new_cert.as_str(), new_key.as_str())
        );
        assert!(!paths.key_new().exists());
        assert_eq!(std::fs::read_to_string(paths.key()).unwrap(), new_key);

        let (third_cert, third_key) = pair();
        replace_pair(&paths, third_key.as_bytes(), third_cert.as_bytes()).unwrap();
        let (cert, key) = load_pair(&paths, pair_matches).unwrap();
        assert_eq!(
            (cert.as_str(), key.as_str()),
            (third_cert.as_str(), third_key.as_str())
        );
    }

    #[test]
    fn a_mismatched_pair_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        write_private_file(&paths.cert(), pair().0.as_bytes()).unwrap();
        write_private_file(&paths.key(), pair().1.as_bytes()).unwrap();
        assert!(load_pair(&paths, pair_matches).is_err());
    }
}
