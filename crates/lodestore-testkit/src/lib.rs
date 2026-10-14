//! Test harness shared by the integration tests: a pinned MinIO started as a
//! process and a proxy that injects S3 failures.

pub mod fault_proxy;
pub mod minio;
