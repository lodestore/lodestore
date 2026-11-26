//! Server tests that need the storage emulators (sshd, Azurite,
//! fake-gcs-server). They run through `scripts/test-emulators.sh`.

#[path = "../it/common/mod.rs"]
mod common;

mod agent_material;
mod agent_options;
mod gc_sftp;
mod host_key_push;
mod sas_vending;
mod tofu;
