use std::io::Read;
use std::path::PathBuf;

use semver::Version;

use crate::config::Config;
use crate::install::{ARCHES, checksum};

const MAX_SIGNATURE_BYTES: u64 = 4096;
const COMMENT_PREFIX: &str = "trusted comment: ";
const COMMENT_PRODUCT: &str = "lodestore-agent";

#[derive(Debug, Clone)]
pub struct AgentBinary {
    pub arch: &'static str,
    pub path: PathBuf,
    pub signature_path: PathBuf,
    pub size: u64,
    pub sha256: Option<String>,
    pub version: Option<Version>,
    pub signed: bool,
}

/// Reads the version out of a minisign `trusted comment` line. The server does
/// not verify the signature itself; the agent does, so this only has to be
/// well-formed and agree with the file name.
fn signed_version(text: &str, arch: &str) -> Option<Version> {
    let comment = text
        .lines()
        .find_map(|line| line.strip_prefix(COMMENT_PREFIX))?;
    let mut words = comment.split_whitespace();
    let (product, version, comment_arch) = (words.next()?, words.next()?, words.next()?);
    if words.next().is_some() || product != COMMENT_PRODUCT || comment_arch != arch {
        return None;
    }
    Version::parse(version).ok()
}

pub(crate) fn read_signature(path: PathBuf) -> Option<String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_SIGNATURE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_SIGNATURE_BYTES {
        return None;
    }
    String::from_utf8(bytes).ok()
}

pub async fn binaries(config: &Config) -> Vec<AgentBinary> {
    let mut found = Vec::new();
    for (arch, file) in ARCHES {
        let path = config.agent_dir.join(file);
        let Ok(meta) = tokio::fs::metadata(&path).await else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let signature_path = config.agent_dir.join(format!("{file}.minisig"));
        let version = tokio::task::spawn_blocking({
            let signature_path = signature_path.clone();
            move || read_signature(signature_path)
        })
        .await
        .ok()
        .flatten()
        .and_then(|text| signed_version(&text, arch));
        found.push(AgentBinary {
            arch,
            sha256: checksum(path.clone()).await,
            path,
            signature_path,
            size: meta.len(),
            signed: version.is_some(),
            version,
        });
    }
    found
}

pub fn target_version(binaries: &[AgentBinary]) -> Option<Version> {
    binaries
        .iter()
        .filter(|b| b.signed)
        .filter_map(|b| b.version.clone())
        .max()
}

pub fn binary_for<'a>(
    binaries: &'a [AgentBinary],
    arch: &str,
    version: &Version,
) -> Option<&'a AgentBinary> {
    binaries
        .iter()
        .find(|b| b.signed && b.arch == arch && b.version.as_ref() == Some(version))
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;

    fn sig(version: &str, arch: &str) -> String {
        format!(
            "untrusted comment: x\nRUQ=\ntrusted comment: lodestore-agent {version} {arch}\nAAAA\n"
        )
    }

    fn config(dir: &std::path::Path) -> Config {
        Config {
            agent_dir: dir.to_path_buf(),
            ..Config::default()
        }
    }

    fn put(dir: &std::path::Path, arch: &str, signature: Option<&str>) {
        let file = format!("lodestore-agent-linux-{arch}");
        std::fs::write(dir.join(&file), b"binary").unwrap();
        if let Some(signature) = signature {
            std::fs::write(dir.join(format!("{file}.minisig")), signature).unwrap();
        }
    }

    #[tokio::test]
    async fn a_binary_without_a_signature_is_unsigned() {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), "x86_64", None);
        let found = binaries(&config(dir.path())).await;
        assert_eq!(found.len(), 1);
        assert!(!found[0].signed && found[0].version.is_none());
        assert_eq!(target_version(&found), None);
    }

    #[tokio::test]
    async fn a_signature_for_another_arch_does_not_count() {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), "x86_64", Some(&sig("1.2.3", "aarch64")));
        let found = binaries(&config(dir.path())).await;
        assert!(!found[0].signed);
    }

    #[tokio::test]
    async fn a_malformed_signature_is_unsigned_without_panicking() {
        let bad = [
            sig("not-a-version", "x86_64"),
            "trusted comment: lodestore-agent 1.2.3\n".to_string(),
            "trusted comment: lodestore-agent 1.2.3 x86_64 extra\n".to_string(),
            "trusted comment: other-product 1.2.3 x86_64\n".to_string(),
            "RUQ=\nAAAA\n".to_string(),
            format!("{}\n{}", "a".repeat(5000), sig("1.2.3", "x86_64")),
        ];
        for text in bad {
            let dir = tempfile::tempdir().unwrap();
            put(dir.path(), "x86_64", Some(&text));
            let found = binaries(&config(dir.path())).await;
            assert!(!found[0].signed, "{text:?}");
        }
    }

    #[tokio::test]
    async fn the_target_is_the_highest_signed_version() {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), "x86_64", Some(&sig("1.2.3", "x86_64")));
        put(dir.path(), "aarch64", Some(&sig("1.3.0", "aarch64")));
        let found = binaries(&config(dir.path())).await;
        let target = target_version(&found).unwrap();
        assert_eq!(target, Version::new(1, 3, 0));
        assert!(binary_for(&found, "x86_64", &target).is_none());
        let aarch64 = binary_for(&found, "aarch64", &target).unwrap();
        assert_eq!(aarch64.size, 6);
        assert_eq!(
            aarch64.sha256.as_deref(),
            Some(
                data_encoding::HEXLOWER
                    .encode(&Sha256::digest(b"binary"))
                    .as_str()
            )
        );
    }
}
