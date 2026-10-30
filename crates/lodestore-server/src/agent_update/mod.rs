use semver::Version;

pub mod api;
pub mod fetch;
pub mod manifest;
pub mod rollout;

/// A version that does not parse never counts as having reached the target.
pub(crate) fn reached(version: &str, target: &str) -> bool {
    match (Version::parse(version), Version::parse(target)) {
        (Ok(version), Ok(target)) => version >= target,
        _ => false,
    }
}
