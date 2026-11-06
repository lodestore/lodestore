use std::path::PathBuf;

use serde::Deserialize;

use crate::Paths;
use crate::hooks::unsafe_reason;

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct HookSettings {
    pub allow_inline: bool,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RestoreSettings {
    pub allowed_restore_roots: Option<Vec<PathBuf>>,
    pub allow_in_place: bool,
}

pub const DEFAULT_RESTORE_DIR: &str = "restore";

impl RestoreSettings {
    pub fn roots(&self, paths: &Paths) -> Vec<PathBuf> {
        let Some(list) = &self.allowed_restore_roots else {
            return vec![paths.state_dir.join(DEFAULT_RESTORE_DIR)];
        };
        list.iter()
            .filter(|root| {
                let ok = root.is_absolute()
                    && root.as_os_str().as_encoded_bytes()[1..]
                        .split(|b| *b == b'/')
                        .all(|part| !part.is_empty() && part != b"." && part != b"..");
                if !ok {
                    tracing::warn!(
                        "ignoring restore root {}: it must be an absolute path without \"..\", \".\" or a trailing \"/\"",
                        root.display()
                    );
                }
                ok
            })
            .cloned()
            .collect()
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AgentSettings {
    pub hooks: HookSettings,
    pub restore: RestoreSettings,
    pub socket: Option<PathBuf>,
}

pub const DEFAULT_SOCKET: &str = "/run/lodestore/agent.sock";

impl AgentSettings {
    pub fn socket_path(&self) -> PathBuf {
        self.socket
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET))
    }

    pub fn load(paths: &Paths) -> AgentSettings {
        let path = paths.agent_yaml();
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return AgentSettings::default(),
            Err(e) => {
                tracing::warn!("ignoring {}: {e}", path.display());
                return AgentSettings::default();
            }
        };
        if !meta.is_file() {
            tracing::warn!("ignoring {}: not a regular file", path.display());
            return AgentSettings::default();
        }
        if let Some(reason) = unsafe_reason(&meta) {
            tracing::warn!("ignoring {}: {reason}", path.display());
            return AgentSettings::default();
        }
        let parsed = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|text| yaml_serde::from_str(&text).map_err(|e| e.to_string()));
        parsed.unwrap_or_else(|e| {
            tracing::warn!("ignoring {}: {e}", path.display());
            AgentSettings::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(tmp: &tempfile::TempDir) -> Paths {
        Paths {
            config_dir: tmp.path().join("etc"),
            state_dir: tmp.path().join("var"),
        }
    }

    fn load(yaml: &str) -> (tempfile::TempDir, Paths, AgentSettings) {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(&tmp);
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        let file = paths.agent_yaml();
        std::fs::write(&file, yaml).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let settings = AgentSettings::load(&paths);
        (tmp, paths, settings)
    }

    #[test]
    fn restore_settings_default_root() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(&tmp);
        let settings = AgentSettings::load(&paths);
        assert_eq!(
            settings.restore.roots(&paths),
            vec![paths.state_dir.join("restore")]
        );
        assert!(!settings.restore.allow_in_place);
    }

    #[test]
    fn restore_settings_explicit_list() {
        let (_tmp, paths, settings) =
            load("restore:\n  allowed_restore_roots: [/srv/r, /mnt/x]\n  allow_in_place: true\n");
        assert_eq!(
            settings.restore.roots(&paths),
            vec![PathBuf::from("/srv/r"), PathBuf::from("/mnt/x")]
        );
        assert!(settings.restore.allow_in_place);

        let (_tmp, paths, settings) = load("restore:\n  allowed_restore_roots: []\n");
        assert!(settings.restore.roots(&paths).is_empty());
    }

    #[test]
    fn restore_settings_drops_relative_and_dotdot() {
        let (_tmp, paths, settings) = load(
            "socket: /run/x.sock\nhooks:\n  allow_inline: true\nrestore:\n  allowed_restore_roots: [/srv/r, rel, /a/../b, /c/, /d/./e]\n",
        );
        assert_eq!(
            settings.restore.roots(&paths),
            vec![PathBuf::from("/srv/r")]
        );
        assert_eq!(settings.socket, Some(PathBuf::from("/run/x.sock")));
        assert!(settings.hooks.allow_inline);
    }
}
