//! `lode` against the storage emulators (Azurite, fake-gcs-server, sshd).
//! They run through `scripts/test-emulators.sh`.

#[path = "../it/common/mod.rs"]
mod common;

mod backends;
