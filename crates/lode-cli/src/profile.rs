use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::cli::UsageError;

/// One `[profiles.<name>]` table of `config.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub repository: Option<String>,
    pub password_file: Option<PathBuf>,
    pub password_command: Option<String>,
    pub server: Option<String>,
    pub token: Option<String>,
}

#[derive(Deserialize)]
struct ConfigFile {
    #[serde(default)]
    profiles: BTreeMap<String, Profile>,
}

/// `$XDG_CONFIG_HOME/lode/config.toml`, else `$HOME/.config/lode/config.toml`,
/// on every platform.
pub fn config_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .ok_or_else(|| UsageError("cannot find the config folder: set HOME".into()))?;
    Ok(base.join("lode").join("config.toml"))
}

pub fn load_profile(name: &str) -> Result<Profile> {
    let path = config_path()?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(UsageError(format!(
                "profile {name:?} not found: {} does not exist",
                path.display()
            ))
            .into());
        }
        Err(err) => {
            return Err(err).with_context(|| format!("cannot read {}", path.display()));
        }
    };
    let mut file: ConfigFile =
        toml::from_str(&text).map_err(|err| UsageError(format!("{}: {err}", path.display())))?;
    let profile = file
        .profiles
        .remove(name)
        .ok_or_else(|| UsageError(format!("profile {name:?} not found in {}", path.display())))?;
    if profile.token.is_some() && readable_by_others(&path) {
        eprintln!(
            "warning: {} holds a token and is readable by others. Run: chmod 600 {}",
            path.display(),
            path.display()
        );
    }
    Ok(profile)
}

fn readable_by_others(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o077 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_profiles_table() {
        let file: ConfigFile = toml::from_str(
            "[profiles.home]\nrepository = \"/srv/repo\"\npassword_file = \"/etc/pw\"\n",
        )
        .unwrap();
        let home = &file.profiles["home"];
        assert_eq!(home.repository.as_deref(), Some("/srv/repo"));
        assert_eq!(home.password_file.as_deref(), Some("/etc/pw".as_ref()));
        assert!(home.password_command.is_none());
    }

    #[test]
    fn parses_server_and_token() {
        let file: ConfigFile = toml::from_str(
            "[profiles.ci]\nserver = \"https://lode.example.com\"\ntoken = \"lsk_x\"\n",
        )
        .unwrap();
        let ci = &file.profiles["ci"];
        assert_eq!(ci.server.as_deref(), Some("https://lode.example.com"));
        assert_eq!(ci.token.as_deref(), Some("lsk_x"));
    }

    #[test]
    fn detects_files_open_to_others() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        for (mode, open) in [(0o600, false), (0o400, false), (0o640, true), (0o604, true)] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(readable_by_others(&path), open, "{mode:o}");
        }
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<ConfigFile>("[profiles.p]\nrepo = \"x\"\n").is_err());
    }
}
