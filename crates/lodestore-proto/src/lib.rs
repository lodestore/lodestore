//! Lodestore agent control protocol.
#![forbid(unsafe_code)]

#[allow(clippy::all, clippy::pedantic)]
pub mod agent {
    pub mod v1 {
        tonic::include_proto!("lodestore.agent.v1");
    }
}

pub const PROTOCOL_VERSION: u32 = 2;

pub const ERROR_METADATA_KEY: &str = "lodestore-error";
pub const ERROR_REVOKED: &str = "revoked";
pub const ERROR_CERT_STALE: &str = "certificate_stale";
pub const ERROR_TOKEN_INVALID: &str = "token_invalid";
pub const ERROR_VERSION_UNSUPPORTED: &str = "version_unsupported";

/// A release channel is the non-empty pre-release part of a signed version; build metadata is
/// ignored and nothing configures it. A stable agent takes only stable releases, a pre-release
/// agent takes anything newer.
pub fn accepts_channel(current: &semver::Version, target: &semver::Version) -> bool {
    target.pre.is_empty() || !current.pre.is_empty()
}

#[cfg(test)]
mod tests {
    use super::accepts_channel;
    use semver::Version;

    #[test]
    fn channel_rule_table() {
        let rows = [
            ("0.2.0", "0.3.0", true),
            ("0.2.0", "0.3.0-rc.1", false),
            ("0.3.0-rc.1", "0.3.0", true),
            ("0.3.0-rc.1", "0.3.0-rc.2", true),
            ("0.2.0", "0.2.1-beta.1", false),
            ("0.2.0+build.1", "0.3.0", true),
            ("0.2.0", "0.3.0+build.1", true),
        ];
        for (current, target, want) in rows {
            let current = Version::parse(current).unwrap();
            let target = Version::parse(target).unwrap();
            assert_eq!(
                accepts_channel(&current, &target),
                want,
                "{current} -> {target}"
            );
        }
    }
}
