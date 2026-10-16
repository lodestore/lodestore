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
pub struct AgentSettings {
    pub hooks: HookSettings,
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
