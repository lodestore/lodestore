use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub mod apply;
pub mod control;
pub mod fetch;
pub mod guard;
pub mod minisign;
pub mod switch;
pub mod trust;

use control::Flight;
use switch::Switcher;

pub const ROLLBACK_AFTER: Duration = Duration::from_secs(120);
pub const MAX_UPDATE_BYTES: u64 = 256 * 1024 * 1024;
pub const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct UpdateRuntime {
    pub exe: PathBuf,
    pub version: String,
    pub switcher: Arc<dyn Switcher>,
    pub enabled: bool,
    pub flight: Arc<Flight>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    Switched,
    Rejected(String),
    Busy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub update_id: String,
    pub target_version: String,
    pub previous_version: String,
    pub deadline_ms: u64,
    pub guard_unit: String,
}

impl UpdateRuntime {
    /// Updates are on only when systemd runs this process and the binary's
    /// directory is writable; the path is taken before any replacement.
    pub fn detect() -> UpdateRuntime {
        let exe = std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .unwrap_or_default();
        let under_systemd = std::env::var_os("INVOCATION_ID").is_some()
            && Path::new("/run/systemd/system").is_dir();
        let writable = exe
            .parent()
            .is_some_and(|dir| rustix::fs::access(dir, rustix::fs::Access::WRITE_OK).is_ok());
        UpdateRuntime {
            enabled: under_systemd && writable,
            exe,
            version: env!("CARGO_PKG_VERSION").to_string(),
            switcher: Arc::new(switch::Systemd),
            flight: Arc::default(),
        }
    }
}
