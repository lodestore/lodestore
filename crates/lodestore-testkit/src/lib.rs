//! Test harness shared by the integration tests: a pinned MinIO, fake-gcs-server
//! and Azurite started as processes, an sshd in a container, a proxy that
//! injects S3 failures and records requests, and an HTTP CONNECT proxy.
#![forbid(unsafe_code)]

pub mod azurite;
pub mod connect_proxy;
pub mod fault_proxy;
pub mod gcs;
pub mod minio;
pub mod minisign;
pub mod qemu;

#[cfg(test)]
pub(crate) fn assert_pinned(key: &str, value: &str) {
    let pins = include_str!("../../../scripts/pins.env");
    let line = format!("{key}={value}");
    assert!(
        pins.lines().any(|l| l == line),
        "scripts/pins.env disagrees with the testkit on {key}"
    );
}
