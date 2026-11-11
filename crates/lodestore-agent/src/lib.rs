//! Lodestore host agent: registers with the server over mTLS and keeps a
//! session open. The agent only ever dials out.

pub mod config;
pub mod connect;
pub mod enroll;
pub mod facts;
pub mod hooks;
pub mod identity;
pub mod jobs;
pub mod session;
pub mod shared_channel;
pub mod socket;
pub mod tls;
pub mod token;
pub mod update;

pub use enroll::{EnrollArgs, Enrolled, enroll, enroll_with_previous};
pub use identity::Paths;
pub use session::{RunArgs, run, run_with};

pub const EXIT_ALREADY_ENROLLED: u8 = 3;
pub const EXIT_REVOKED: u8 = 77;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("already registered as {0}")]
    AlreadyEnrolled(String),
    #[error("invalid join token: {0}")]
    InvalidToken(String),
    #[error("the server's certificate authority does not match the fingerprint in the join token")]
    TrustAnchorMismatch,
    #[error(
        "the join token carries no CA fingerprint: the server uses an external certificate, which this agent cannot verify yet"
    )]
    MissingPin,
    #[error("this agent was revoked by the server")]
    Revoked,
    #[error(
        "the server does not accept this agent's certificate; re-enroll the host with a new join token"
    )]
    CertificateStale,
    #[error("not registered: {0}")]
    NotEnrolled(String),
    #[error("the server refused the request: {0}")]
    Rejected(String),
    #[error("cannot reach the server: {0}")]
    Connect(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl AgentError {
    pub fn exit_code(&self) -> u8 {
        match self {
            AgentError::AlreadyEnrolled(_) => EXIT_ALREADY_ENROLLED,
            AgentError::Revoked => EXIT_REVOKED,
            _ => 1,
        }
    }

    pub(crate) fn from_status(status: &tonic::Status) -> AgentError {
        let marker = status
            .metadata()
            .get(lodestore_proto::ERROR_METADATA_KEY)
            .and_then(|v| v.to_str().ok());
        if marker == Some(lodestore_proto::ERROR_REVOKED) {
            AgentError::Revoked
        } else if marker == Some(lodestore_proto::ERROR_CERT_STALE) {
            AgentError::CertificateStale
        } else {
            AgentError::Rejected(status.message().to_string())
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct NetOptions {
    pub https_proxy: Option<String>,
    pub http_proxy: Option<String>,
    pub no_proxy: Option<String>,
    pub ca_bundle: Option<std::path::PathBuf>,
}

impl NetOptions {
    pub fn from_env() -> Self {
        let first = |names: [&str; 2]| names.iter().find_map(|n| std::env::var(n).ok());
        NetOptions {
            https_proxy: first(["HTTPS_PROXY", "https_proxy"]),
            http_proxy: first(["HTTP_PROXY", "http_proxy"]),
            no_proxy: first(["NO_PROXY", "no_proxy"]),
            ca_bundle: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_status_maps_certificate_stale() {
        let mut status = tonic::Status::permission_denied("stale");
        status.metadata_mut().insert(
            lodestore_proto::ERROR_METADATA_KEY,
            lodestore_proto::ERROR_CERT_STALE.parse().unwrap(),
        );
        let error = AgentError::from_status(&status);
        assert!(matches!(error, AgentError::CertificateStale));
        assert_eq!(error.exit_code(), 1);
    }
}
