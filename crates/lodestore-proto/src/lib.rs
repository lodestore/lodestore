//! Lodestore agent control protocol.

#[allow(clippy::all, clippy::pedantic)]
pub mod agent {
    pub mod v1 {
        tonic::include_proto!("lodestore.agent.v1");
    }
}

pub const PROTOCOL_VERSION: u32 = 1;

pub const ERROR_METADATA_KEY: &str = "lodestore-error";
pub const ERROR_REVOKED: &str = "revoked";
pub const ERROR_TOKEN_INVALID: &str = "token_invalid";
pub const ERROR_VERSION_UNSUPPORTED: &str = "version_unsupported";
