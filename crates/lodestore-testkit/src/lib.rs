//! Test harness shared by the integration tests: a pinned MinIO started as a
//! process, a proxy that injects S3 failures and an HTTP CONNECT proxy.

pub mod connect_proxy;
pub mod fault_proxy;
pub mod minio;
pub mod minisign;
