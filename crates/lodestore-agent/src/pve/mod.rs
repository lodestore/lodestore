//! Proxmox VE support: the socket operations the Perl backup provider uses to
//! hand a container backup or restore to the agent.

pub mod socket;

pub const STORAGE_ID: &str = "lodestore";
pub const TOKEN_ENV: &str = "LODESTORE_JOB_TOKEN";
pub const ROOT_UID: u32 = 0;
pub const PROVIDER_PROTOCOL: u32 = 1;
pub const MIN_PROVIDER_PROTOCOL: u32 = 1;
