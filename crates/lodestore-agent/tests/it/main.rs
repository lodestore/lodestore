#[path = "../../../lodestore-format/tests/it/adv_support/mod.rs"]
mod adv_support;
mod common;

mod adv_guest_config;
mod ct_backup;
mod ct_restore;
mod deadline;
mod distrusted;
mod hooks;
mod jobs;
mod limits;
mod pve_socket;
mod restore;
mod runner;
mod socket;
mod update_apply;
mod update_guard;
mod update_systemd;
mod update_trust;
